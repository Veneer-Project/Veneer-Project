//! Discord Rich Presence over the local Discord IPC socket.
//!
//! Protocol: frames of `opcode: u32 LE`, `length: u32 LE`, JSON payload.
//! Opcode 0 is the handshake, opcode 1 a command frame.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

/// What to show on the user's profile.
#[derive(Clone, Debug, Default)]
pub struct Activity {
    /// First line, e.g. the experience name.
    // (`large_image` below is an asset key uploaded to the Discord
    // application, or an `https` image URL.)
    pub details: Option<String>,
    /// Second line, e.g. "In game" / "Browsing".
    pub state: Option<String>,
    /// Unix timestamp (seconds) the activity started at.
    pub start: Option<u64>,
    pub large_image: Option<String>,
    pub large_text: Option<String>,
    /// Small badge over the large image: an asset key or an `https` URL.
    pub small_image: Option<String>,
    pub small_text: Option<String>,
    /// Up to two `(label, url)` buttons.
    pub buttons: Vec<(String, String)>,
}

pub struct Client {
    stream: UnixStream,
    nonce: u64,
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Candidate socket locations: native, Flatpak and Snap Discord clients.
fn socket_paths() -> Vec<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let mut out = Vec::new();
    for dir in [
        "",
        "app/com.discordapp.Discord",
        "app/dev.vencord.Vesktop",
        "snap.discord",
    ] {
        for i in 0..10 {
            out.push(base.join(dir).join(format!("discord-ipc-{i}")));
        }
    }
    out
}

impl Client {
    /// Connect to a running Discord client and identify as `client_id`
    /// (a Discord application ID).
    pub fn connect(client_id: &str) -> std::io::Result<Client> {
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "Discord is not running");
        for path in socket_paths() {
            match UnixStream::connect(&path) {
                Ok(stream) => {
                    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut c = Client { stream, nonce: 0 };
                    c.send(
                        OP_HANDSHAKE,
                        &format!("{{\"v\":1,\"client_id\":{}}}", json_str(client_id)),
                    )?;
                    // The reply carries the user's Discord profile: do not log it.
                    c.recv()?;
                    log::debug!("connected to Discord at {}", path.display());
                    return Ok(c);
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn send(&mut self, op: u32, payload: &str) -> std::io::Result<()> {
        let mut frame = Vec::with_capacity(8 + payload.len());
        frame.extend_from_slice(&op.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload.as_bytes());
        self.stream.write_all(&frame)
    }

    fn recv(&mut self) -> std::io::Result<String> {
        let mut header = [0u8; 8];
        self.stream.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let mut body = vec![0u8; len.min(1 << 20)];
        self.stream.read_exact(&mut body)?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Whether Discord still has its end of the socket open. Discord drops
    /// every connection when it restarts or reloads.
    pub fn is_alive(&mut self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut buf = [0u8; 512];
        let alive = loop {
            match self.stream.read(&mut buf) {
                Ok(0) => break false,
                // Unsolicited data: discard it and keep looking.
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break true,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break false,
            }
        };
        alive && self.stream.set_nonblocking(false).is_ok()
    }

    /// Set (or with `None`, clear) the rich presence.
    pub fn set_activity(&mut self, activity: Option<&Activity>) -> std::io::Result<()> {
        self.nonce += 1;
        let body = match activity {
            None => "null".to_string(),
            Some(a) => {
                let mut parts = Vec::new();
                if let Some(d) = &a.details {
                    parts.push(format!("\"details\":{}", json_str(d)));
                }
                if let Some(s) = &a.state {
                    parts.push(format!("\"state\":{}", json_str(s)));
                }
                if let Some(t) = a.start {
                    parts.push(format!("\"timestamps\":{{\"start\":{t}}}"));
                }
                let mut assets = Vec::new();
                for (key, value) in [
                    ("large_image", &a.large_image),
                    ("large_text", &a.large_text),
                    ("small_image", &a.small_image),
                    ("small_text", &a.small_text),
                ] {
                    if let Some(v) = value {
                        assets.push(format!("\"{key}\":{}", json_str(v)));
                    }
                }
                if !assets.is_empty() {
                    parts.push(format!("\"assets\":{{{}}}", assets.join(",")));
                }
                if !a.buttons.is_empty() {
                    let b: Vec<String> = a
                        .buttons
                        .iter()
                        .take(2)
                        .map(|(l, u)| format!("{{\"label\":{},\"url\":{}}}", json_str(l), json_str(u)))
                        .collect();
                    parts.push(format!("\"buttons\":[{}]", b.join(",")));
                }
                format!("{{{}}}", parts.join(","))
            }
        };
        let payload = format!(
            "{{\"cmd\":\"SET_ACTIVITY\",\"args\":{{\"pid\":{},\"activity\":{body}}},\"nonce\":\"{}\"}}",
            std::process::id(),
            self.nonce
        );
        self.send(OP_FRAME, &payload)?;
        let reply = self.recv()?;
        log::trace!("discord reply: {reply}");
        Ok(())
    }
}

/// Current Unix time in seconds, for [`Activity::start`].
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_when_discord_goes_away() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut client = Client { stream: ours, nonce: 0 };
        assert!(client.is_alive());
        // Unsolicited data must not be mistaken for a closed connection.
        (&theirs).write_all(b"event").unwrap();
        assert!(client.is_alive());
        drop(theirs);
        assert!(!client.is_alive());
    }

    #[test]
    fn activity_json_is_escaped() {
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
