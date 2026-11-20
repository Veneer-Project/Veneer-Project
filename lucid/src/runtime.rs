//! Bringing the engine up: system libraries, the guest image, the fake JVM.

use std::error::Error;
use std::ffi::c_void;

use lucid_android::window::NativeWindow;
use lucid_linker::Image;

use crate::device::Device;
use crate::{app, audio, cookies, game, input, java, paths, startup};

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
    join_place: Option<i64>,
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
        game::subscribe();
        crate::webview::install();
        input::spawn_test_keys();
        input::spawn_test_clicks();
        crate::presence::start();
        if let Some(place) = self.join_place.take() {
            std::thread::spawn(move || game::launch(game::Launch::place(place)));
        }
        self.started = true;
        log::info!("app shell started");
    }

    fn resized(&mut self, width: i32, height: i32) {
        log::debug!("resized to {width}x{height}");
        if self.started {
            if game::in_game() {
                game::update_surface();
            } else if let Some(w) = lucid_android::window::window() {
                startup::update_surface(w);
            }
        }
    }

    fn event(&mut self, event: &winit::event::WindowEvent, window: &'static NativeWindow) {
        if self.started {
            self.input.handle(event, window);
        }
    }

    fn pointer_motion(&mut self, dx: f64, dy: f64, window: &'static NativeWindow) {
        if self.started {
            self.input.relative_motion(dx, dy, window);
        }
    }

    fn wants_pointer_lock(&mut self) -> bool {
        self.started && self.input.wants_pointer_lock()
    }

    fn closing(&mut self) {
        if !self.started {
            return;
        }
        // Never let a stuck engine keep the process alive after the
        // window is gone.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(5));
            log::warn!("engine did not shut down in time; exiting anyway");
            unsafe { libc::_exit(0) }
        });
        cookies::save();
        startup::shutdown(game::in_game());
        // Give the engine's background writers a moment to finish.
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

pub fn run(probe: bool, join_place: Option<i64>) -> Result<(), Box<dyn Error>> {
    let device = Device::detect();
    if device.app_version.is_empty() {
        return Err("unknown app version; run `lucid install` again".into());
    }
    log::info!("Roblox {}", device.app_version);
    java::install(&device);
    input::install();
    audio::install();
    crate::bus::install();
    crate::prefs::install();
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
    java::set_notification_sink(|(kind, data)| match kind.as_str() {
        _ if crate::webview::handle_notification(&kind, &data) => {}
        "DID_LOG_IN" | "DID_SIGN_UP" | "DID_SWITCH_ACCOUNT" => {
            std::thread::spawn(cookies::save);
        }
        "DID_LOG_OUT" => cookies::clear(),
        _ => {}
    });
    app::run(Engine {
        device,
        cookies: cookies::load(),
        join_place,
        started: false,
        input: input::Input::default(),
    })?;
    // Engine threads are still running; leave without unwinding them.
    unsafe { libc::_exit(0) }
}
