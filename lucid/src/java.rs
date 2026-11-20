//! The "Java side" of the Roblox app, reimplemented: the static callback
//! classes native code calls into, and helpers for calling `native` methods.

use std::sync::OnceLock;
use std::time::Instant;

use lucid_jni::{JObject, JValue, Ref, new_instance, new_string, to_local, vm};

use crate::device::Device;

/// Address of a guest native method, logging when it does not exist.
pub fn native(class: &str, name: &str, sig: &str) -> Option<usize> {
    let addr = vm().native(class, name, sig);
    if addr.is_none() {
        log::error!("native method {class}.{name}{sig} not found in the engine");
    }
    addr
}

/// A local reference to a class object (the `jclass` of a static native).
pub fn class_ref(class: &str) -> JObject {
    to_local(Some(vm().class(class).object()))
}

/// A local reference to a new Java string.
pub fn jstr(s: &str) -> JObject {
    to_local(Some(new_string(s)))
}

pub fn local(r: &Ref) -> JObject {
    to_local(Some(r.clone()))
}

/// Call a static `native` method of the engine:
/// `jcall!("pkg/Class", "name", "(sig)V", fn(ArgTypes...) -> Ret, args...)`.
/// Evaluates to `Option<Ret>` (`None` if the engine lacks the method).
#[macro_export]
macro_rules! jcall {
    ($class:expr, $name:expr, $sig:expr, fn($($t:ty),*) $(-> $r:ty)?, $($a:expr),* $(,)?) => {
        $crate::jcall_on!($crate::java::class_ref($class); $class, $name, $sig, fn($($t),*) $(-> $r)?, $($a),*)
    };
}

/// Like [`jcall!`] but with an explicit receiver (for instance natives).
#[macro_export]
macro_rules! jcall_on {
    ($recv:expr; $class:expr, $name:expr, $sig:expr, fn($($t:ty),*) $(-> $r:ty)?, $($a:expr),* $(,)?) => {
        match $crate::java::native($class, $name, $sig) {
            Some(addr) => {
                let f: unsafe extern "C" fn(*mut lucid_jni::JNIEnv, lucid_jni::JObject $(, $t)*) $(-> $r)? =
                    unsafe { std::mem::transmute(addr) };
                log::debug!("-> {}.{}", $class, $name);
                let recv = $recv;
                let r = unsafe { f(lucid_jni::env(), recv $(, $a)*) };
                log::debug!("<- {}.{}", $class, $name);
                Some(r)
            }
            None => None,
        }
    };
}

/// The logged-in user, as last reported by the Lua app.
#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub display_name: String,
    pub under13: bool,
    pub membership: i32,
}

static CURRENT_USER: std::sync::Mutex<Option<User>> = std::sync::Mutex::new(None);

pub fn user() -> User {
    CURRENT_USER.lock().unwrap().clone().unwrap_or(User {
        id: -1,
        name: String::new(),
        display_name: String::new(),
        under13: true,
        membership: 0,
    })
}

fn update_user(kind: &str, data: &str) {
    match kind {
        "DID_LOG_IN" | "DID_SIGN_UP" | "DID_SWITCH_ACCOUNT" => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else {
                return;
            };
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
            let u = User {
                id: v
                    .get("userId")
                    .and_then(|x| x.as_i64().or_else(|| x.as_str()?.parse().ok()))
                    .unwrap_or(-1),
                name: s("username"),
                display_name: s("displayName"),
                under13: v.get("isUnder13").and_then(|x| x.as_bool()).unwrap_or(true),
                membership: v.get("membershipType").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
            };
            log::info!("logged in as {} ({})", u.name, u.id);
            *CURRENT_USER.lock().unwrap() = Some(u);
        }
        "DID_LOG_OUT" => *CURRENT_USER.lock().unwrap() = None,
        _ => {}
    }
}

const GL_JAVA: &str = "com/roblox/engine/jni/NativeGLJavaInterface";
const LOCALE: &str = "com/roblox/engine/jni/locale/NativeLocaleJavaInterface";
const USER: &str = "com/roblox/engine/jni/user/NativeUserJavaInterface";
const REPORTER: &str = "com/roblox/engine/jni/reporter/SessionReporterJavaInterface";
const FMOD: &str = "org/fmod/FMOD";

/// A notification the engine sent to the app shell.
pub type Notification = (String, String);

static NOTIFY: OnceLock<Box<dyn Fn(Notification) + Send + Sync>> = OnceLock::new();

/// Install the sink for `onDataModelNotificationCallback` events.
pub fn set_notification_sink(f: impl Fn(Notification) + Send + Sync + 'static) {
    let _ = NOTIFY.set(Box::new(f));
}

fn arg_str(args: &[JValue], i: usize) -> String {
    args.get(i).and_then(|a| a.as_str()).unwrap_or("").to_string()
}

/// Build the `DeviceStaticParams` object.
fn device_static_params(d: &Device) -> Ref {
    let o = new_instance("com/roblox/engine/jni/model/DeviceStaticParams");
    let i = o.as_instance().unwrap();
    i.set("appBuildVariant", "googleProdRelease")
        .set("appVersion", d.app_version.as_str())
        .set("cpu64Bit", true)
        .set("deviceName", d.device_name.as_str())
        .set("deviceSku", JValue::null())
        .set("manufacturer", d.manufacturer.as_str())
        .set("osVersion", d.sdk.to_string())
        .set("socModel", JValue::null());
    o
}

