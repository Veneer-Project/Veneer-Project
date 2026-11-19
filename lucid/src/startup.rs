//! The startup sequence the Android app's Java code performs, in the same
//! order, against the engine's `native` methods.
//!
//! Reference: `RobloxApplication.onCreate`, `ActivitySplash`, the app
//! settings setup and `ActivityNativeMain` in the decompiled APK.

use std::error::Error;
use std::fs;

use lucid_android::window::NativeWindow;
use lucid_jni::{JObject, JValue, Ref, new_instance, new_list, with_frame};

use crate::device::{API_URL, BASE_URL, Device, SETTINGS_GROUP};
use crate::java::{jstr, local};
use crate::paths;

const SETTINGS: &str = "com/roblox/engine/jni/NativeSettingsInterface";
const GL: &str = "com/roblox/engine/jni/NativeGLInterface";
const BRIDGE: &str = "com/roblox/engine/jni/NativeAppBridgeInterface";
const INPUT: &str = "com/roblox/engine/jni/NativeInputInterface";
const REPORTING: &str = "com/roblox/engine/jni/NativeReportingInterface";

const S_V: &str = "(Ljava/lang/String;)V";
const SS_V: &str = "(Ljava/lang/String;Ljava/lang/String;)V";

fn path_str(p: std::path::PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

/// `RobloxApplication.onCreate` after the library is loaded.
pub fn application_on_create() {
    with_frame(|| {
        let assets = new_instance("android/content/res/AssetManager");
        let context = new_instance("android/content/Context");
        jcall!(
            "com/roblox/client/JNIAAssetManagerSetup",
            "initNative",
            "(Landroid/content/res/AssetManager;)V",
            fn(JObject),
            local(&assets)
        );
        let storage = new_instance("com/roblox/client/LocalStorageManager");
        jcall_on!(
            local(&storage);
            "com/roblox/client/LocalStorageManager",
            "initStorageManagerNativeV3",
            "(Landroid/content/res/AssetManager;Ljava/lang/String;Ljava/lang/String;)V",
            fn(JObject, JObject, JObject),
            local(&assets),
            jstr(&path_str(paths::files_dir())),
            jstr(&path_str(paths::cache_dir()))
        );
        for class in [
            "com/roblox/universalapp/linking/JNIBaseUrlProtocol",
            "com/roblox/universalapp/linking/JNIWebLoginProtocol",
        ] {
            jcall!(class, "init", "(Landroid/content/Context;)V", fn(JObject), local(&context));
        }
    });
}

/// The CA bundle the engine's HTTP stack expects at `<files>/exe/ssl/`.
fn install_ca_bundle() -> std::io::Result<()> {
    let src = paths::assets_dir().join("ssl/cacert.pem");
    let dst = paths::files_dir().join("exe/ssl/cacert.pem");
    if !dst.exists() || fs::metadata(&src)?.len() != fs::metadata(&dst)?.len() {
        fs::create_dir_all(dst.parent().unwrap())?;
        fs::copy(&src, &dst)?;
    }
    Ok(())
}

/// The directory handed to the engine as `PlatformParams.assetFolderPath`.
/// Android creates it empty too; real assets come via the asset manager.
pub fn asset_folder() -> String {
    let base = paths::data_dir().join("app_assets");
    for d in ["content", "ExtraContent", "android"] {
        let _ = fs::create_dir_all(base.join(d));
    }
    path_str(base.join("content"))
}

/// Settings and global engine initialisation (`AppManager.initialize` and
/// the "ASMA.setup" step).
pub fn engine_setup(device: &Device, cookies: &str) -> Result<(), Box<dyn Error>> {
    install_ca_bundle()?;
    with_frame(|| {
        let set_base_url = || {
            jcall!(SETTINGS, "nativeSetBaseUrl", SS_V, fn(JObject, JObject), jstr(BASE_URL), jstr(API_URL));
            jcall!(SETTINGS, "nativeSetRobloxChannel", S_V, fn(JObject), jstr(""));
        };
        set_base_url();
        jcall!(
            SETTINGS,
            "nativeSetDefaultAppPolicyFile",
            S_V,
            fn(JObject),
            jstr("content/guac/defaultConfigs/GuacDefaultPolicy-GlobalDist.json")
        );
        jcall!(REPORTING, "initAppShellReporter", "()V", fn(),);

        jcall!(SETTINGS, "nativeSetExceptionReasonFilename", S_V, fn(JObject), jstr("exception_reason.txt"));
        set_base_url();
        jcall!(SETTINGS, "nativeOverrideChannelPlatformName", S_V, fn(JObject), jstr(SETTINGS_GROUP));
        jcall!(SETTINGS, "nativeSetCacheDirectory", S_V, fn(JObject), jstr(&path_str(paths::cache_dir())));
        jcall!(SETTINGS, "nativeSetFilesDirectory", S_V, fn(JObject), jstr(&path_str(paths::files_dir())));
        jcall!(SETTINGS, "nativeInitFastLog", "()V", fn(),);
        jcall!(SETTINGS, "nativeSetRobloxVersion", S_V, fn(JObject), jstr(&device.app_version));
        jcall!(
            SETTINGS,
            "nativeSetPlatformHeadersWithIdfa",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            fn(JObject, JObject, JObject),
            jstr(&device.android_id),
            jstr("googleplay"),
            jstr("")
        );
        // Crashpad (which forks an Android app_process) is deliberately
        // not initialised.
        jcall!(SETTINGS, "nativeSetMultipleCookies", SS_V, fn(JObject, JObject), jstr(BASE_URL), jstr(cookies));

        jcall!(GL, "nativeGameGlobalInit", "()V", fn(),);
        jcall!(
            SETTINGS,
            "nativeSetHttpClientProxy",
            "(Ljava/lang/String;J)V",
            fn(JObject, i64),
            jstr(""),
            0
        );
        jcall!(GL, "nativeUpdateAdapterInit", "()V", fn(),);
    });
    Ok(())
}

/// Download the client settings (fast flags) the way the app does.
fn fetch_client_settings() -> Result<(String, String), Box<dyn Error>> {
    let url = format!(
        "https://clientsettingscdn.roblox.com/v2/settings/application/{SETTINGS_GROUP}"
    );
    log::info!("fetching client settings");
    let resp = ureq::get(&url)
        .set("Pragma", "akamai-x-cache-on")
        .timeout(std::time::Duration::from_secs(30))
        .call()?;
    let signature = resp.header("X-Signature-Ed25519").unwrap_or("").to_string();
    let mut body = String::new();
    std::io::Read::read_to_string(&mut resp.into_reader(), &mut body)?;
    Ok((body, signature))
}

/// Fast flag overrides: lucid's own defaults, then the user's
/// `fflags.json` (a JSON object, e.g. `{"DFIntTaskSchedulerTargetFps": 144}`)
/// in lucid's config directory.
fn flag_overrides() -> Option<String> {
    let mut flags = serde_json::Map::new();
    for (k, v) in crate::flags::DEFAULTS {
        flags.insert(k.to_string(), serde_json::Value::String(v.to_string()));
    }
    let path = paths::config_dir().join("fflags.json");
    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&text) {
            Ok(user) => {
                log::info!("applying {} fast flag overrides from {}", user.len(), path.display());
                for (k, v) in user {
                    // The engine wants every value as a string.
                    let v = match v {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    flags.insert(k, serde_json::Value::String(v));
                }
            }
            Err(e) => log::warn!("ignoring {}: {e}", path.display()),
        },
        Err(_) => {}
    }
    if let Ok(extra) = std::env::var("LUCID_FFLAGS") {
        if let Ok(m) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&extra) {
            for (k, v) in m {
                let v = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                flags.insert(k, serde_json::Value::String(v));
            }
        }
    }
    if flags.is_empty() {
        None
    } else {
        serde_json::to_string(&flags).ok()
    }
}

