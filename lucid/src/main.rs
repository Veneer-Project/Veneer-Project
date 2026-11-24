//! Veneer (codename Lucid): run the Android x86-64 or arm64 build of Roblox on Linux.

mod abi;
mod apk_signature;
mod app;
mod audio;
mod bus;
mod cookies;
mod crash;
mod desktop;
mod device;
mod flags;
mod game;
mod input;
mod install;
#[macro_use]
mod java;
mod keyring;
mod opensles;
mod overlay;
mod paths;
mod permissions;
mod prefs;
mod pulse;
mod presence;
mod runtime;
mod startup;
mod update;
mod voice;
mod webview;
mod webview_helper;

use std::path::PathBuf;
use std::process::ExitCode;

struct Logger {
    /// What goes to the terminal.
    level: log::LevelFilter,
    /// Everything up to debug level also goes here, for bug reports.
    file: Option<std::sync::Mutex<std::fs::File>>,
}

/// A log line's origin without the crate prefix, which people would
/// otherwise see: `veneer::update` (the binary's modules) → `update`,
/// `lucid_linker::stubs` (the codename library crates) → `linker::stubs`,
/// the binary's root module → `veneer`.
fn short_target(target: &str) -> &str {
    if target == "veneer" || target == "lucid" {
        return "veneer";
    }
    target
        .strip_prefix("veneer::")
        .or_else(|| target.strip_prefix("lucid::"))
        .or_else(|| target.strip_prefix("lucid_"))
        .unwrap_or(target)
}

