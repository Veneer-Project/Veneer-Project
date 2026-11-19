//! The "device" lucid presents to the engine.

use std::fs;

use crate::paths;

pub const BASE_URL: &str = "https://www.roblox.com/";
pub const API_URL: &str = "https://api.roblox.com/";
pub const SETTINGS_GROUP: &str = "GoogleAndroidApp";

pub struct Device {
    pub app_version: String,
    pub manufacturer: String,
    pub device_name: String,
    /// Android API level we claim to be.
    pub sdk: u32,
    pub android_id: String,
    pub total_memory_mb: i32,
}

fn total_memory_mb() -> i32 {
    fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map(|kb| (kb / 1024) as i32)
        .unwrap_or(4096)
}

/// A stable random per-install identifier (Android's `android_id`).
fn android_id() -> String {
    let path = paths::data_dir().join("android_id");
    if let Ok(id) = fs::read_to_string(&path) {
        let id = id.trim();
        if id.len() == 16 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return id.to_string();
        }
    }
    let mut bytes = [0u8; 8];
    unsafe { libc::getrandom(bytes.as_mut_ptr() as *mut libc::c_void, 8, 0) };
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = fs::write(&path, &id);
    id
}

impl Device {
    pub fn detect() -> Device {
        let app_version = fs::read_to_string(paths::data_dir().join("app/version"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        Device {
            app_version,
            manufacturer: "Linux".into(),
            device_name: "Linux PC".into(),
            sdk: 33,
            android_id: android_id(),
            total_memory_mb: total_memory_mb(),
        }
    }

    /// The user agent the Android app sends (note the double spaces).
    pub fn user_agent(&self, width: i32, height: i32, dpi: i32) -> String {
        let (wdp, hdp) = (width * 160 / dpi.max(1), height * 160 / dpi.max(1));
        format!(
            "Mozilla/5.0 ({}MB; {width}x{height}; {dpi}x{dpi}; {wdp}x{hdp}; {}; {}) \
             AppleWebKit/537.36 (KHTML, like Gecko)  ROBLOX Android App {v} Tablet Hybrid()  \
             GooglePlayStore RobloxApp/{v} (GlobalDist; GooglePlayStore)",
            self.total_memory_mb,
            self.device_name,
            13,
            v = self.app_version
        )
    }
}
