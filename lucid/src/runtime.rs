//! Bringing the engine up: system libraries, the guest image, the fake JVM.

use std::error::Error;
use std::ffi::c_void;

use lucid_android::window::NativeWindow;
use lucid_linker::Image;

use crate::device::Device;
use crate::{app, cookies, input, java, paths, startup};

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

struct Engine {
    device: Device,
    cookies: String,
    started: bool,
    input: input::Input,
}

impl app::Frontend for Engine {
    fn started(&mut self, window: &'static NativeWindow) {
        if let Err(e) = startup::engine_setup(&self.device, &self.cookies) {
            log::error!("engine setup failed: {e}");
            return;
        }
        if let Err(e) = startup::client_settings() {
            log::error!("client settings failed: {e}");
            return;
        }
        startup::start_app(&self.device, window, &self.cookies);
        self.started = true;
        log::info!("app shell started");
    }

    fn resized(&mut self, width: i32, height: i32) {
        log::debug!("resized to {width}x{height}");
        if self.started {
            if let Some(w) = lucid_android::window::window() {
                startup::update_surface(w);
            }
        }
    }

    fn event(&mut self, event: &winit::event::WindowEvent, window: &'static NativeWindow) {
        if self.started {
            self.input.handle(event, window);
        }
    }

    fn closing(&mut self) {
        if self.started {
            cookies::save();
        }
        if self.started && std::env::var_os("LUCID_CLEAN_EXIT").is_some() {
            startup::stop_app();
        }
    }
}

pub fn run(probe: bool) -> Result<(), Box<dyn Error>> {
    let device = Device::detect();
    if device.app_version.is_empty() {
        return Err("unknown app version; run `lucid install` again".into());
    }
    log::info!("Roblox {}", device.app_version);
    java::install(&device);
    input::install();
    load_engine()?;
    startup::application_on_create();
    if probe {
        startup::engine_setup(&device, "")?;
        startup::client_settings()?;
        return Ok(());
    }
    // Keep the saved session in step with what the engine reports. The
    // save runs on its own thread: the notification arrives inside an
    // engine callback.
    java::set_notification_sink(|(kind, _)| match kind.as_str() {
        "DID_LOG_IN" | "DID_SIGN_UP" | "DID_SWITCH_ACCOUNT" => {
            std::thread::spawn(cookies::save);
        }
        "DID_LOG_OUT" => cookies::clear(),
        _ => {}
    });
    app::run(Engine {
        device,
        cookies: cookies::load(),
        started: false,
        input: input::Input::default(),
    })?;
    // Engine threads are still running; leave without unwinding them.
    unsafe { libc::_exit(0) }
}
