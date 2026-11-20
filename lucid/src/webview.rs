//! Web pages the app wants to show.
//!
//! On Android several screens (private servers, some settings and purchase
//! pages, help) are web pages shown in an in-app WebView that shares the
//! app's login. Here the page is shown by a helper process (`veneer webview`,
//! see `webview_helper.rs`); this module is the app side of it: it hands
//! over the session cookies, offers every link the user clicks to the
//! engine first (so "join this server" starts the game in lucid), and
//! answers the page's JavaScript bridge. Without GTK/WebKit the page opens
//! in the system browser instead.

use std::io::{BufRead, BufReader, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::bus;

const PROTOCOL: &str = "com/roblox/protocols/webview/WebViewProtocol";

/// Open a page in the user's default browser. Only `https` URLs are opened.
pub fn open_in_browser(url: &str) {
    let url = url.trim();
    if !url.starts_with("https://") || url.contains(char::is_whitespace) {
        log::warn!("not opening non-https page: {url}");
        return;
    }
    // The app sometimes asks for the same page twice in a row.
    static LAST: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if last.as_ref().is_some_and(|(u, t)| u == url && t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        *last = Some((url.to_string(), Instant::now()));
    }
    log::info!("opening in the browser: {url}");
    println!("Roblox wants to show a web page; opening it in your browser:\n  {url}");
    // The argument is passed as-is (no shell), after `--` so it cannot be
    // read as an option.
    if let Err(e) = std::process::Command::new("xdg-open").arg("--").arg(url).spawn() {
        log::warn!("could not start xdg-open ({e}); open the page manually: {url}");
    }
}

/// The older, notification-based way the app asks for a web page
/// (`OPEN_CUSTOM_WEBVIEW`, `OPEN_YOUTUBE_VIDEO`: JSON with a `url`;
/// `POST_PURCHASE`: the URL itself). Returns whether `kind` was one of them.
pub fn handle_notification(kind: &str, data: &str) -> bool {
    if !matches!(kind, "OPEN_CUSTOM_WEBVIEW" | "OPEN_YOUTUBE_VIDEO" | "POST_PURCHASE") {
        return false;
    }
    let url = serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|v| v.get("url")?.as_str().map(str::to_string))
        .unwrap_or_else(|| data.to_string());
    std::thread::spawn(move || open_in_browser(&url));
    true
}

const LINKING: &str = "com/roblox/universalapp/linking/JNILinkingProtocol";

/// The helper's stdin while a page is open.
static CURRENT: Mutex<Option<ChildStdin>> = Mutex::new(None);

fn send(line: &str) {
    if let Some(stdin) = CURRENT.lock().unwrap().as_mut() {
        let _ = writeln!(stdin, "{line}");
        let _ = stdin.flush();
    }
}

fn close_current() {
    send("CLOSE");
}

/// Offer a link the user clicked to the engine, the way the Android
/// WebView does. Returns true if the engine took it (for example a link
/// that starts a game), in which case the page must not load it.
fn engine_handles(url: &str) -> bool {
    use lucid_jni::{JObject, from_raw, with_frame};
    with_frame(|| {
        let link = crate::jcall!(
            LINKING,
            "nativeCreateInternalDlic",
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            fn(JObject, JObject) -> JObject,
            crate::java::jstr(url),
            crate::java::jstr("android_webview")
        )?;
        if link.is_null() {
            return None;
        }
        // Keep our own reference to the string across the two calls.
        let keep = unsafe { from_raw(link) }?;
        let handled = crate::jcall!(
            LINKING,
            "nativeEngineShouldHandleDlic",
            "(Ljava/lang/String;)Z",
            fn(JObject) -> u8,
            crate::java::local(&keep)
        )? != 0;
        if handled {
            crate::jcall!(
                LINKING,
                "nativeDetectDlic",
                "(Ljava/lang/String;)V",
                fn(JObject),
                crate::java::local(&keep)
            );
        }
        Some(handled)
    })
    .unwrap_or(false)
}

/// A call from the page through `__globalRobloxAndroidBridge__`.
fn handle_bridge(json: &str) {
    let Ok(v) = serde_json::from_str::<Value>(json) else {
        return;
    };
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let (module, function, callback) = (text("moduleID"), text("functionName"), text("callbackID"));
    log::debug!("webview bridge call {module}.{function}");
    let mut ok = false;
    match (module, function) {
        ("Game", "launchGame") => {
            let request = v.get("params").and_then(|p| p.get("request"));
            let place = request
                .filter(|r| r.get("requestType").and_then(|t| t.as_str()) == Some("RequestGame"))
                .and_then(|r| r.get("placeId"))
                .and_then(|p| p.as_i64().or_else(|| p.as_str()?.parse().ok()));
            if let Some(place) = place {
                ok = true;
                close_current();
                std::thread::spawn(move || crate::game::launch(crate::game::Launch::place(place)));
            }
        }
        ("Overlay", "close") => {
            ok = true;
            close_current();
        }
        _ => log::debug!("unhandled webview bridge call: {json}"),
    }
    if !callback.is_empty() && callback.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        send(&format!(
            "EVAL if (window.Roblox && window.Roblox.Hybrid && window.Roblox.Hybrid.Bridge && \
             typeof window.Roblox.Hybrid.Bridge.nativeCallback === 'function') {{ \
             window.Roblox.Hybrid.Bridge.nativeCallback('{callback}', {ok}, {{}}); }}"
        ));
    }
}