/// Fetch client settings and hand them to the engine.
pub fn client_settings() -> Result<(), Box<dyn Error>> {
    let (body, signature) = fetch_client_settings()?;
    log::info!("client settings: {} bytes", body.len());
    let signed = body.contains("\"FFlagPassFlagSignatureToEngine\":\"True\"")
        || body.contains("\"FFlagPassFlagSignatureToEngine\":\"true\"");
    let overrides = flag_overrides();
    let result = with_frame(|| {
        // Overrides are a flat JSON object of flag name -> string value
        // (Android reads them from ClientAppSettings.json), or null.
        let ov: JObject = match &overrides {
            Some(json) => jstr(json),
            None => std::ptr::null_mut(),
        };
        if signed {
            jcall!(
                GL,
                "nativeInitClientSettingsSigned",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                fn(JObject, JObject, JObject, JObject) -> i32,
                jstr(&body),
                jstr(&signature),
                ov,
                jstr(SETTINGS_GROUP)
            )
        } else {
            jcall!(
                GL,
                "nativeInitClientSettings",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                fn(JObject, JObject, JObject) -> i32,
                jstr(&body),
                ov,
                jstr(SETTINGS_GROUP)
            )
        }
    });
    match result {
        Some(0) => {}
        Some(code) => return Err(format!("the engine rejected the client settings (code {code})").into()),
        None => return Err("client settings entry point missing".into()),
    }
    with_frame(|| {
        let exits = new_list(Vec::new());
        jcall!(
            GL,
            "nativePostClientSettingsLoadedInitialization3",
            "(Ljava/util/List;)V",
            fn(JObject),
            local(&exits)
        );
        jcall!(SETTINGS, "nativeSetPreferencesFile", S_V, fn(JObject), jstr("prefs"));
    });
    Ok(())
}

