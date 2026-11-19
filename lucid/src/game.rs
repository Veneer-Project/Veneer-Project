//! Joining and leaving experiences.
//!
//! The Lua app asks to launch a game by publishing a JSON message on the
//! engine's message bus; the Android app answers by starting the game on a
//! surface. Afterwards the engine reports `gameDidLeave` and the app shell
//! is started again.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use lucid_jni::{JObject, JValue, Ref, from_raw, new_instance, vm, with_frame};
use serde_json::Value;

use crate::java::{local, user};
use crate::jcall;
use crate::jcall_on;
use crate::startup;

const GL: &str = "com/roblox/engine/jni/NativeGLInterface";
const REPORTING: &str = "com/roblox/engine/jni/NativeReportingInterface";
const BUS: &str = "com/roblox/universalapp/messagebus/MessageBus";
const RAW_CALLBACK: &str = "com/roblox/universalapp/messagebus/RawCallback";
const CALLBACK_IMPL: &str = "com/roblox/universalapp/messagebus/MessageBus$a";
const CONNECTION: &str = "com/roblox/universalapp/messagebus/Connection";

static IN_GAME: AtomicBool = AtomicBool::new(false);
/// Objects the engine holds references to for the process lifetime.
static KEEP: Mutex<Vec<Ref>> = Mutex::new(Vec::new());

pub fn in_game() -> bool {
    IN_GAME.load(Ordering::Acquire)
}

/// A launch request, as published by the Lua app.
#[derive(Default, Debug)]
pub struct Launch {
    pub place_id: i64,
    pub user_id: i64,
    pub conversation_id: i64,
    pub referred_by_player_id: i64,
    pub game_instance_id: String,
    pub reserved_server_access_code: String,
    pub call_id: String,
    pub referral_page: String,
    pub access_code: String,
    pub link_code: String,
    pub launch_data: String,
    pub event_id: String,
    pub game_join_context: String,
    pub join_attempt_id: String,
    pub join_attempt_origin: String,
    pub iso_context: String,
    pub game_id_to_exclude: String,
}

