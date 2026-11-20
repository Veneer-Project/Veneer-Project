//! Where lucid keeps things on disk.

use std::path::PathBuf;

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_DATA_HOME` (default `~/.local/share`): where launchers
/// (`applications/`) and icon themes (`icons/`) of the user live.
pub fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".local/share"))
}

/// `$XDG_DATA_HOME/lucid` (default `~/.local/share/lucid`).
pub fn data_dir() -> PathBuf {
    data_home().join("lucid")
}

/// Native libraries extracted from the APK.
pub fn lib_dir() -> PathBuf {
    data_dir().join("app/lib")
}

/// The APK's `assets/` tree.
pub fn assets_dir() -> PathBuf {
    data_dir().join("app/assets")
}

/// The app's private files directory (Android `getFilesDir()`).
pub fn files_dir() -> PathBuf {
    data_dir().join("files")
}

/// The app's cache directory (Android `getCacheDir()`).
pub fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".cache"))
        .join("lucid")
}

/// Where an existing Sober (Flatpak) install keeps its downloaded APKs.
pub fn sober_package_dir() -> PathBuf {
    home().join(".var/app/org.vinegarhq.Sober/data/sober/packages/x86_64/com.roblox.client")
}

/// `$XDG_CONFIG_HOME/lucid` (default `~/.config/lucid`): user configuration.
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"))
        .join("lucid")
}
