//! Web pages the app wants to show.
//!
//! On Android several screens (private servers, some settings and purchase
//! pages, help) are web pages shown in an in-app WebView. lucid has no
//! embedded browser, so those pages are opened in the user's own browser
//! and the app is told the window was closed again, instead of being left
//! on an empty screen.

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
    if let Some(id) = bus::message_id(&protocol, &open) {
        bus::subscribe(&id, move |json| {
            log::debug!("webview open request: {json}");
            let url = serde_json::from_str::<Value>(json)
                .ok()
                .and_then(|v| v.get(url_key.as_str())?.as_str().map(str::to_string));
            let closed_id = closed_id.clone();
            // Off the engine's thread: we publish back onto the bus.
            std::thread::spawn(move || {
                match url {
                    Some(url) => open_in_browser(&url),
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
    for name in ["getMutateWindowId", "getCloseWindowId"] {
        if let Some(id) = key(name).and_then(|m| bus::message_id(&protocol, &m)) {
            bus::subscribe(&id, |json| log::debug!("webview request ignored: {json}"));
        }
    }
    lucid_jni::with_frame(|| {
        crate::jcall!(PROTOCOL, "initializeAndroidWebViewProtocol", "()V", fn(),);
    });
    log::debug!("WebView protocol ready");
}
