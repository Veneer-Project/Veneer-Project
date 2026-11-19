//! Discord Rich Presence: shows whether the user is browsing or in an
//! experience. Opt-in: set `LUCID_DISCORD_CLIENT_ID` to a Discord
//! application ID (the project does not ship one yet).

use std::sync::OnceLock;
use std::sync::mpsc::{Sender, channel};

use lucid_discord::{Activity, Client, now};

pub enum State {
    Browsing,
    InGame { place_id: i64 },
}

static SENDER: OnceLock<Sender<State>> = OnceLock::new();

/// Look the experience's public name up from its place id.
fn game_name(place_id: i64) -> Option<String> {
    let get = |url: String| -> Option<serde_json::Value> {
        let body = ureq::get(&url)
            .timeout(std::time::Duration::from_secs(10))
            .call()
            .ok()?
            .into_string()
            .ok()?;
        serde_json::from_str(&body).ok()
    };
    let universe = get(format!(
        "https://apis.roblox.com/universes/v1/places/{place_id}/universe"
    ))?
    .get("universeId")?
    .as_i64()?;
    get(format!("https://games.roblox.com/v1/games?universeIds={universe}"))?
        .get("data")?
        .get(0)?
        .get("name")?
        .as_str()
        .map(str::to_string)
}

fn activity(state: &State) -> Activity {
    match state {
        State::Browsing => Activity {
            details: Some("In the app".into()),
            start: Some(now()),
            ..Activity::default()
        },
        State::InGame { place_id } => Activity {
            details: Some(game_name(*place_id).unwrap_or_else(|| "In an experience".into())),
            state: Some("Playing".into()),
            start: Some(now()),
            buttons: vec![(
                "View experience".into(),
                format!("https://www.roblox.com/games/{place_id}"),
            )],
            ..Activity::default()
        },
    }
}

/// Start the presence thread if a client id is configured.
pub fn start() {
    let Ok(client_id) = std::env::var("LUCID_DISCORD_CLIENT_ID") else {
        return;
    };
    let (tx, rx) = channel::<State>();
    if SENDER.set(tx).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let mut client: Option<Client> = None;
        for state in rx {
            // (Re)connect lazily: Discord may start or restart at any time.
            if client.is_none() {
                client = Client::connect(&client_id)
                    .inspect_err(|e| log::debug!("discord not reachable: {e}"))
                    .ok();
            }
            if let Some(c) = client.as_mut() {
                if let Err(e) = c.set_activity(Some(&activity(&state))) {
                    log::debug!("discord presence update failed: {e}");
                    client = None;
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
