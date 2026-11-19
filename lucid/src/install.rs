//! Installing the game: unpack the native libraries and assets from the
//! Roblox APK(s) into lucid's data directory.

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

/// Unpack the given APK files (base + ABI split, in any order).
pub fn install(apks: &[PathBuf]) -> io::Result<Summary> {
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

pub fn is_installed() -> bool {
    paths::lib_dir().join("libroblox.so").exists()
}
