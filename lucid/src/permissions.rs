//! The `PermissionsProtocol` on the message bus: the Lua app asks the
//! Android side which permissions the device supports and has granted
//! (microphone for voice chat, camera, contacts, notifications, ...).
//! Without answers it assumes none, so, for example, voice chat never shows
//! a microphone.
//!
//! Requests carry `{"permissions": ["MICROPHONE_ACCESS", ...]}`; the
//! Android app (`mm/k.java` in the decompiled APK) replies on the same
//! protocol and method with a status and the permissions that are missing.
//! On a desktop there is no permission prompt: what Veneer can provide is
//! granted, the rest (camera, contacts, notifications) is missing.

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::bus;

const PROTOCOL: &str = "PermissionsProtocol";

/// Features this platform has (`SupportsPermissions`).
const SUPPORTED: &[&str] = &["MICROPHONE_ACCESS", "LOCAL_NETWORK", "WRITE_MEDIA_STORAGE"];
/// Features that count as granted. `MEDIA_PICKER` needs no permission on
/// current Android either.
const GRANTED: &[&str] = &["MICROPHONE_ACCESS", "LOCAL_NETWORK", "WRITE_MEDIA_STORAGE", "MEDIA_PICKER"];

/// The `permissions` array of a request.
fn requested(json: &str) -> Vec<String> {
    serde_json::from_str::<Value>(json)
        .ok()
        .and_then(|v| v.get("permissions")?.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| p.as_str().map(str::to_string))
        .collect()
}

/// The string map the Android app sends along: `missing_<feature>` keys
/// plus the status under `status_key`.
fn extra(status_key: &str, status: &str, listed: &[String]) -> String {
    let mut map = serde_json::Map::new();
    for p in listed {
        map.insert(format!("missing_{}", p.to_ascii_lowercase()), json!(""));
    }
    map.insert(status_key.into(), json!(status));
    Value::Object(map).to_string()
}

fn mention_microphone(features: &[String]) {
    static SAID: AtomicBool = AtomicBool::new(false);
    if features.iter().any(|f| f == "MICROPHONE_ACCESS") && !SAID.swap(true, Ordering::Relaxed) {
        log::info!("Roblox asked for the microphone: allowed (it is only opened while voice chat is on)");
    }
}

/// `HasPermissions` and `PermissionsRequest`: granted unless something
/// requested is not available here.
fn answer_status(method: &'static str, json: &str) {
    let features = requested(json);
    mention_microphone(&features);
    let missing: Vec<String> = features.iter().filter(|f| !GRANTED.contains(&f.as_str())).cloned().collect();
    let status = if missing.is_empty() { "AUTHORIZED" } else { "DENIED" };
    log::debug!("permissions {method} {features:?}: {status}");
    let response = json!({ "status": status, "missingPermissions": missing }).to_string();
    bus::publish_response(PROTOCOL, method, &response, 0, &extra("status", status, &missing));
}

/// `ShouldShowPermissionUpsell` and `ShouldShowRequestPermissionRationale`:
/// never ask the user to grant something (there is no prompt to show).
fn answer_upsell(method: &'static str, json: &str) {
    let features = requested(json);
    let response = json!({ "upsellStatus": "HIDE", "hiddenUpsellPermissions": features }).to_string();
    bus::publish_response(PROTOCOL, method, &response, 0, &extra("upsellStatus", "HIDE", &features));
}

/// Answer the permission requests from now on.
pub fn install() {
    let mut ok = bus::subscribe_requests(PROTOCOL, "SupportsPermissions", |_| {
        let response = json!({ "permissions": SUPPORTED }).to_string();
        let mut map = serde_json::Map::new();
        for p in ["CAMERA_ACCESS", "MICROPHONE_ACCESS", "LOCAL_NETWORK", "CONTACTS_ACCESS", "WRITE_MEDIA_STORAGE", "POST_NOTIFICATIONS"] {
            map.insert(p.into(), json!(""));
        }
        bus::publish_response(PROTOCOL, "SupportsPermissions", &response, 0, &Value::Object(map).to_string());
    });
    for method in ["HasPermissions", "PermissionsRequest"] {
        ok &= bus::subscribe_requests(PROTOCOL, method, move |json| answer_status(method, json));
    }
    for method in ["ShouldShowPermissionUpsell", "ShouldShowRequestPermissionRationale"] {
        ok &= bus::subscribe_requests(PROTOCOL, method, move |json| answer_upsell(method, json));
    }
    if ok {
        log::debug!("permissions protocol ready");
    } else {
        log::warn!("could not subscribe to the permissions protocol; voice chat will not see a microphone");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_extras_follow_the_android_format() {
        assert_eq!(requested(r#"{"permissions":["MICROPHONE_ACCESS","CAMERA_ACCESS"]}"#), ["MICROPHONE_ACCESS", "CAMERA_ACCESS"]);
        assert!(requested("not json").is_empty());
        let e: Value = serde_json::from_str(&extra("status", "DENIED", &["CAMERA_ACCESS".into()])).unwrap();
        assert_eq!(e, json!({ "missing_camera_access": "", "status": "DENIED" }));
    }
}
