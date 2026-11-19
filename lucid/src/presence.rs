//! Discord Rich Presence: shows whether the user is browsing or in an
//! experience. On by default when Discord is running; set `LUCID_DISCORD=0`
//! to turn it off, or `LUCID_DISCORD_CLIENT_ID` to use another application.

use std::sync::OnceLock;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};

use lucid_discord::{Activity, Client, now};

pub enum State {
    Browsing,
    InGame { place_id: i64 },
}

/// The project's Discord application.
const CLIENT_ID: &str = "1508911243266752542";
/// Art asset uploaded to that application (see `assets/logo.png`).
const LOGO: &str = "logo";

static SENDER: OnceLock<Sender<State>> = OnceLock::new();

fn get_json(url: String) -> Option<serde_json::Value> {
    let body = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    serde_json::from_str(&body).ok()
}

/// Look up an experience's public name and icon URL from its place id.
fn game_info(place_id: i64) -> (Option<String>, Option<String>) {
    let Some(universe) = get_json(format!(
        "https://apis.roblox.com/universes/v1/places/{place_id}/universe"
    ))
    .and_then(|v| v.get("universeId")?.as_i64()) else {
        return (None, None);
    };
    let name = get_json(format!("https://games.roblox.com/v1/games?universeIds={universe}"))
        .and_then(|v| Some(v.get("data")?.get(0)?.get("name")?.as_str()?.to_string()));
    let icon = get_json(format!(
        "https://thumbnails.roblox.com/v1/games/icons?universeIds={universe}&size=512x512&format=Png&isCircular=false"
    ))
    .and_then(|v| Some(v.get("data")?.get(0)?.get("imageUrl")?.as_str()?.to_string()));
    (name, icon)
}

fn activity(state: &State) -> Activity {
    match state {
        State::Browsing => Activity {
            details: Some("In the menu".into()),
            start: Some(now()),
            large_image: Some(LOGO.into()),
            large_text: Some("Roblox on Linux, via lucid".into()),
            ..Activity::default()
        },
        State::InGame { place_id } => {
            let (name, icon) = game_info(*place_id);
            // The experience's own icon when we can get it, with the logo
            // as a badge; otherwise just the logo.
            let (large, small) = match icon {
                Some(url) => (url, Some(LOGO.to_string())),
                None => (LOGO.to_string(), None),
            };
            Activity {
                details: Some(name.clone().unwrap_or_else(|| "In an experience".into())),
                state: Some("Playing".into()),
                start: Some(now()),
                large_image: Some(large),
                large_text: name,
                small_image: small,
                small_text: Some("lucid".into()),
                buttons: vec![(
                    "View experience".into(),
                    format!("https://www.roblox.com/games/{place_id}"),
                )],
            }
        }
    }
}

/// Start the presence thread (unless the user turned it off).
pub fn start() {
    if matches!(std::env::var("LUCID_DISCORD").as_deref(), Ok("0" | "off" | "false")) {
        return;
    }
    let client_id =
        std::env::var("LUCID_DISCORD_CLIENT_ID").unwrap_or_else(|_| CLIENT_ID.to_string());
    let (tx, rx) = channel::<State>();
    if SENDER.set(tx).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let mut client: Option<Client> = None;
        // What should be showing, and whether Discord has been told.
        let mut current: Option<Activity> = None;
        let mut shown = false;
        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(state) => {
                    current = Some(activity(&state));
                    shown = false;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Discord closes the socket when it restarts or reloads
            // (Ctrl+R); notice that, reconnect and show the presence again.
            if client.as_mut().is_some_and(|c| !c.is_alive()) {
                log::debug!("lost the connection to Discord");
                client = None;
                shown = false;
            }
            if client.is_none() {
                client = Client::connect(&client_id)
                    .inspect_err(|e| log::trace!("discord not reachable: {e}"))
                    .ok();
            }
            if let (Some(c), Some(a), false) = (client.as_mut(), current.as_ref(), shown) {
                match c.set_activity(Some(a)) {
                    Ok(()) => shown = true,
                    Err(e) => {
                        log::debug!("discord presence update failed: {e}");
                        client = None;
                    }
                }
            }
        }
    });
    set(State::Browsing);
}

pub fn set(state: State) {
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(state);
    }
}
