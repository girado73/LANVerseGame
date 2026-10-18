//! Wire messages (protocol v1).
//!
//! One JSON object per WebSocket text frame. The field names and their order
//! are the contract with every released client; see `docs/protocol.md`.
//! Server messages are built as ordered `serde_json::Map`s so a new Rust update
//! never reorders a frame.

use serde_json::{Map, Value, json};

use crate::sim::events::SimulationEvent;
use crate::sim::script::Marker;
use crate::sim::snapshot::{SnapshotFrame, round_block};

/// Wire constant shared with the client.
pub const PROTOCOL_VERSION: i64 = 1;
/// Largest single received frame.
pub const MAX_FRAME_BYTES: usize = 65_536;
/// Highest accepted tick rate (client-side limit).
pub const MAX_TICK_RATE: i64 = 240;
/// Highest number of players in one snapshot.
pub const MAX_PLAYERS_PER_SNAPSHOT: usize = 512;
/// Highest accepted player name length.
pub const MAX_PLAYER_NAME_LENGTH: usize = 32;
/// Actions the protocol documents; anything else is ignored.
pub const KNOWN_ACTIONS: [&str; 5] = ["jump", "dash", "use_item", "interact", "emote"];

// ---------------------------------------------------------------------------
// Client -> server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Hello {
    pub protocol_version: i64,
    pub player_name: String,
    pub token: Option<String>,
    pub match_id: Option<String>,
    pub client_version: Option<String>,
}