/// Show a page in the embedded browser and return when it is closed.
/// Falls back to the system browser if the embedded one is unavailable.
fn show(url: &str, title: &str) {
    if !url.starts_with("https://") {
        log::warn!("not opening non-https page: {url}");
        return;
    }
    // One page at a time.
    close_current();
    let child = std::env::current_exe().and_then(|exe| {
        Command::new(exe)
            .arg("webview")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    });
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            log::warn!("could not start the embedded browser: {e}");
            open_in_browser(url);
            return;
        }
    };
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return;
    };
    // The session cookies go through the pipe, never the command line.
    let cookies: Vec<[String; 2]> =
        crate::cookies::current().into_iter().map(|(n, v)| [n, v]).collect();
    let config = serde_json::json!({
        "url": url,
        "title": title,
        "user_agent": crate::startup::USER_AGENT.get().cloned().unwrap_or_default(),
        "cookies": cookies,
    });
    if writeln!(stdin, "{config}").is_err() {
        open_in_browser(url);
        return;
    }
    log::info!("showing web page: {url}");
    *CURRENT.lock().unwrap() = Some(stdin);

    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if let Some(rest) = line.strip_prefix("NAVIGATE ") {
            let (id, target) = rest.split_once(' ').unwrap_or((rest, ""));
            let web = target.starts_with("https://") || target.starts_with("http://");
            if engine_handles(target) {
                log::info!("the app took over a link from the web page");
                send(&format!("DENY {id}"));
                close_current();
            } else if web {
                send(&format!("ALLOW {id}"));
            } else {
                log::debug!("web page link ignored: {target}");
                send(&format!("DENY {id}"));
            }
        } else if let Some(json) = line.strip_prefix("BRIDGE ") {
            handle_bridge(json);
        } else {
            log::debug!("webview: {line}");
        }
    }
    *CURRENT.lock().unwrap() = None;
    let status = child.wait().ok().and_then(|s| s.code());
    if status == Some(crate::webview_helper::EXIT_UNAVAILABLE) {
        log::warn!("GTK 3 / WebKitGTK not found; opening the page in the system browser instead");
        open_in_browser(url);
    }
}

/// Wire up the WebView protocol on the message bus.
pub fn install() {
    let key = |name: &str| bus::static_string(PROTOCOL, name);
    let (Some(protocol), Some(open), Some(handle_close)) =
        (key("getProtocolName"), key("getOpenWindowId"), key("getHandleWindowCloseId"))
    else {
        log::warn!("WebView protocol not found in the engine; web pages will not open");
        return;
    };
    let url_key = key("getUrlKey").unwrap_or_else(|| "url".into());

    // "Is a WebView available?" Yes: we handle the pages ourselves.
    if let (Some(method), Some(available)) = (key("getIsAvailableId"), key("getAvailableKey")) {
        bus::set_request_handler(&protocol, &method, move |_| {
            serde_json::json!({ available.as_str(): true }).to_string()
        });
    }

    let closed_id = bus::message_id(&protocol, &handle_close);
    let title_key = key("getTitleKey").unwrap_or_else(|| "title".into());
    if let Some(id) = bus::message_id(&protocol, &open) {
        bus::subscribe(&id, move |json| {
            log::debug!("webview open request: {json}");
            let v: Option<Value> = serde_json::from_str(json).ok();
            let field = |k: &str| {
                v.as_ref().and_then(|v| v.get(k)?.as_str().map(str::to_string))
            };
            let (url, title) = (field(&url_key), field(&title_key).unwrap_or_default());
            let closed_id = closed_id.clone();
            // Off the engine's thread: this runs for as long as the page is open.
            std::thread::spawn(move || {
                match url {
                    Some(url) => show(&url, &title),
                    None => log::warn!("webview open request without a URL"),
                }
                // Tell the app its window is gone, so it leaves the page
                // that was waiting for it.
                if let Some(id) = closed_id {
                    bus::publish(&id, "{}");
                }
            });
        });
    }
    if let Some(id) = key("getCloseWindowId").and_then(|m| bus::message_id(&protocol, &m)) {
        bus::subscribe(&id, |_| close_current());
    }
    if let Some(id) = key("getMutateWindowId").and_then(|m| bus::message_id(&protocol, &m)) {
        bus::subscribe(&id, |json| log::debug!("webview mutate request ignored: {json}"));
    }
    lucid_jni::with_frame(|| {
        crate::jcall!(PROTOCOL, "initializeAndroidWebViewProtocol", "()V", fn(),);
    });
    log::debug!("WebView protocol ready");
    // Debug aid: LUCID_TEST_WEBVIEW=<url> shows that page 20 s after start.
    if let Ok(url) = std::env::var("LUCID_TEST_WEBVIEW") {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(20));
            show(&url, "Test");
        });
    }
}