pub fn platform_params(window: &NativeWindow) -> Ref {
    let (w, h) = window.size();
    let dpi = window.density().max(1) as f32;
    let o = new_instance("com/roblox/engine/jni/model/PlatformParams");
    o.as_instance()
        .unwrap()
        .set("assetFolderPath", asset_folder())
        .set("dpiScale", dpi / 160.0)
        .set("isKeyboardDevice", true)
        .set("isMouseDevice", true)
        .set("isTouchDevice", false)
        .set("viewportWidthMm", (w as f32 / dpi * 25.4).round() as i32)
        .set("viewportHeightMm", (h as f32 / dpi * 25.4).round() as i32)
        .set("isLuaChatEnabled", true)
        .set("isLuaGamesPageEnabled", true)
        .set("isLuaHomePageEnabled", true)
        .set("isTablet", true);
    o
}

fn device_params(device: &Device, window: &NativeWindow) -> Ref {
    let (w, h) = window.size();
    let (_, country) = lucid_android::window::locale();
    let o = new_instance("com/roblox/engine/jni/model/DeviceParams");
    o.as_instance()
        .unwrap()
        .set("appBuildVariant", "googleProdRelease")
        .set("appVersion", device.app_version.as_str())
        .set("country", country)
        .set("cpu64Bit", true)
        .set("deviceName", device.device_name.as_str())
        .set("deviceSku", JValue::null())
        .set("deviceTotalMemoryMB", device.total_memory_mb)
        .set("displayPhysicalHeightPixels", h)
        .set("displayPhysicalWidthPixels", w)
        .set("displayResolution", format!("{w}x{h}"))
        .set("isChrome", false)
        .set("isLowRamDevice", false)
        .set("largeMemoryClass", 512)
        .set("lowMemoryKillerBackgroundAppThreshold", 0i64)
        .set("lowMemoryKillerForegroundAppThreshold", 0i64)
        .set("manufacturer", device.manufacturer.as_str())
        .set("memoryClass", 256)
        .set("networkType", "WiFi")
        .set("osVersion", device.sdk.to_string())
        .set("socModel", JValue::null())
        .set("testDeviceName", JValue::null());
    o
}

fn dp(px: i32, window: &NativeWindow) -> i32 {
    (px as f32 * 160.0 / window.density().max(1) as f32 + 0.5) as i32
}

/// Tell the engine about the usable UI area (no system bars on desktop).
pub fn update_ui_sizes(window: &NativeWindow) {
    with_frame(|| {
        jcall!(
            GL,
            "updateAppUISizes",
            "(IIIII)V",
            fn(i32, i32, i32, i32, i32),
            dp(window.size().0, window),
            0,
            0,
            0,
            0
        );
    });
}

/// Hand the (new or resized) surface to the app shell.
pub fn update_surface(window: &NativeWindow) {
    with_frame(|| {
        let surface = new_instance("android/view/Surface");
        let pp = platform_params(window);
        jcall!(
            GL,
            "nativeAppBridgeV2UpdateSurfaceAppWithPlatformParams",
            "(Landroid/view/Surface;Lcom/roblox/engine/jni/model/PlatformParams;)V",
            fn(JObject, JObject),
            local(&surface),
            local(&pp)
        );
    });
    update_ui_sizes(window);
}

