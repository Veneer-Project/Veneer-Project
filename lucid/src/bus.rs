//! The engine's message bus, as seen from the "Java" side: how the Lua app
//! and the platform layer talk to each other. Payloads are JSON strings.

use std::sync::{Mutex, OnceLock};

use lucid_jni::{JObject, JValue, Ref, from_raw, new_instance, new_string, vm, with_frame};

use crate::java::{jstr, local};
use crate::{jcall, jcall_on};

const BUS: &str = "com/roblox/universalapp/messagebus/MessageBus";
const RAW_CALLBACK: &str = "com/roblox/universalapp/messagebus/RawCallback";
const CALLBACK_IMPL: &str = "com/roblox/universalapp/messagebus/MessageBus$a";
const REQUEST_HANDLER: &str = "com/roblox/universalapp/messagebus/RequestHandlerRaw";
const REQUEST_IMPL: &str = "com/roblox/universalapp/messagebus/MessageBus$b";
const CONNECTION: &str = "com/roblox/universalapp/messagebus/Connection";

type Subscriber = Box<dyn Fn(&str) + Send + Sync>;
type Responder = Box<dyn Fn(&str) -> String + Send + Sync>;

static SUBSCRIBERS: Mutex<Vec<&'static Subscriber>> = Mutex::new(Vec::new());
static RESPONDERS: Mutex<Vec<&'static Responder>> = Mutex::new(Vec::new());
/// Objects the engine holds references to for the process lifetime.
static KEEP: Mutex<Vec<Ref>> = Mutex::new(Vec::new());

fn bus() -> &'static Ref {
    static INSTANCE: OnceLock<Ref> = OnceLock::new();
    INSTANCE.get_or_init(|| new_instance(BUS))
}

/// Which registered closure a callback object stands for.
fn index_of(this: Option<&Ref>) -> Option<usize> {
    this?.as_instance()?.get("index").map(|v| v.as_i64() as usize)
}

/// Register the Java-side classes the engine calls back into.
pub fn install() {
    let vm = vm();
    vm.set_super(CALLBACK_IMPL, RAW_CALLBACK);
    vm.set_super(REQUEST_IMPL, REQUEST_HANDLER);
    for class in [CALLBACK_IMPL, RAW_CALLBACK] {
        vm.on(class, "run", "(Ljava/lang/String;)V", |this, a| {
            let handler = index_of(this).and_then(|i| SUBSCRIBERS.lock().unwrap().get(i).copied());
            if let Some(h) = handler {
                h(a[0].as_str().unwrap_or(""));
            }
        });
    }
    for class in [REQUEST_IMPL, REQUEST_HANDLER] {
        vm.on(class, "run", "(Ljava/lang/String;)Ljava/lang/String;", |this, a| {
            let handler = index_of(this).and_then(|i| RESPONDERS.lock().unwrap().get(i).copied());
            match handler {
                Some(h) => JValue::Object(Some(new_string(&h(a[0].as_str().unwrap_or(""))))),
                None => JValue::Object(Some(new_string(""))),
            }
        });
    }
    vm.on(CONNECTION, "<init>", "(J)V", |this, a| {
        if let Some(i) = this.and_then(|t| t.as_instance()) {
            i.set("f9884a", a[0].as_i64());
        }
    });
}

/// Call a static native that takes nothing and returns a string (protocol
/// names, message ids and JSON keys are exposed this way).
pub fn static_string(class: &str, name: &str) -> Option<String> {
    with_frame(|| {
        let r = jcall!(class, name, "()Ljava/lang/String;", fn() -> JObject,)?;
        unsafe { from_raw(r) }.and_then(|o| o.as_str().map(str::to_string))
    })
}

/// The bus message id for a protocol method.
pub fn message_id(protocol: &str, method: &str) -> Option<String> {
    with_frame(|| {
        let r = jcall!(
            BUS,
            "getMessageId",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            fn(JObject, JObject) -> JObject,
            jstr(protocol),
            jstr(method)
        )?;
        unsafe { from_raw(r) }.and_then(|o| o.as_str().map(str::to_string))
    })
}

/// Call `f` with the JSON payload of every message published under `id`.
/// The callback runs inside the engine: do slow work on another thread.
pub fn subscribe(id: &str, f: impl Fn(&str) + Send + Sync + 'static) -> bool {
    let index = {
        let mut subs = SUBSCRIBERS.lock().unwrap();
        subs.push(Box::leak(Box::new(Box::new(f) as Subscriber)));
        subs.len() - 1
    };
    let callback = new_instance(CALLBACK_IMPL);
    callback.as_instance().unwrap().set("index", index as i64);
    let conn = with_frame(|| {
        let c = jcall_on!(
            local(bus());
            BUS,
            "doSubscribeRaw",
            "(Ljava/lang/String;Lcom/roblox/universalapp/messagebus/RawCallback;Z)Lcom/roblox/universalapp/messagebus/Connection;",
            fn(JObject, JObject, u8) -> JObject,
            jstr(id),
            local(&callback),
            0
        )?;
        unsafe { from_raw(c) }
    });
    let mut keep = KEEP.lock().unwrap();
    keep.push(callback);
    match conn {
        Some(c) => {
            keep.push(c);
            true
        }
        None => false,
    }
}

/// Answer requests for `protocol`/`method`: `f` maps the request JSON to
/// the response JSON.
pub fn set_request_handler(
    protocol: &str,
    method: &str,
    f: impl Fn(&str) -> String + Send + Sync + 'static,
) {
    let index = {
        let mut r = RESPONDERS.lock().unwrap();
        r.push(Box::leak(Box::new(Box::new(f) as Responder)));
        r.len() - 1
    };
    let handler = new_instance(REQUEST_IMPL);
    handler.as_instance().unwrap().set("index", index as i64);
    with_frame(|| {
        jcall_on!(
            local(bus());
            BUS,
            "setRequestHandlerRaw",
            "(Ljava/lang/String;Ljava/lang/String;Lcom/roblox/universalapp/messagebus/RequestHandlerRaw;)V",
            fn(JObject, JObject, JObject),
            jstr(protocol),
            jstr(method),
            local(&handler)
        );
    });
    KEEP.lock().unwrap().push(handler);
}

/// Publish a JSON payload under a message id.
pub fn publish(id: &str, json: &str) {
    with_frame(|| {
        jcall_on!(
            local(bus());
            BUS,
            "publishRaw",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            fn(JObject, JObject),
            jstr(id),
            jstr(json)
        );
    });
}
