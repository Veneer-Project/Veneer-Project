//! Where Veneer keeps things on disk.

use std::path::PathBuf;

/// The folder name under the XDG base directories.
const DIR: &str = "veneer";
/// What the folders were called before the project was named Veneer
/// (its codename); moved to [`DIR`] once by [`migrate`].
const OLD_DIR: &str = "lucid";

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// An XDG base directory: `$<var>` if set to an absolute path, else `~/<default>`.
fn xdg(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(default))
}

/// `$XDG_DATA_HOME` (default `~/.local/share`): where launchers
/// (`applications/`) and icon themes (`icons/`) of the user live.
pub fn data_home() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}

fn cache_home() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}

/// `$XDG_DATA_HOME/veneer` (default `~/.local/share/veneer`).
pub fn data_dir() -> PathBuf {
    data_home().join(DIR)
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

/// The app's cache directory (Android `getCacheDir()`), `$XDG_CACHE_HOME/veneer`.
pub fn cache_dir() -> PathBuf {
    cache_home().join(DIR)
}

/// Where an existing Sober (Flatpak) install keeps its downloaded APKs.
pub fn sober_package_dir() -> PathBuf {
    home()
        .join(".var/app/org.vinegarhq.Sober/data/sober/packages")
        .join(crate::abi::SPLIT)
        .join("com.roblox.client")
}

/// `$XDG_CONFIG_HOME/veneer` (default `~/.config/veneer`): user configuration.
pub fn config_dir() -> PathBuf {
    config_home().join(DIR)
}

/// Move the data, cache and config folders from their codename (`lucid`) to
/// `veneer`, so the installed game and the login carry over. Only when the
/// new folder does not exist yet; a rename, so it is instant. Must run before
/// anything creates the new folders (the log file does).
pub fn migrate() {
    for base in [data_home(), cache_home(), config_home()] {
        let (old, new) = (base.join(OLD_DIR), base.join(DIR));
        if old.is_dir() && !new.exists() {
            match std::fs::rename(&old, &new) {
                Ok(()) => eprintln!("veneer: moved {} to {}", old.display(), new.display()),
                Err(e) => eprintln!("veneer: could not move {} to {}: {e}", old.display(), new.display()),
            }
        }
    }
}