/// `ActivityNativeMain`: start the app bridge and the Lua app shell on the
/// window's surface.
pub fn start_app(device: &Device, window: &NativeWindow, cookies: &str) {
    let (w, h) = window.size();
    let ua = device.user_agent(w, h, window.density());
    with_frame(|| {
        jcall!(INPUT, "nativeSetGyroscopeEnabled", "(Z)V", fn(u8), 0);
        jcall!(INPUT, "nativeSetAccelerometerEnabled", "(Z)V", fn(u8), 0);
        jcall!(GL, "nativePassCurrentDisplayRefreshRate", "(F)V", fn(f32), 60.0);
        jcall!(REPORTING, "applicationForegrounded", "()V", fn(),);
        jcall!(
            "com/roblox/universalapp/applifecyclenativeadapter/JNIAppLifecycleNativeAdapter",
            "setActive",
            "()V",
            fn(),
        );

        let dev = device_params(device, window);
        jcall!(
            SETTINGS,
            "nativeSetDeviceInfo",
            "(Lcom/roblox/engine/jni/model/DeviceParams;)V",
            fn(JObject),
            local(&dev)
        );
        jcall!(
            BRIDGE,
            "nativeAppBridgeAppStart",
            "(Ljava/lang/String;Ljava/lang/String;ZLjava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            fn(JObject, JObject, u8, JObject, JObject, JObject),
            jstr(BASE_URL),
            jstr(&ua),
            1,
            jstr(&device.android_id),
            jstr("AppAndroidV"),
            jstr("")
        );
        jcall!(SETTINGS, "nativeSetMultipleCookies", SS_V, fn(JObject, JObject), jstr(BASE_URL), jstr(cookies));

        let pp = platform_params(window);
        let init = new_instance("com/roblox/engine/jni/autovalue/InitParams");
        init.as_instance()
            .unwrap()
            .set("baseURL", BASE_URL)
            .set("buildVariant", JValue::null())
            .set("deviceParams", dev.clone())
            .set("isPotato", false)
            .set("isTablet", true)
            .set("isVrDevice", false)
            .set("pioneerPlaceId", 0i64)
            .set("platformParams", pp.clone())
            .set("userAgent", ua.as_str())
            .set("vrContext", JValue::null());
        jcall!(
            GL,
            "nativeAppBridgeV2InitWithParams",
            "(Lcom/roblox/engine/jni/autovalue/InitParams;)V",
            fn(JObject),
            local(&init)
        );
        jcall!(GL, "isColdStartDeeplinkToGame", "()Z", fn() -> u8,);
        jcall!(GL, "nativeAppBridgeStartLuaAppDM", "()V", fn(),);

    });
    start_app_surface(window);
}

/// Start (or return to) the Lua app shell on the window's surface.
pub fn start_app_surface(window: &NativeWindow) {
    let u = crate::java::user();
    with_frame(|| {
        jcall!(
            GL,
            "setTaskSchedulerBackgroundMode",
            "(ZLjava/lang/String;)V",
            fn(u8, JObject),
            0,
            jstr("ASMA.start")
        );
        let surface = new_instance("android/view/Surface");
        let start = new_instance("com/roblox/engine/jni/autovalue/StartAppParams");
        start
            .as_instance()
            .unwrap()
            .set("appStarterPlace", "rbxasset://places/Mobile.rbxl")
            .set("appStarterScript", "LuaAppStarterScript")
            .set("appUserId", u.id)
            .set("isUnder13", u.under13)
            .set("membershipType", u.membership)
            .set("platformParams", platform_params(window))
            .set("selectedTheme", "dark")
            .set("surface", surface)
            .set("username", u.name.as_str())
            .set("vrContext", JValue::null());
        jcall!(
            GL,
            "nativeAppBridgeV2StartAppWithParams",
            "(Lcom/roblox/engine/jni/autovalue/StartAppParams;)V",
            fn(JObject),
            local(&start)
        );
    });
    update_surface(window);
}

/// The window is closing: tell the engine the app is going away, the way
/// Android's lifecycle would, so it flushes settings and storage.
pub fn shutdown(in_game: bool) {
    const LIFECYCLE: &str =
        "com/roblox/universalapp/applifecyclenativeadapter/JNIAppLifecycleNativeAdapter";
    with_frame(|| {
        jcall!(LIFECYCLE, "setInactive", "()V", fn(),);
        if in_game {
            jcall!(GL, "nativeOnFragmentStop", "()V", fn(),);
            jcall!(REPORTING, "gameBackgrounded", "()V", fn(),);
            jcall!(GL, "nativeAppBridgeV2PauseGame", "()V", fn(),);
        } else {
            jcall!(GL, "nativeAppBridgeV2PauseApp", "()V", fn(),);
        }
        jcall!(LIFECYCLE, "setHidden", "()V", fn(),);
        jcall!(REPORTING, "applicationBackgrounded", "()V", fn(),);
    });
}
