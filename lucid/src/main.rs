//! lucid: run the Android x86-64 build of Roblox on Linux.

mod app;
mod audio;
mod cookies;
mod crash;
mod device;
mod game;
mod input;
mod install;
#[macro_use]
mod java;
mod paths;
mod presence;
mod runtime;
mod startup;

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
