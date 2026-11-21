//! Persisting the login session. The engine owns the live cookie jar; like
//! the Android app, we read it back after login and replay it at startup.
//!
//! The saved login lives in the desktop's keyring (Secret Service), which
//! keeps it encrypted with the user's login password and out of any file a
//! stealer could search for. Without a keyring it falls back to a file
//! readable only by the user. Rule: if that file exists it is the newest
//! copy (from before the keyring was used, or from a time the keyring
//! failed), so it is moved into the keyring as soon as that works, and only
//! deleted once the keyring gives the same login back.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use lucid_jni::{JObject, from_raw, with_frame};

use crate::device::BASE_URL;
use crate::java::jstr;
use crate::jcall;
use crate::{keyring, paths};

const SETTINGS: &str = "com/roblox/engine/jni/NativeSettingsInterface";

/// The keyring item's `account` attribute and its label (shown in, e.g.,
/// GNOME's "Passwords and Keys").
const ACCOUNT: &str = "roblox-login";
const LABEL: &str = "Veneer: Roblox login";

fn path() -> PathBuf {
    paths::data_dir().join("cookies")
}

/// Where the login can be kept besides the fallback file.
trait SecretStore {
    fn store(&self, secret: &str) -> Result<(), String>;
    fn lookup(&self) -> Result<Option<String>, String>;
    fn clear(&self) -> Result<(), String>;
}

struct Keyring;

impl SecretStore for Keyring {
    fn store(&self, secret: &str) -> Result<(), String> {
        keyring::store(ACCOUNT, LABEL, secret)
    }
    fn lookup(&self) -> Result<Option<String>, String> {
        keyring::lookup(ACCOUNT)
    }
    fn clear(&self) -> Result<(), String> {
        keyring::clear(ACCOUNT)
    }
}

/// Say once per run that the keyring cannot be used.
fn keyring_unavailable(error: &str, file: &Path) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Ordering::Relaxed) {
        log::warn!(
            "cannot use the keyring ({error}); the Roblox login is kept in {} (readable only by you)",
            file.display()
        );
    }
}

