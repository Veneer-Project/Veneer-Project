//! Persisting the login session. The engine owns the live cookie jar; like
//! the Android app, we read it back after login and replay it at startup.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use lucid_jni::{JObject, from_raw, with_frame};

use crate::device::BASE_URL;
use crate::java::jstr;
use crate::jcall;
use crate::paths;

const SETTINGS: &str = "com/roblox/engine/jni/NativeSettingsInterface";

fn path() -> PathBuf {
    paths::data_dir().join("cookies")
}

/// The saved cookies as a `Cookie:` header value (`a=b; c=d`), or empty.
pub fn load() -> String {
    fs::read_to_string(path()).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Convert the engine's Netscape-format dump (entries separated by `;`,
/// fields by tabs: domain at 0, name at 5, value at 6) to a header value.
fn to_header(netscape: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for entry in netscape.split(';') {
        let f: Vec<&str> = entry.trim().split('\t').collect();
        if f.len() < 7 || !f[0].trim_start_matches("#HttpOnly_").ends_with("roblox.com") {
            continue;
        }
        if matches!(f[5], "RBXThemeOverride" | "RBXHideThemeSetting") {
            continue;
        }
        out.push(format!("{}={}", f[5], f[6]));
    }
    out.join("; ")
}

/// Ask the engine for its current cookies and write them to disk
/// (readable by the user only: this file is the login session).
pub fn save() {
    let dump = with_frame(|| {
        let r = jcall!(
            SETTINGS,
            "nativeGetCookiesInNetscapeFormat",
            "(Ljava/lang/String;)Ljava/lang/String;",
            fn(JObject) -> JObject,
            jstr(BASE_URL)
        )?;
        unsafe { from_raw(r) }.and_then(|o| o.as_str().map(str::to_string))
    });
    let Some(dump) = dump else {
        return;
    };
    let header = to_header(&dump);
    if header.is_empty() {
        return;
    }
    let result = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path())
        .and_then(|mut f| f.write_all(header.as_bytes()));
    match result {
        Ok(()) => log::debug!("saved session cookies"),
        Err(e) => log::warn!("could not save session cookies: {e}"),
    }
}

/// Forget the saved session (after logout).
pub fn clear() {
    let _ = fs::remove_file(path());
}
