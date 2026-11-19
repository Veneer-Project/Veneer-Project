//! Bringing the engine up: system libraries, the guest image, the fake JVM.

use std::error::Error;
use std::ffi::c_void;

use lucid_android::window::NativeWindow;
use lucid_linker::Image;

use crate::{app, paths};

/// Load `libroblox.so` and run its constructors and `JNI_OnLoad`.
fn load_engine() -> Result<&'static Image, Box<dyn Error>> {
    for dir in [paths::files_dir(), paths::cache_dir()] {
        std::fs::create_dir_all(dir)?;
    }
    lucid_bionic::install();
    lucid_android::install();
    lucid_android::asset::set_root(paths::assets_dir());
    if std::env::var_os("LUCID_LENIENT").is_some() {
        lucid_linker::stubs::LENIENT.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let path = paths::lib_dir().join("libroblox.so");
    log::info!("loading {}", path.display());
    let image = lucid_linker::load(&path)?;
    log::info!(
        "{} mapped at {:#x}..{:#x}",
        image.name,
        image.map_start,
        image.map_start + image.map_size
    );
    if !image.unresolved.is_empty() {
        log::warn!(
            "{} unresolved imports: {}",
            image.unresolved.len(),
            image.unresolved.join(" ")
        );
    }

    lucid_jni::vm().set_symbol_resolver(lucid_linker::resolve_global);
    unsafe { image.run_init() };
    log::info!("constructors done");

    let on_load = image.symbol("JNI_OnLoad").ok_or("libroblox.so has no JNI_OnLoad")?;
    let on_load: unsafe extern "C" fn(*mut lucid_jni::JavaVM, *mut c_void) -> i32 =
        unsafe { std::mem::transmute(on_load) };
    let version =
        lucid_jni::with_frame(|| unsafe { on_load(lucid_jni::java_vm(), std::ptr::null_mut()) });
    log::info!("JNI_OnLoad returned {version:#x}");
    Ok(image)
}

struct Engine;

impl app::Frontend for Engine {
    fn started(&mut self, window: &'static NativeWindow) {
        log::info!("window ready: {:?}", window.size());
    }
    fn resized(&mut self, width: i32, height: i32) {
        log::debug!("resized to {width}x{height}");
    }
    fn closing(&mut self) {
        log::info!("window closed");
    }
}

pub fn run(probe: bool) -> Result<(), Box<dyn Error>> {
    load_engine()?;
    if probe {
        return Ok(());
    }
    app::run(Engine)
}
