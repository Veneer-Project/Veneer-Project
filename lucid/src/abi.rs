//! The Android ABI that matches the CPU Veneer was built for. The engine
//! runs natively, so an x86-64 Veneer needs Roblox's x86_64 libraries and an
//! AArch64 Veneer the arm64-v8a ones.

/// Folder of the native libraries inside the APK (`lib/<ABI>/`).
#[cfg(target_arch = "x86_64")]
pub const ANDROID: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
pub const ANDROID: &str = "arm64-v8a";

/// The same ABI as Google Play spells it in split names
/// (`split_config.<SPLIT>.apk`).
#[cfg(target_arch = "x86_64")]
pub const SPLIT: &str = "x86_64";
#[cfg(target_arch = "aarch64")]
pub const SPLIT: &str = "arm64_v8a";

/// File name of the split APK with the native libraries.
pub fn split_apk() -> String {
    format!("split_config.{SPLIT}.apk")
}

/// The Google Play device profile presented when downloading the game. On
/// x86-64 an x86-64 Chromebook (the "Play Games on PC" profile is not offered
/// Roblox at all); on AArch64 a Pixel Tablet.
#[cfg(target_arch = "x86_64")]
pub const DEVICE_PROPERTIES: &str = include_str!("../data/play_device.properties");
#[cfg(target_arch = "aarch64")]
pub const DEVICE_PROPERTIES: &str = include_str!("../data/play_device_arm64.properties");
