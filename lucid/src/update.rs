//! Keeping the game installed and current.
//!
//! Roblox's x86-64 Android build is only published on Google Play, which
//! needs a signed-in account. We use the public anonymous-account token
//! service run by the Aurora Store project (the open-source Play client),
//! then talk to Google Play directly and download from Google's CDN.

use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpapi::Gpapi;

use crate::{install, paths};

const PACKAGE: &str = "com.roblox.client";
const DISPENSER: &str = "https://auroraoss.com/api/auth";
/// The dispenser only answers clients identifying as Aurora Store.
const DISPENSER_USER_AGENT: &str = "com.aurora.store-4.7.5-71";
/// The Google Play device profile we present: an x86-64 Chromebook, so
/// Play serves the x86-64 split. (The "Play Games on PC" x86-64 profile is
/// not offered Roblox at all.)
const DEVICE_PROPERTIES: &str = include_str!("../data/play_device.properties");

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// What Google Play currently offers.
pub struct Release {
    pub version: String,
    version_code: i64,
    api: Gpapi,
}

fn device_properties_json() -> String {
    let mut map = serde_json::Map::new();
    for line in DEVICE_PROPERTIES.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), serde_json::Value::String(v.replace("\\:", ":")));
        }
    }
    serde_json::Value::Object(map).to_string()
}

/// Ask the token service for an anonymous Google Play login:
/// `(email, auth token)`. Retries briefly when the service is busy.
fn request_anonymous_login() -> Result<(String, String)> {
    let mut last: Box<dyn Error> = "anonymous token service: no attempt made".into();
    for (attempt, wait) in [0u64, 3, 8].into_iter().enumerate() {
        if wait > 0 {
            std::thread::sleep(Duration::from_secs(wait));
        }
        let resp = ureq::post(DISPENSER)
            .set("User-Agent", DISPENSER_USER_AGENT)
            .set("Content-Type", "application/json")
            .timeout(Duration::from_secs(30))
            .send_string(&device_properties_json());
        match resp {
            Ok(resp) => {
                let v: serde_json::Value = serde_json::from_reader(resp.into_reader())?;
                let field = |k: &str| {
                    v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
                };
                return match (field("email"), field("authToken")) {
                    (Some(email), Some(token)) => Ok((email, token)),
                    _ => Err("anonymous token service returned no login".into()),
                };
            }
            // Rate limited or temporarily down: worth another try.
            Err(ureq::Error::Status(code, _)) if code == 429 || code >= 500 => {
                log::debug!("token service answered {code} (attempt {})", attempt + 1);
                last = format!("anonymous token service is busy (HTTP {code})").into();
            }
            Err(e) => return Err(format!("anonymous token service: {e}").into()),
        }
    }
    Err(last)
}

/// Where the last working anonymous login is kept, so that most starts do
/// not need the token service at all.
fn login_cache() -> PathBuf {
    paths::data_dir().join("play_login.json")
}

fn cached_login() -> Option<(String, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(login_cache()).ok()?).ok()?;
    Some((v.get("email")?.as_str()?.to_string(), v.get("token")?.as_str()?.to_string()))
}

