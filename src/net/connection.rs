//! Per-connection state.

use crate::net::service::GameIdentity;

/// One realtime connection.
///
/// The transport hands over an opaque connection id; this type holds what the
/// simulation needs to know about the human behind it. Once `bind` ran, the
/// connection owns a player inside a world.
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionState {
    pub id: String,
    pub connected_at: i64,
    pub session_id: String,
    pub tick_rate: i64,
    pub protocol_version: i64,
    pub match_id: Option<String>,
    pub player_id: Option<String>,
    pub identity: Option<GameIdentity>,
    pub handshake_completed: bool,
    pub closed: bool,
    /// Whether the client was told about the join and the round yet.
    pub world_state_sent: bool,
    pub last_activity_at: i64,
    /// Frames that failed decoding (garbage must not hold a slot forever).
    pub invalid_frames: u32,
    /// The player replaced an existing session of the same identity.
    pub was_reconnect: bool,
    /// Inputs dropped because one was already accepted for that tick.
    pub coalesced_inputs: u64,
}

impl ConnectionState {
    pub fn new(
        id: &str,
        now: i64,
        session_id: &str,
        tick_rate: i64,
        protocol_version: i64,
    ) -> Self {
        Self {
            id: id.to_owned(),
            connected_at: now,
            session_id: session_id.to_owned(),
            tick_rate,
            protocol_version,
            match_id: None,
            player_id: None,
            identity: None,
            handshake_completed: false,
            closed: false,
            world_state_sent: false,
            last_activity_at: now,
            invalid_frames: 0,
            was_reconnect: false,
            coalesced_inputs: 0,
        }
    }

    pub fn touch(&mut self, now: i64) {
        self.last_activity_at = now;
    }

    pub fn bind(
        &mut self,
        match_id: &str,
        player_id: &str,
        identity: GameIdentity,
        was_reconnect: bool,
    ) {
        self.handshake_completed = true;
        self.match_id = Some(match_id.to_owned());
        self.player_id = Some(player_id.to_owned());
        self.identity = Some(identity);
        self.was_reconnect = was_reconnect;
    }

    pub fn is_idle(&self, now: i64, idle_timeout_ms: i64) -> bool {
        now - self.last_activity_at > idle_timeout_ms
    }
}

/// How the game loop reaches sockets (in-process queues, or a daemon bridge).
pub trait Transport: Send + Sync {
    fn send(&self, connection: &str, frame: &serde_json::Value);

    /// Sends already-serialized JSON (the snapshot hot path).
    ///
    /// The default re-parses into a `Value`; transports may override it.
    fn send_json(&self, connection: &str, json: &str) {
        if let Ok(value) = serde_json::from_str(json) {
            self.send(connection, &value);
        }
    }

    fn close(&self, connection: &str);
}

/// The in-process transport: queues frames per connection.
#[derive(Default)]
pub struct QueueTransport {
    frames: std::sync::Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>,
    closed: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl QueueTransport {
    pub fn take(&self, connection: &str) -> Vec<serde_json::Value> {
        self.frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(connection)
            .unwrap_or_default()
    }

    pub fn has_frames(&self, connection: &str) -> bool {
        self.frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(connection)
            .is_some_and(|frames| !frames.is_empty())
    }
}

impl Transport for QueueTransport {
    fn send(&self, connection: &str, frame: &serde_json::Value) {
        self.frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(connection.to_owned())
            .or_default()
            .push(frame.clone());
    }

    fn close(&self, connection: &str) {
        self.closed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(connection.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_uses_a_strict_timeout() {
        let connection = ConnectionState::new("c1", 1_000, "s-1", 60, 1);
        assert!(!connection.is_idle(10_999, 10_000));
        assert!(connection.is_idle(11_001, 10_000));
    }
}
