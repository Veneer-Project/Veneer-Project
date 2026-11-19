//! lucid: run the Android x86-64 build of Roblox on Linux.

use std::path::PathBuf;
use std::process::ExitCode;

struct Logger {
    level: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            let tag = match r.level() {
                log::Level::Error => "E",
                log::Level::Warn => "W",
                log::Level::Info => "I",
                log::Level::Debug => "D",
                log::Level::Trace => "T",
            };
            eprintln!("{tag} {}: {}", r.target(), r.args());
        }
    }
    fn flush(&self) {}
}

fn init_logging() {
    let level = match std::env::var("LUCID_LOG").as_deref() {
        Ok("trace") => log::LevelFilter::Trace,
        Ok("debug") => log::LevelFilter::Debug,
        Ok("warn") => log::LevelFilter::Warn,
        Ok("error") => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };
    let logger: &'static Logger = Box::leak(Box::new(Logger { level }));
    log::set_logger(logger).unwrap();
    log::set_max_level(level);
}

fn main() -> ExitCode {
    init_logging();
    let Some(lib_dir) = std::env::args().nth(1).map(PathBuf::from) else {
        eprintln!("usage: lucid <dir containing libroblox.so>");
        return ExitCode::from(2);
    };

    lucid_bionic::install();
    lucid_android::install();
    if std::env::var_os("LUCID_LENIENT").is_some() {
        lucid_linker::stubs::LENIENT.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let path = lib_dir.join("libroblox.so");
    log::info!("loading {}", path.display());
    let image = match lucid_linker::load(&path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("lucid: cannot load {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
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

    log::info!("running constructors");
    unsafe { image.run_init() };
    log::info!("constructors done");

    let Some(on_load) = image.symbol("JNI_OnLoad") else {
        eprintln!("lucid: no JNI_OnLoad in {}", image.name);
        return ExitCode::FAILURE;
    };
    let on_load: unsafe extern "C" fn(*mut lucid_jni::JavaVM, *mut std::ffi::c_void) -> i32 =
        unsafe { std::mem::transmute(on_load) };
    let version = lucid_jni::with_frame(|| unsafe {
        on_load(lucid_jni::java_vm(), std::ptr::null_mut())
    });
    log::info!("JNI_OnLoad returned {version:#x}");
    ExitCode::SUCCESS
}