fn save_login(email: &str, token: &str) {
    use std::os::unix::fs::OpenOptionsExt;
    let json = serde_json::json!({ "email": email, "token": token }).to_string();
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(login_cache())
        .and_then(|mut f| {
            // `mode` only applies to a newly created file.
            f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
            f.write_all(json.as_bytes())
        });
    if let Err(e) = written {
        log::debug!("could not cache the Google Play login: {e}");
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

/// Log in to Google Play with the given anonymous account and look up
/// the newest release.
fn query(email: &str, token: &str) -> Result<Release> {
    // gpapi reads custom device profiles from a file.
    let profile = paths::cache_dir().join("play_device.properties");
    fs::create_dir_all(paths::cache_dir())?;
    fs::write(&profile, DEVICE_PROPERTIES)?;
    let profile = profile.to_string_lossy().into_owned();
    let mut api = Gpapi::from_device_properties_file("default", email, profile.as_str());
    api.set_auth_token(token);
    runtime()?.block_on(async {
        if let Err(e) = api.login().await {
            // A fresh anonymous account has to accept Google Play's terms
            // of service once before it can be used.
            if !matches!(e.kind(), gpapi::error::ErrorKind::TermsOfService) {
                return Err(format!("Google Play login: {e}").into());
            }
            api.accept_tos().await.map_err(|e| format!("Google Play terms of service: {e}"))?;
        }
        let details = api
            .details(PACKAGE)
            .await
            .map_err(|e| format!("Google Play details: {e}"))?
            .and_then(|d| d.item)
            .and_then(|i| i.details)
            .and_then(|d| d.app_details)
            .ok_or("Google Play returned no details for Roblox")?;
        let version_code = details.version_code.ok_or("no version code")?;
        let version = details.version_string.ok_or("no version name")?;
        Ok(Release {
            version,
            version_code: version_code as i64,
            api,
        })
    })
}

/// Ask Google Play for the newest release.
///
/// The anonymous login from the last successful check is reused while
/// Google still accepts it; the token service is only contacted when there
/// is none or it has expired. That keeps us well clear of its rate limits.
pub fn latest() -> Result<Release> {
    if let Some((email, token)) = cached_login() {
        match query(&email, &token) {
            Ok(release) => {
                log::debug!("reused the cached Google Play login");
                return Ok(release);
            }
            Err(e) => log::debug!("cached Google Play login no longer works: {e}"),
        }
    }
    let (email, token) = request_anonymous_login()?;
    let release = query(&email, &token)?;
    save_login(&email, &token);
    Ok(release)
}

/// Download one file from Google's CDN, printing progress.
fn fetch(url: &str, dest: &Path, label: &str) -> Result<()> {
    let resp = ureq::get(url).timeout(Duration::from_secs(1800)).call()?;
    let total: u64 = resp.header("Content-Length").and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut reader = resp.into_reader();
    let mut out = File::create(dest)?;
    let mut buf = vec![0u8; 256 * 1024];
    let (mut done, mut shown) = (0u64, u64::MAX);
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        let mb = done / (1 << 20);
        if mb != shown {
            shown = mb;
            if total > 0 {
                eprint!("\r  {label}: {mb} / {} MB", total / (1 << 20));
            } else {
                eprint!("\r  {label}: {mb} MB");
            }
        }
    }
    eprintln!();
    if total > 0 && done != total {
        return Err(format!("{label}: download ended early ({done} of {total} bytes)").into());
    }
    Ok(())
}

/// Download a release's APKs and install them.
pub fn download_and_install(release: &Release) -> Result<()> {
    let (base, splits, _, _) = runtime()?
        .block_on(release.api.get_download_info(PACKAGE, Some(release.version_code)))
        .map_err(|e| format!("Google Play delivery: {e}"))?;
    let base = base.ok_or("Google Play offered no download for this device")?;
    let names: Vec<String> = splits.iter().filter_map(|(n, _)| n.clone()).collect();
    let split = splits
        .into_iter()
        .find_map(|(name, url)| match (name, url) {
            (Some(n), Some(u)) if n.contains("x86_64") => Some(u),
            _ => None,
        })
        .ok_or_else(|| format!("Google Play offered no x86_64 build of Roblox (splits: {names:?})"))?;

    let dir = paths::cache_dir().join("download");
    fs::create_dir_all(&dir)?;
    let files: [(PathBuf, &str, &str); 2] = [
        (dir.join("base.apk"), base.as_str(), "base.apk"),
        (dir.join("split_config.x86_64.apk"), split.as_str(), "split_config.x86_64.apk"),
    ];
    for (path, url, label) in &files {
        fetch(url, path, label)?;
    }
    let apks: Vec<PathBuf> = files.iter().map(|f| f.0.clone()).collect();
    let summary = install::install(&apks)?;
    let _ = fs::remove_dir_all(&dir);
    println!(
        "Installed Roblox {}.",
        summary.version.as_deref().unwrap_or(&release.version)
    );
    Ok(())
}

/// Whether dotted version `a` is strictly newer than `b`.
fn is_newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    parse(a) > parse(b)
}

/// Run `f` while showing `label` followed by an animated `.`, `..`, `...`.
fn with_spinner<T>(label: &str, f: impl FnOnce() -> T) -> T {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // Only animate on a terminal; in a pipe or log file, print one line.
    if unsafe { libc::isatty(2) } == 0 {
        eprintln!("{label}...");
        return f();
    }
    let done = Arc::new(AtomicBool::new(false));
    let (flag, text) = (done.clone(), label.to_string());
    let spinner = std::thread::spawn(move || {
        let mut n = 0;
        while !flag.load(Ordering::Relaxed) {
            n = n % 3 + 1;
            eprint!("\r{text}{:<3}", ".".repeat(n));
            let _ = std::io::stderr().flush();
            std::thread::sleep(Duration::from_millis(400));
        }
        // Erase the line so whatever comes next starts clean.
        eprint!("\r{}\r", " ".repeat(text.len() + 3));
        let _ = std::io::stderr().flush();
    });
    let result = f();
    done.store(true, Ordering::Relaxed);
    let _ = spinner.join();
    result
}

/// The cold-start check: make sure the game is installed and current.
/// Returns an error only if there is nothing usable to run.
pub fn ensure_current() -> Result<()> {
    let installed = install::installed_version();
    if installed.is_none() {
        println!("Roblox is not installed. Checking Google Play...");
    }
    let release = match with_spinner("Checking for newer versions", latest) {
        Ok(r) => r,
        Err(e) => {
            return match &installed {
                Some(v) => {
                    eprintln!(
                        "warning: could not check for a Roblox update ({e}); starting installed version {v}."
                    );
                    Ok(())
                }
                None => Err(format!(
                    "could not download Roblox: {e}\n\
                     You can install it manually with `veneer install <base.apk> <split_config.x86_64.apk>`."
                )
                .into()),
            };
        }
    };
    match &installed {
        // Google rolls releases out gradually, and each anonymous account
        // can be at a different stage: never "update" to an older build.
        Some(v) if !is_newer(&release.version, v) => Ok(()),
        Some(v) => {
            println!("Roblox {} is available (installed: {v}). Downloading the APK...", release.version);
            if let Err(e) = download_and_install(&release) {
                eprintln!("warning: update failed ({e}); starting installed version {v}.");
            }
            Ok(())
        }
        None => {
            println!("Downloading the Roblox {} APK...", release.version);
            download_and_install(&release).map_err(|e| {
                format!(
                    "could not download Roblox: {e}\n\
                     You can install it manually with `veneer install <base.apk> <split_config.x86_64.apk>`."
                )
                .into()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn version_comparison_is_numeric() {
        assert!(is_newer("2.748.1527", "2.747.1486"));
        assert!(!is_newer("2.747.1486", "2.748.1527"));
        assert!(!is_newer("2.748.1527", "2.748.1527"));
        assert!(is_newer("2.1000.1", "2.999.9999"));
    }
}