fn read_file(file: &Path) -> Option<String> {
    let text = fs::read_to_string(file).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn write_file(file: &Path, secret: &str) -> std::io::Result<()> {
    let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(file)?;
    // `mode` only applies to new files.
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    f.write_all(secret.as_bytes())
}

fn load_with(store: &dyn SecretStore, file: &Path) -> String {
    if let Some(saved) = read_file(file) {
        match store.store(&saved).and_then(|()| store.lookup()) {
            Ok(Some(back)) if back == saved => match fs::remove_file(file) {
                Ok(()) => log::info!("moved the Roblox login from {} into the keyring", file.display()),
                Err(e) => log::warn!("the login is in the keyring, but {} could not be deleted: {e}", file.display()),
            },
            Ok(_) => log::warn!("the keyring did not keep the login; it stays in {}", file.display()),
            Err(e) => keyring_unavailable(&e, file),
        }
        return saved;
    }
    match store.lookup() {
        Ok(saved) => saved.map(|s| s.trim().to_string()).unwrap_or_default(),
        Err(e) => {
            keyring_unavailable(&e, file);
            String::new()
        }
    }
}

fn save_with(store: &dyn SecretStore, file: &Path, secret: &str) {
    match store.store(secret) {
        Ok(()) => {
            log::debug!("saved the session cookies in the keyring");
            // Any older copy in the file is now stale.
            let _ = fs::remove_file(file);
        }
        Err(e) => {
            keyring_unavailable(&e, file);
            match write_file(file, secret) {
                Ok(()) => log::debug!("saved the session cookies in {}", file.display()),
                Err(e) => log::warn!("could not save the session cookies: {e}"),
            }
        }
    }
}

fn clear_with(store: &dyn SecretStore, file: &Path) {
    if let Err(e) = store.clear() {
        log::debug!("could not clear the keyring item: {e}");
    }
    let _ = fs::remove_file(file);
}

/// The saved cookies as a `Cookie:` header value (`a=b; c=d`), or empty.
pub fn load() -> String {
    load_with(&Keyring, &path())
}

/// Parse the engine's Netscape-format dump (entries separated by `;`,
/// fields by tabs: domain at 0, name at 5, value at 6) into name/value
/// pairs for roblox.com.
fn parse(netscape: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in netscape.split(';') {
        let f: Vec<&str> = entry.trim().split('\t').collect();
        if f.len() < 7 || !f[0].trim_start_matches("#HttpOnly_").ends_with("roblox.com") {
            continue;
        }
        if matches!(f[5], "RBXThemeOverride" | "RBXHideThemeSetting") {
            continue;
        }
        out.push((f[5].to_string(), f[6].to_string()));
    }
    out
}

fn to_header(netscape: &str) -> String {
    let pairs: Vec<String> = parse(netscape).iter().map(|(n, v)| format!("{n}={v}")).collect();
    pairs.join("; ")
}

/// The engine's current roblox.com cookies (the live login session).
pub fn current() -> Vec<(String, String)> {
    dump().map(|d| parse(&d)).unwrap_or_default()
}

fn dump() -> Option<String> {
    with_frame(|| {
        let r = jcall!(
            SETTINGS,
            "nativeGetCookiesInNetscapeFormat",
            "(Ljava/lang/String;)Ljava/lang/String;",
            fn(JObject) -> JObject,
            jstr(BASE_URL)
        )?;
        unsafe { from_raw(r) }.and_then(|o| o.as_str().map(str::to_string))
    })
}

/// Ask the engine for its current cookies and save them (the login session).
pub fn save() {
    let Some(dump) = dump() else {
        return;
    };
    let header = to_header(&dump);
    if header.is_empty() {
        return;
    }
    save_with(&Keyring, &path(), &header);
}

/// Forget the saved session (after logout).
pub fn clear() {
    clear_with(&Keyring, &path());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A keyring that works, or fails every call.
    struct Fake {
        item: RefCell<Option<String>>,
        works: bool,
    }

    impl Fake {
        fn new(works: bool, item: Option<&str>) -> Fake {
            Fake { item: RefCell::new(item.map(str::to_string)), works }
        }
    }

    impl SecretStore for Fake {
        fn store(&self, secret: &str) -> Result<(), String> {
            if !self.works {
                return Err("no keyring".into());
            }
            *self.item.borrow_mut() = Some(secret.to_string());
            Ok(())
        }
        fn lookup(&self) -> Result<Option<String>, String> {
            if !self.works {
                return Err("no keyring".into());
            }
            Ok(self.item.borrow().clone())
        }
        fn clear(&self) -> Result<(), String> {
            if !self.works {
                return Err("no keyring".into());
            }
            *self.item.borrow_mut() = None;
            Ok(())
        }
    }

    fn temp_file(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("veneer-cookies-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn an_old_file_moves_into_the_keyring() {
        let file = temp_file("move");
        write_file(&file, "a=1; b=2\n").unwrap();
        let keyring = Fake::new(true, Some("older"));
        assert_eq!(load_with(&keyring, &file), "a=1; b=2");
        assert_eq!(keyring.item.borrow().as_deref(), Some("a=1; b=2"));
        assert!(!file.exists());
        // Next start: read from the keyring.
        assert_eq!(load_with(&keyring, &file), "a=1; b=2");
    }

    #[test]
    fn without_a_keyring_the_file_is_used_and_kept() {
        let file = temp_file("fallback");
        let keyring = Fake::new(false, None);
        assert_eq!(load_with(&keyring, &file), "");
        save_with(&keyring, &file, "a=1");
        assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(load_with(&keyring, &file), "a=1");
        assert!(file.exists());
        clear_with(&keyring, &file);
        assert!(!file.exists());
    }

    #[test]
    fn saving_to_the_keyring_removes_a_stale_file() {
        let file = temp_file("stale");
        write_file(&file, "old").unwrap();
        let keyring = Fake::new(true, None);
        save_with(&keyring, &file, "new");
        assert!(!file.exists());
        assert_eq!(load_with(&keyring, &file), "new");
        clear_with(&keyring, &file);
        assert_eq!(load_with(&keyring, &file), "");
    }

    #[test]
    fn a_keyring_that_loses_the_item_does_not_lose_the_login() {
        // Stores without error but gives something else back: keep the file.
        struct Forgetful;
        impl SecretStore for Forgetful {
            fn store(&self, _: &str) -> Result<(), String> {
                Ok(())
            }
            fn lookup(&self) -> Result<Option<String>, String> {
                Ok(None)
            }
            fn clear(&self) -> Result<(), String> {
                Ok(())
            }
        }
        let file = temp_file("forgetful");
        write_file(&file, "a=1").unwrap();
        assert_eq!(load_with(&Forgetful, &file), "a=1");
        assert!(file.exists());
        fs::remove_file(&file).unwrap();
    }
}