impl log::Log for Logger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= self.level || (self.file.is_some() && m.level() <= log::Level::Debug)
    }
    fn log(&self, r: &log::Record) {
        // Our HTTP client's own chatter is not useful in either place.
        if r.target().starts_with("rustls") || r.target().starts_with("ureq") {
            return;
        }
        if let (Some(f), true) = (&self.file, r.level() <= log::Level::Debug) {
            use std::io::Write;
            let _ = writeln!(f.lock().unwrap(), "{} {}: {}", r.level(), short_target(r.target()), r.args());
        }
        if r.level() <= self.level {
            let tag = match r.level() {
                log::Level::Error => "E",
                log::Level::Warn => "W",
                log::Level::Info => "I",
                log::Level::Debug => "D",
                log::Level::Trace => "T",
            };
            eprintln!("{tag} {}: {}", short_target(r.target()), r.args());
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
    let dir = paths::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(dir.join("latest.log")).ok().map(std::sync::Mutex::new);
    let logger: &'static Logger = Box::leak(Box::new(Logger { level, file }));
    log::set_logger(logger).unwrap();
    log::set_max_level(level.max(log::LevelFilter::Debug));
}

fn usage() -> ExitCode {
    eprintln!(
        "Veneer - run the Android build of Roblox on Linux\n\n\
         usage:\n  \
         veneer install <base.apk> <{split}>  unpack the game from APK files\n  \
         veneer install --from-sober                          reuse the APKs of a Sober install\n  \
         veneer run [--place <id>] [--no-update]              start Roblox (optionally joining a place);\n  \
         \x20                                                    downloads or updates the game first\n  \
         veneer update                                        only download / update the game\n  \
         veneer probe                                         load the engine without a window (debug)\n  \
         veneer desktop-entry [--remove]                      add (or remove) Veneer with its icon to the\n  \
         \x20                                                    app menu, pointing at this binary\n\n\
         environment:\n  \
         LUCID_LOG=error|warn|info|debug|trace   log verbosity\n  \
         LUCID_DISCORD=0                         turn Discord Rich Presence off",
        split = crate::abi::split_apk(),
    );
    ExitCode::from(2)
}

fn cmd_install(args: &[String]) -> ExitCode {
    let apks: Vec<PathBuf> = if args.iter().any(|a| a == "--from-sober") {
        let found = install::sober_apks();
        if found.is_empty() {
            eprintln!(
                "veneer: no APKs found in {}",
                paths::sober_package_dir().display()
            );
            return ExitCode::FAILURE;
        }
        found
    } else if args.is_empty() {
        return usage();
    } else {
        args.iter().map(PathBuf::from).collect()
    };
    match install::install(&apks) {
        Ok(s) => {
            println!(
                "installed Roblox {} ({} libraries, {} assets) into {}",
                s.version.as_deref().unwrap_or("(unknown version)"),
                s.libs,
                s.assets,
                paths::data_dir().display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("veneer: install failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_desktop_entry(args: &[String]) -> ExitCode {
    if args.iter().any(|a| a == "--remove") {
        return match desktop::remove() {
            Ok(n) => {
                println!("removed the launcher and icons ({n} files)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("veneer: {e}");
                ExitCode::FAILURE
            }
        };
    }
    match desktop::install() {
        Ok((launcher, exe)) => {
            println!(
                "installed {} and the icons; it starts {} run\n\
                 (run this again if the binary moves, e.g. switching between debug and release builds)",
                launcher.display(),
                exe.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("veneer: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Keep other programs of the same user out of this process: once it is not
/// "dumpable", the kernel lets only root attach to it (ptrace) or read and
/// write its memory (`/proc/<pid>/mem`, `process_vm_readv` / `writev`).
/// First thing in `main`, so the embedded browser helper (which holds the
/// login while a page is open) is covered too.
fn protect_process() {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        eprintln!("veneer: could not protect the process: {}", std::io::Error::last_os_error());
    }
}

/// The kernel's `uname` version string; under FreeBSD's Linux compatibility
/// layer (the "Linuxulator") it is FreeBSD's own, e.g. "FreeBSD 14.2-RELEASE ...",
/// while the release field claims a Linux version.
fn kernel_version() -> String {
    let mut u: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut u) } != 0 {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(u.version.as_ptr()) }.to_string_lossy().into_owned()
}

/// Report the host, and on FreeBSD check what the Linux compatibility layer
/// must provide: the guest's libc (glibc) and the crash report need
/// `/proc/self/maps`, which only exists when linprocfs is mounted.
fn check_host() {
    let version = kernel_version();
    if !version.starts_with("FreeBSD") {
        return;
    }
    log::info!("running on {} through its Linux compatibility layer (experimental)", version.trim());
    if !std::path::Path::new("/proc/self/maps").exists() {
        log::warn!(
            "/proc is not mounted in the Linux compatibility layer; as root run \
             `sysrc linux_mounts_enable=YES && service linux restart` \
             (see packaging/freebsd/setup.sh)"
        );
    }
}

fn main() -> ExitCode {
    protect_process();
    // The embedded browser runs as a child process of ourselves.
    if std::env::args().nth(1).as_deref() == Some("webview") {
        return ExitCode::from(webview_helper::run() as u8);
    }
    // Before the log file creates the new data folder.
    paths::migrate();
    init_logging();
    crash::install();
    check_host();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => cmd_install(&args[1..]),
        Some("desktop-entry") => cmd_desktop_entry(&args[1..]),
        Some("update") => match update::ensure_current() {
            Ok(()) => {
                println!(
                    "Roblox {} is installed.",
                    install::installed_version().unwrap_or_default()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("veneer: {e}");
                ExitCode::FAILURE
            }
        },
        Some("run") | Some("probe") => {
            // Cold start: install the game if missing, update it if stale.
            let skip = args.iter().any(|a| a == "--no-update")
                || std::env::var_os("LUCID_NO_UPDATE").is_some();
            if skip {
                if install::installed_version().is_none() {
                    eprintln!("veneer: the game is not installed; run without --no-update");
                    return ExitCode::FAILURE;
                }
            } else if let Err(e) = update::ensure_current() {
                eprintln!("veneer: {e}");
                return ExitCode::FAILURE;
            }
            // `veneer run --place <id>` joins an experience directly.
            let place = args
                .iter()
                .position(|a| a == "--place")
                .and_then(|i| args.get(i + 1))
                .and_then(|p| p.parse::<i64>().ok());
            match runtime::run(args[0] == "probe", place) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("veneer: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => usage(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn short_target_hides_the_codename() {
        use super::short_target;
        assert_eq!(short_target("veneer::update"), "update");
        assert_eq!(short_target("veneer"), "veneer");
        assert_eq!(short_target("lucid::update"), "update");
        assert_eq!(short_target("lucid_linker::stubs"), "linker::stubs");
        assert_eq!(short_target("lucid_jni"), "jni");
        assert_eq!(short_target("lucid"), "veneer");
        assert_eq!(short_target("android"), "android");
    }
}