fn num(v: &Value, key: &str) -> i64 {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

impl Launch {
    pub fn place(place_id: i64) -> Launch {
        Launch {
            place_id,
            ..Launch::default()
        }
    }

    fn from_json(json: &str) -> Option<Launch> {
        let v: Value = serde_json::from_str(json).ok()?;
        Some(Launch {
            place_id: num(&v, "placeId"),
            user_id: num(&v, "userId"),
            conversation_id: num(&v, "conversationId"),
            referred_by_player_id: num(&v, "referredByPlayerId"),
            game_instance_id: text(&v, "gameInstanceId"),
            reserved_server_access_code: text(&v, "reservedServerAccessCode"),
            call_id: text(&v, "callId"),
            referral_page: text(&v, "referralPage"),
            access_code: text(&v, "accessCode"),
            link_code: text(&v, "linkCode"),
            launch_data: text(&v, "launchData"),
            event_id: text(&v, "eventId"),
            game_join_context: text(&v, "gameJoinContext"),
            join_attempt_id: text(&v, "joinAttemptId"),
            join_attempt_origin: text(&v, "joinAttemptOrigin"),
            iso_context: text(&v, "isoContext"),
            game_id_to_exclude: text(&v, "gameIdToExclude"),
        })
    }

    /// The engine's join request type, chosen like the Android app does.
    fn request_type(&self) -> i32 {
        if self.place_id > 0 && self.conversation_id > 0 {
            6
        } else if self.user_id > 0 {
            1
        } else if self.place_id <= 0 {
            -1
        } else if !self.link_code.is_empty() || !self.access_code.is_empty() {
            2
        } else if !self.game_instance_id.is_empty() {
            3
        } else if !self.reserved_server_access_code.is_empty() {
            8
        } else {
            0
        }
    }
}

/// Start an experience on the window's surface.
pub fn launch(l: Launch) {
    let Some(window) = lucid_android::window::window() else {
        return;
    };
    if IN_GAME.swap(true, Ordering::AcqRel) {
        log::warn!("launch request ignored: already in a game");
        return;
    }
    log::info!("joining place {} (request type {})", l.place_id, l.request_type());
    crate::presence::set(crate::presence::State::InGame { place_id: l.place_id });
    let u = user();
    with_frame(|| {
        // On Android the app shell's surface goes away when the game
        // activity covers it.
        jcall!(GL, "nativeAppBridgeV2PauseApp", "()V", fn(),);

        let surface = new_instance("android/view/Surface");
        let p = new_instance("com/roblox/engine/jni/autovalue/StartGameParams");
        p.as_instance()
            .unwrap()
            .set("surface", surface)
            .set("platformParams", startup::platform_params(window))
            .set("deviceParams", JValue::null())
            .set("vrContext", JValue::null())
            .set("placeId", l.place_id)
            .set("userId", l.user_id)
            .set("conversationId", l.conversation_id)
            .set("referredByPlayerId", l.referred_by_player_id)
            .set("isUnder13", u.under13)
            .set("username", u.name.as_str())
            .set("joinRequestType", l.request_type())
            .set("accessCode", l.access_code.as_str())
            .set("linkCode", l.link_code.as_str())
            .set("gameId", l.game_instance_id.as_str())
            .set("reservedServerAccessCode", l.reserved_server_access_code.as_str())
            .set("callId", l.call_id.as_str())
            .set("referralPage", l.referral_page.as_str())
            .set("launchData", l.launch_data.as_str())
            .set("joinAttemptId", l.join_attempt_id.as_str())
            .set("joinAttemptOrigin", l.join_attempt_origin.as_str())
            .set("isoContext", l.iso_context.as_str())
            .set("gameJoinContext", l.game_join_context.as_str())
            .set("eventId", l.event_id.as_str())
            .set("gameIdToExclude", l.game_id_to_exclude.as_str());
        let r = jcall!(
            GL,
            "nativeAppBridgeV2StartGameWithParam",
            "(Lcom/roblox/engine/jni/autovalue/StartGameParams;)I",
            fn(JObject) -> i32,
            local(&p)
        );
        log::info!("start game -> {r:?}");
        jcall!(REPORTING, "gameForegrounded", "()V", fn(),);
        jcall!(GL, "nativeOnFragmentStart", "()V", fn(),);
    });
}

/// Hand the resized surface to the running game.
pub fn update_surface() {
    let Some(window) = lucid_android::window::window() else {
        return;
    };
    with_frame(|| {
        let surface = new_instance("android/view/Surface");
        let pp = startup::platform_params(window);
        jcall!(
            GL,
            "nativeAppBridgeV2UpdateSurfaceGameWithPlatformParams",
            "(Landroid/view/Surface;Lcom/roblox/engine/jni/model/PlatformParams;Landroid/app/Activity;)V",
            fn(JObject, JObject, JObject),
            local(&surface),
            local(&pp),
            std::ptr::null_mut()
        );
    });
}

/// The engine finished leaving a game: bring the app shell back.
pub fn on_left() {
    if !IN_GAME.swap(false, Ordering::AcqRel) {
        return;
    }
    crate::presence::set(crate::presence::State::Browsing);
    // Runs on its own thread: we are inside an engine callback here.
    std::thread::spawn(|| {
        with_frame(|| {
            jcall!(GL, "nativeOnFragmentStop", "()V", fn(),);
            jcall!(REPORTING, "gameBackgrounded", "()V", fn(),);
        });
        if let Some(window) = lucid_android::window::window() {
            startup::start_app_surface(window);
        }
    });
}

/// Listen for launch requests from the Lua app.
pub fn subscribe() {
    let vm = vm();
    vm.set_super(CALLBACK_IMPL, RAW_CALLBACK);
    for class in [CALLBACK_IMPL, RAW_CALLBACK] {
        vm.on(class, "run", "(Ljava/lang/String;)V", |_, a| {
            let json = a[0].as_str().unwrap_or("").to_string();
            log::debug!("launch request: {json}");
            match Launch::from_json(&json) {
                Some(l) => {
                    std::thread::spawn(move || launch(l));
                }
                None => log::warn!("unparseable launch request: {json}"),
            }
        });
    }
    vm.on(CONNECTION, "<init>", "(J)V", |this, a| {
        if let Some(i) = this.and_then(|t| t.as_instance()) {
            i.set("f9884a", a[0].as_i64());
        }
    });

    with_frame(|| {
        let id = jcall!(
            "com/roblox/universalapp/experience/JNIExperienceProtocol",
            "getLaunchId",
            "()Ljava/lang/String;",
            fn() -> JObject,
        );
        let Some(id) = id.filter(|p| !p.is_null()) else {
            log::error!("no launch message id; joining games will not work");
            return;
        };
        let bus = new_instance(BUS);
        let callback = new_instance(CALLBACK_IMPL);
        let conn = jcall_on!(
            local(&bus);
            BUS,
            "doSubscribeRaw",
            "(Ljava/lang/String;Lcom/roblox/universalapp/messagebus/RawCallback;Z)Lcom/roblox/universalapp/messagebus/Connection;",
            fn(JObject, JObject, u8) -> JObject,
            id,
            local(&callback),
            0
        );
        let mut keep = KEEP.lock().unwrap();
        keep.push(bus);
        keep.push(callback);
        if let Some(c) = conn.and_then(|c| unsafe { from_raw(c) }) {
            keep.push(c);
            log::debug!("subscribed to launch requests");
        }
    });
}
