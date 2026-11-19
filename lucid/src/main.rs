//! lucid: run the Android x86-64 build of Roblox on Linux.

mod app;
mod audio;
mod cookies;
mod crash;
mod device;
mod flags;
mod game;
mod input;
mod install;
#[macro_use]
mod java;
mod overlay;
mod paths;
mod prefs;
mod presence;
mod runtime;
mod startup;

use std::path::PathBuf;
use std::process::ExitCode;

struct Logger {
    /// What goes to the terminal.
    level: log::LevelFilter,
    /// Everything up to debug level also goes here, for bug reports.
    file: Option<std::sync::Mutex<std::fs::File>>,
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
            let _ = writeln!(f.lock().unwrap(), "{} {}: {}", r.level(), r.target(), r.args());
        }
        if r.level() <= self.level {
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
    let dir = paths::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::File::create(dir.join("latest.log")).ok().map(std::sync::Mutex::new);
    let logger: &'static Logger = Box::leak(Box::new(Logger { level, file }));
    log::set_logger(logger).unwrap();
    log::set_max_level(level.max(log::LevelFilter::Debug));
}

fn usage() -> ExitCode {
    eprintln!(
        "lucid - run the Android build of Roblox on Linux\n\n\
         usage:\n  \
         lucid install <base.apk> <split_config.x86_64.apk>   unpack the game from APK files\n  \
         lucid install --from-sober                           reuse the APKs of a Sober install\n  \
         lucid run [--place <id>]                             start Roblox (optionally joining a place)\n  \
         lucid probe                                          load the engine without a window (debug)\n\n\
         environment:\n  \
         LUCID_LOG=error|warn|info|debug|trace   log verbosity\n  \
         LUCID_DISCORD_CLIENT_ID=<app id>        enable Discord Rich Presence"
    );
    ExitCode::from(2)
}

fn cmd_install(args: &[String]) -> ExitCode {
    let apks: Vec<PathBuf> = if args.iter().any(|a| a == "--from-sober") {
        let found = install::sober_apks();
        if found.is_empty() {
            eprintln!(
                "lucid: no APKs found in {}",
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
            eprintln!("lucid: install failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    init_logging();
    crash::install();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => cmd_install(&args[1..]),
        Some("run") | Some("probe") => {
            if !install::is_installed() {
                eprintln!("lucid: the game is not installed yet; see `lucid install`");
                return ExitCode::FAILURE;
            }
            // `lucid run --place <id>` joins an experience directly.
            let place = args
                .iter()
                .position(|a| a == "--place")
                .and_then(|i| args.get(i + 1))
                .and_then(|p| p.parse::<i64>().ok());
            match runtime::run(args[0] == "probe", place) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("lucid: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => usage(),
    }
}