/// Register every Java method the engine is known to call.
pub fn install(device: &Device) {
    lucid_jni::install_stdlib();
    let vm = vm();
    let start = Instant::now();

    let statics = device_static_params(device);
    vm.on(
        GL_JAVA,
        "getDeviceStaticParams",
        "()Lcom/roblox/engine/jni/model/DeviceStaticParams;",
        move |_, _| statics.clone(),
    );
    vm.on(
        GL_JAVA,
        "onDataModelNotificationCallback",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |_, a| {
            let n = (arg_str(a, 0), arg_str(a, 1));
            // The payload can carry account details: keep it out of the
            // default log level.
            log::info!("engine notification {}", n.0);
            log::trace!("engine notification payload {}", n.1);
            update_user(&n.0, &n.1);
            if let Some(sink) = NOTIFY.get() {
                sink(n);
            }
        },
    );
    vm.on(
        GL_JAVA,
        "onAppBridgeNotification",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |_, a| log::info!("app bridge notification {} {}", arg_str(a, 0), arg_str(a, 1)),
    );
    vm.on(GL_JAVA, "gameLoadedCallback", "(J)V", |_, _| log::info!("game loaded"));
    vm.on(GL_JAVA, "gameDidLeave", "()V", |_, _| {
        log::info!("game left");
        crate::game::on_left();
    });
    vm.on(GL_JAVA, "exitGameWithError", "(I)V", |_, a| {
        log::warn!("game exited with error {}", a[0].as_i64());
        crate::game::on_left();
    });
    vm.on(GL_JAVA, "screenOrientationChanged", "(I)V", |_, _| ());
    // Older route for web pages: (title, url).
    vm.on(
        GL_JAVA,
        "openNativeOverlay",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |_, a| {
            let url = arg_str(a, 1);
            std::thread::spawn(move || crate::webview::open_in_browser(&url));
        },
    );
    vm.on(GL_JAVA, "listenToMotionEvents", "(Ljava/lang/String;)V", |_, _| ());
    vm.on(GL_JAVA, "onVrSessionStateUpdate", "(I)V", |_, _| ());

    vm.on(
        "com/roblox/universalapp/logging/LoggingProtocol",
        "getProcessTimestamp",
        "()J",
        move |_, _| start.elapsed().as_millis() as i64,
    );

    let (lang, country) = lucid_android::window::locale();
    let java_locale = format!("{lang}_{country}");
    let rbx_locale = java_locale.to_lowercase();
    vm.on(LOCALE, "getLocale", "()Ljava/lang/String;", move |_, _| new_string(&java_locale));
    let l = rbx_locale.clone();
    vm.on(LOCALE, "getRobloxLocale", "()Ljava/lang/String;", move |_, _| new_string(&l));
    vm.on(LOCALE, "getGameLocale", "()Ljava/lang/String;", move |_, _| new_string(&rbx_locale));

    // Logged-out defaults; the Lua app drives login itself.
    vm.on(USER, "getAlternateName", "()Ljava/lang/String;", |_, _| new_string(""));
    vm.on(USER, "getDisplayName", "()Ljava/lang/String;", |_, _| new_string(&user().display_name));
    vm.on(USER, "getUsername", "()Ljava/lang/String;", |_, _| new_string(&user().name));
    vm.on(USER, "getTheme", "()Ljava/lang/String;", |_, _| new_string("dark"));
    vm.on(USER, "getPlatformName", "()Ljava/lang/String;", |_, _| new_string(""));
    vm.on(USER, "getHasRobloxSubscription", "()Z", |_, _| false);
    vm.on(USER, "getIsUnder13", "()Z", |_, _| user().under13);
    vm.on(USER, "getMembershipType", "()I", |_, _| user().membership);
    vm.on(USER, "getUserId", "()J", |_, _| user().id);

    let version = device.app_version.clone();
    vm.on(REPORTER, "getAppVersion", "()Ljava/lang/String;", move |_, _| new_string(&version));
    let files = crate::paths::files_dir().to_string_lossy().into_owned();
    vm.on(REPORTER, "getFilesDir", "()Ljava/lang/String;", move |_, _| new_string(&files));
    vm.on(REPORTER, "getLastLoggedInUser", "()Ljava/lang/String;", |_, _| new_string(""));
    vm.on(REPORTER, "getLastLoggedInUserId", "()Ljava/lang/String;", |_, _| new_string("-1"));
    vm.on(
        REPORTER,
        "sendSessionReport",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |_, _| (),
    );

    vm.on(
        "com/roblox/engine/jni/util/NetworkUtils",
        "getPublicIPv4Addresseses",
        "()Ljava/lang/String;",
        |_, _| new_string(""),
    );
    vm.on(
        "com/roblox/client/LocalStorageManager",
        "getAllocatableBytes",
        "()J",
        |_, _| 8i64 << 30,
    );

    // djinni keeps Java proxies alive through this; with no GC it is a no-op.
    for sig in ["(Ljava/lang/Object;J)V", "(Ljava/lang/Object;Ljava/lang/Class;J)V"] {
        vm.on("com/snapchat/djinni/NativeObjectManager", "register", sig, |_, _| ());
    }

    // FMOD: no AAudio or Java audio device yet, so it runs without output.
    vm.on(FMOD, "checkInit", "()Z", |_, _| true);
    vm.on(FMOD, "supportsAAudio", "()Z", |_, _| false);
    vm.on(FMOD, "supportsLowLatency", "()Z", |_, _| false);
    vm.on(FMOD, "lowLatencyFlag", "()Z", |_, _| false);
    vm.on(FMOD, "proAudioFlag", "()Z", |_, _| false);
    vm.on(FMOD, "isBluetoothOn", "()Z", |_, _| false);
    vm.on(FMOD, "getOutputSampleRate", "()I", |_, _| 48000);
    vm.on(FMOD, "getOutputBlockSize", "()I", |_, _| 512);
    vm.on(
        FMOD,
        "getAssetManager",
        "()Landroid/content/res/AssetManager;",
        |_, _| new_instance("android/content/res/AssetManager"),
    );
}