impl Hello {
    /// The wire form (used when a hello is relayed unchanged).
    pub fn to_value(&self) -> Value {
        let mut frame = Map::new();
        frame.insert("type".into(), Value::from("hello"));
        frame.insert(
            "protocol_version".into(),
            Value::from(self.protocol_version),
        );
        frame.insert("player_name".into(), Value::from(self.player_name.clone()));

        for (key, value) in [
            ("token", &self.token),
            ("match_id", &self.match_id),
            ("client_version", &self.client_version),
        ] {
            if let Some(value) = value {
                frame.insert(key.into(), Value::from(value.clone()));
            }
        }

        Value::Object(frame)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Input {
    pub client_tick: i64,
    pub sequence: i64,
    pub movement_x: f64,
    pub movement_y: f64,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ping {
    pub client_time_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pull {
    pub what: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Invalid {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unknown {
    pub message_type: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Hello(Hello),
    Input(Input),
    Ping(Ping),
    Pull(Pull),
    Invalid(Invalid),
    /// Forward compatibility: ignored without an error.
    Unknown(Unknown),
}

/// Stateless decoder. Trusts nothing: types, ranges and lengths are clamped or
/// rejected, movement is normalized to length <= 1 and unknown actions drop.
pub fn decode(payload: &str) -> ClientMessage {
    if payload.len() > MAX_FRAME_BYTES {
        return ClientMessage::Invalid(Invalid {
            code: "frame_too_large",
            message: "Frame exceeds the maximum size.".to_owned(),
        });
    }

    let data: Value = match serde_json::from_str(payload) {
        Ok(value) => value,
        Err(_) => {
            return ClientMessage::Invalid(Invalid {
                code: "invalid_json",
                message: "Frame is not valid JSON.".to_owned(),
            });
        }
    };

    let Value::Object(data) = data else {
        return ClientMessage::Invalid(Invalid {
            code: "invalid_frame",
            message: "Frame must be a JSON object.".to_owned(),
        });
    };

    let Some(Value::String(message_type)) = data.get("type") else {
        return ClientMessage::Invalid(Invalid {
            code: "missing_type",
            message: "Frame is missing a string \"type\".".to_owned(),
        });
    };

    match message_type.as_str() {
        "hello" => decode_hello(&data),
        "input" => decode_input(&data),
        "ping" => decode_ping(&data),
        "pull" => ClientMessage::Pull(Pull {
            what: match data.get("what") {
                Some(Value::String(what)) => what.clone(),
                _ => "mode".to_owned(),
            },
        }),
        other => ClientMessage::Unknown(Unknown {
            message_type: other.to_owned(),
        }),
    }
}

fn decode_hello(data: &Map<String, Value>) -> ClientMessage {
    let Some(protocol_version) = data.get("protocol_version").and_then(Value::as_i64) else {
        return invalid("invalid_hello", "\"protocol_version\" must be an integer.");
    };

    let Some(Value::String(raw_name)) = data.get("player_name") else {
        return invalid(
            "invalid_hello",
            "\"player_name\" must be a non-empty string.",
        );
    };

    if raw_name.trim().is_empty() {
        return invalid(
            "invalid_hello",
            "\"player_name\" must be a non-empty string.",
        );
    }

    let player_name = sanitize_name(raw_name);

    if player_name.is_empty() {
        return invalid(
            "invalid_hello",
            "\"player_name\" contains only invalid characters.",
        );
    }

    ClientMessage::Hello(Hello {
        protocol_version,
        player_name,
        token: optional_string(data, "token", 512),
        match_id: optional_string(data, "match_id", 64),
        client_version: optional_string(data, "client_version", 64),
    })
}

fn decode_input(data: &Map<String, Value>) -> ClientMessage {
    let Some(sequence) = data.get("sequence").and_then(Value::as_i64) else {
        return invalid(
            "invalid_input",
            "\"sequence\" must be a non-negative integer.",
        );
    };

    if sequence < 0 {
        return invalid(
            "invalid_input",
            "\"sequence\" must be a non-negative integer.",
        );
    }

    let client_tick = data.get("client_tick").and_then(Value::as_i64).unwrap_or(0);
    let (movement_x, movement_y) = normalize_movement(data.get("movement"));

    ClientMessage::Input(Input {
        client_tick,
        sequence,
        movement_x,
        movement_y,
        actions: decode_actions(data.get("actions")),
    })
}

fn decode_ping(data: &Map<String, Value>) -> ClientMessage {
    let Some(client_time_ms) = data.get("client_time_ms").and_then(Value::as_i64) else {
        return invalid("invalid_ping", "\"client_time_ms\" must be an integer.");
    };

    ClientMessage::Ping(Ping { client_time_ms })
}

fn invalid(code: &'static str, message: &str) -> ClientMessage {
    ClientMessage::Invalid(Invalid {
        code,
        message: message.to_owned(),
    })
}

/// Clamps movement to a unit vector so a client cannot outrun the server speed
/// by inflating the vector length.
fn normalize_movement(movement: Option<&Value>) -> (f64, f64) {
    let (x, y) = match movement {
        Some(Value::Object(object)) => {
            (finite_float(object.get("x")), finite_float(object.get("y")))
        }
        // Tolerate the legacy string form.
        Some(Value::String(direction)) => match direction.as_str() {
            "up" => (0.0, -1.0),
            "down" => (0.0, 1.0),
            "left" => (-1.0, 0.0),
            "right" => (1.0, 0.0),
            _ => (0.0, 0.0),
        },
        _ => (0.0, 0.0),
    };

    let x = x.clamp(-1.0, 1.0);
    let y = y.clamp(-1.0, 1.0);
    let length_squared = x * x + y * y;

    if length_squared > 1.0 {
        let length = length_squared.sqrt();
        (x / length, y / length)
    } else {
        (x, y)
    }
}

fn decode_actions(actions: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(actions)) = actions else {
        return Vec::new();
    };

    let mut decoded: Vec<String> = Vec::new();

    for action in actions {
        let Value::String(action) = action else {
            continue;
        };

        if KNOWN_ACTIONS.contains(&action.as_str()) && !decoded.contains(action) {
            decoded.push(action.clone());
        }
    }

    decoded
}

fn finite_float(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| value.is_finite())
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

fn optional_string(data: &Map<String, Value>, key: &str, max_length: usize) -> Option<String> {
    match data.get(key) {
        Some(Value::String(value)) if !value.is_empty() => {
            Some(value.chars().take(max_length).collect())
        }
        _ => None,
    }
}

/// Strips control characters, trims and truncates to the protocol limit.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|character| !character.is_control())
        .collect();

    cleaned
        .trim()
        .chars()
        .take(MAX_PLAYER_NAME_LENGTH)
        .collect::<String>()
        .trim()
        .to_owned()
}

// ---------------------------------------------------------------------------
// Server -> client
// ---------------------------------------------------------------------------

pub fn welcome(
    protocol_version: i64,
    player_id: &str,
    tick_rate: i64,
    server_tick: i64,
    session_id: Option<&str>,
    match_id: Option<&str>,
    mode: Option<(&str, &str, &str)>,
) -> Value {
    let mut frame = Map::new();
    frame.insert("type".into(), Value::from("welcome"));
    frame.insert("protocol_version".into(), Value::from(protocol_version));
    frame.insert("player_id".into(), Value::from(player_id));

    if let Some(session_id) = session_id {
        frame.insert("session_id".into(), Value::from(session_id));
    }
    if let Some(match_id) = match_id {
        frame.insert("match_id".into(), Value::from(match_id));
    }

    frame.insert("tick_rate".into(), Value::from(tick_rate));
    frame.insert("server_tick".into(), Value::from(server_tick));

    if let Some((id, name, checksum)) = mode {
        frame.insert(
            "mode".into(),
            json!({ "id": id, "name": name, "checksum": checksum }),
        );
    }

    Value::Object(frame)
}

pub fn snapshot(frame: &SnapshotFrame) -> Value {
    frame.to_value()
}

pub fn event(event: &SimulationEvent) -> Value {
    Value::Object(event.to_frame())
}

pub fn mode_log(mode_id: &str, level: &str, message: &str) -> Value {
    json!({
        "type": "event",
        "event": "mode_log",
        "mode_id": mode_id,
        "level": level,
        "message": message,
    })
}

/// `lb.log_clear()`: drop the mode console on the client.
///
/// Additive: clients that do not know `mode_log_clear` ignore it, like any
/// unknown event.
pub fn mode_log_clear(mode_id: &str) -> Value {
    json!({
        "type": "event",
        "event": "mode_log_clear",
        "mode_id": mode_id,
    })
}

pub fn mode_error(mode_id: &str, message: &str) -> Value {
    json!({
        "type": "event",
        "event": "mode_error",
        "mode_id": mode_id,
        "message": message,
    })
}

/// `round_state` for a phase, with the optional round block first.
pub fn round_state(
    phase: &str,
    round: Option<&str>,
    round_number: i64,
    countdown_ms: i64,
    winner: Option<&str>,
) -> Value {
    let mut frame = Map::new();
    frame.insert("type".into(), Value::from("round_state"));
    frame.insert("phase".into(), Value::from(phase));

    if let Some(round) = round {
        frame.insert("round".into(), Value::from(round));
        frame.insert("round_number".into(), Value::from(round_number));
    }

    frame.insert("countdown_ms".into(), Value::from(countdown_ms));

    if let Some(winner) = winner {
        frame.insert("winner".into(), Value::from(winner));
    }

    Value::Object(frame)
}

pub fn pong(client_time_ms: i64, server_time_ms: i64) -> Value {
    json!({
        "type": "pong",
        "client_time_ms": client_time_ms,
        "server_time_ms": server_time_ms,
    })
}

pub fn error(code: &str, message: &str, fatal: bool) -> Value {
    json!({
        "type": "error",
        "code": code,
        "message": message,
        "fatal": fatal,
    })
}

/// The Lua package of a match, answering `pull`.
pub fn mode_package(package: Option<&Value>) -> Value {
    match package {
        None => json!({ "type": "mode", "mode": null }),
        Some(package) => {
            let manifest = package
                .get("manifest")
                .cloned()
                .unwrap_or_else(|| json!({}));
            json!({
                "type": "mode",
                "mode": {
                    "id": package.get("id").cloned().unwrap_or(Value::Null),
                    "name": package.get("name").cloned().unwrap_or(Value::Null),
                    "manifest": manifest,
                    "script": package.get("script").cloned().unwrap_or(Value::Null),
                    "checksum": package.get("checksum").cloned().unwrap_or(Value::Null),
                },
            })
        }
    }
}

/// Compatibility frame of the former transport: old clients ignore it, new
/// ones only need it as a marker that the socket is up, so it is sent first.
pub fn connection_established(socket_id: &str, activity_timeout: i64) -> Value {
    let data = json!({
        "socket_id": socket_id,
        "activity_timeout": activity_timeout,
    })
    .to_string();

    json!({
        "event": "pusher:connection_established",
        "data": data,
    })
}

/// Builds the snapshot frame pieces the session needs (kept for callers that
/// only have a world).
pub fn snapshot_round<S: crate::sim::script::ScriptEngine>(
    world: &crate::sim::world::World<S>,
) -> Map<String, Value> {
    round_block(world)
}

/// Markers for a snapshot (empty outside script-driven matches).
pub fn snapshot_markers(markers: &[Marker]) -> Vec<Value> {
    crate::sim::snapshot::markers(markers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_is_decoded_and_sanitized() {
        let message = decode(
            r#"{"type":"hello","protocol_version":1,"player_name":"  Ali\u0000ce  ","token":"1|abc","match_id":"coin_rush","client_version":"0.1.0"}"#,
        );

        match message {
            ClientMessage::Hello(hello) => {
                assert_eq!(hello.protocol_version, 1);
                assert_eq!(hello.player_name, "Alice");
                assert_eq!(hello.token.as_deref(), Some("1|abc"));
                assert_eq!(hello.match_id.as_deref(), Some("coin_rush"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_hello_without_version_or_name_is_invalid() {
        assert!(matches!(
            decode(r#"{"type":"hello","player_name":"A"}"#),
            ClientMessage::Invalid(Invalid {
                code: "invalid_hello",
                ..
            })
        ));
        assert!(matches!(
            decode(r#"{"type":"hello","protocol_version":1,"player_name":"   "}"#),
            ClientMessage::Invalid(Invalid {
                code: "invalid_hello",
                ..
            })
        ));
    }

    #[test]
    fn movement_is_clamped_to_unit_length() {
        let ClientMessage::Input(input) = decode(
            r#"{"type":"input","sequence":1,"movement":{"x":10,"y":10},"actions":["jump"]}"#,
        ) else {
            panic!("not an input");
        };

        let length =
            (input.movement_x * input.movement_x + input.movement_y * input.movement_y).sqrt();
        assert!((length - 1.0).abs() < 1e-12);
    }

    #[test]
    fn legacy_direction_strings_are_accepted() {
        let ClientMessage::Input(input) =
            decode(r#"{"type":"input","sequence":1,"movement":"left"}"#)
        else {
            panic!("not an input");
        };
        assert_eq!((input.movement_x, input.movement_y), (-1.0, 0.0));
    }

    #[test]
    fn unknown_and_duplicate_actions_are_dropped() {
        let ClientMessage::Input(input) =
            decode(r#"{"type":"input","sequence":1,"actions":["jump","fly","jump","dash",7]}"#)
        else {
            panic!("not an input");
        };
        assert_eq!(input.actions, vec!["jump".to_owned(), "dash".to_owned()]);
    }

    #[test]
    fn non_finite_movement_is_neutralized() {
        // JSON has no inf/nan: an overflowing literal is invalid JSON and the
        // whole frame is rejected rather than half-applied.
        assert!(matches!(
            decode(r#"{"type":"input","sequence":1,"movement":{"x":1e999,"y":-1}}"#),
            ClientMessage::Invalid(Invalid {
                code: "invalid_json",
                ..
            })
        ));

        // Values that are not numbers at all become zero intent.
        let ClientMessage::Input(input) =
            decode(r#"{"type":"input","sequence":1,"movement":{"x":null,"y":"NaN"}}"#)
        else {
            panic!("not an input");
        };
        assert_eq!((input.movement_x, input.movement_y), (0.0, 0.0));
    }

    #[test]
    fn a_negative_sequence_is_rejected() {
        assert!(matches!(
            decode(r#"{"type":"input","sequence":-1}"#),
            ClientMessage::Invalid(Invalid {
                code: "invalid_input",
                ..
            })
        ));
    }

    #[test]
    fn unknown_types_and_non_objects_are_handled() {
        assert!(matches!(
            decode(r#"{"type":"blockchain","x":1}"#),
            ClientMessage::Unknown(_)
        ));
        assert!(matches!(
            decode("[1,2,3]"),
            ClientMessage::Invalid(Invalid {
                code: "invalid_frame",
                ..
            })
        ));
        assert!(matches!(
            decode("{oops"),
            ClientMessage::Invalid(Invalid {
                code: "invalid_json",
                ..
            })
        ));
        assert!(matches!(
            decode(r#"{"nope":1}"#),
            ClientMessage::Invalid(Invalid {
                code: "missing_type",
                ..
            })
        ));
    }

    #[test]
    fn pull_defaults_to_the_mode_package() {
        assert_eq!(
            decode(r#"{"type":"pull"}"#),
            ClientMessage::Pull(Pull {
                what: "mode".to_owned()
            })
        );
    }

    #[test]
    fn frames_over_the_size_cap_are_rejected() {
        let huge = format!(
            r#"{{"type":"ping","client_time_ms":1,"pad":"{}"}}"#,
            "x".repeat(MAX_FRAME_BYTES)
        );
        assert!(matches!(
            decode(&huge),
            ClientMessage::Invalid(Invalid {
                code: "frame_too_large",
                ..
            })
        ));
    }

    #[test]
    fn welcome_field_order_is_stable() {
        let frame = welcome(1, "p-1", 60, 0, Some("s-1"), Some("m"), None);
        let keys: Vec<&str> = frame
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            vec![
                "type",
                "protocol_version",
                "player_id",
                "session_id",
                "match_id",
                "tick_rate",
                "server_tick"
            ]
        );
    }
}
