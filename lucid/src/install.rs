//! Installing the game: check that the APK(s) are Roblox's, then unpack the
//! native libraries and assets into Veneer's data directory.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};

use crate::paths;

const ABI_PREFIX: &str = "lib/x86_64/";
const ASSET_PREFIX: &str = "assets/";

#[derive(Default)]
pub struct Summary {
    pub libs: usize,
    pub assets: usize,
    pub version: Option<String>,
}

/// Find the app's `versionName` (e.g. `2.745.1142`) in a binary
/// AndroidManifest. The string pool is UTF-16 or UTF-8, so dropping NUL
/// bytes makes it searchable either way without a full AXML parser.
fn version_from_manifest(data: &[u8]) -> Option<String> {
    let text: Vec<u8> = data.iter().copied().filter(|b| *b != 0).collect();
    let mut i = 0;
    while i < text.len() {
        if text[i].is_ascii_digit() {
            let start = i;
            while i < text.len() && (text[i].is_ascii_digit() || text[i] == b'.') {
                i += 1;
            }
            let cand = std::str::from_utf8(&text[start..i]).ok()?;
            let parts: Vec<&str> = cand.split('.').collect();
            if parts.len() == 3
                && parts.iter().all(|p| !p.is_empty())
                && parts[1].len() == 3
                && parts[0].len() <= 2
            {
                return Some(cand.to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

fn extract(apk: &Path, summary: &mut Summary) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(File::open(apk)?).map_err(io::Error::other)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(io::Error::other)?;
        if entry.is_dir() {
            continue;
        }
        // `enclosed_name` rejects absolute paths and `..` components.
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let name = name.to_string_lossy().into_owned();
        if name == "AndroidManifest.xml" {
            let mut data = Vec::new();
            io::Read::read_to_end(&mut entry, &mut data)?;
            if let Some(v) = version_from_manifest(&data) {
                summary.version = Some(v);
            }
            continue;
        }
        let dest: PathBuf = if let Some(lib) = name.strip_prefix(ABI_PREFIX) {
            if lib.contains('/') {
                continue;
            }
            summary.libs += 1;
            paths::lib_dir().join(lib)
        } else if let Some(asset) = name.strip_prefix(ASSET_PREFIX) {
            summary.assets += 1;
            paths::assets_dir().join(asset)
        } else {
            continue;
        };
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&dest)?;
        io::copy(&mut entry, &mut out)?;
    }
    Ok(())
}

/// Check and unpack the given APK files (base + ABI split, in any order).
pub fn install(apks: &[PathBuf]) -> io::Result<Summary> {
    // Every APK must be signed by Roblox, before anything is touched: the
    // game's native code runs with the user's permissions, so a modified APK
    // (cheats, malware) is refused. There is deliberately no way to skip this.
    for apk in apks {
        log::info!("checking the signature of {}", apk.display());
        match crate::apk_signature::verify(apk) {
            Ok(v) => log::info!(
                "{}: signed by Roblox (scheme {}{})",
                apk.display(),
                v.scheme,
                if v.rotated { ", current key, vouched for by the original" } else { "" }
            ),
            Err(e) => {
                return Err(io::Error::other(format!(
                    "{} is not a genuine Roblox APK, so it was not installed ({e}). \
                     Only APKs signed by Roblox can be installed.",
                    apk.display()
                )));
            }
        }
    }
    // Start from a clean app directory so files from an old version
    // cannot linger. User data (files/, cache) is kept.
    let app = paths::data_dir().join("app");
    if app.exists() {
        fs::remove_dir_all(&app)?;
    }
    fs::create_dir_all(paths::lib_dir())?;
    fs::create_dir_all(paths::assets_dir())?;
    let mut summary = Summary::default();
    for apk in apks {
        log::info!("unpacking {}", apk.display());
        extract(apk, &mut summary)?;
    }
    if !paths::lib_dir().join("libroblox.so").exists() {
        return Err(io::Error::other(
            "no lib/x86_64/libroblox.so in the given APKs (is this the x86_64 split?)",
        ));
    }
    match &summary.version {
        Some(v) => fs::write(app.join("version"), v)?,
        None => log::warn!("could not determine the app version from the APK manifest"),
    }
    Ok(summary)
}

/// APKs from an existing Sober install, if there is one.
pub fn sober_apks() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(paths::sober_package_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "apk"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// The installed Roblox version, if the game is installed.
pub fn installed_version() -> Option<String> {
    if !paths::lib_dir().join("libroblox.so").exists() {
        return None;
    }
    fs::read_to_string(paths::data_dir().join("app/version"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
