//! Something memorable happened during a tick.
//!
//! Events are queued by the world and drained once per tick by the game loop,
//! which turns them into `event` frames. They are neutral value objects so the
//! simulation never depends on the transport.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Position carried by hit events. Serialized as an f64 (never an integer) so
/// `10.0` stays `10.0` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SimulationEvent {
    PlayerJoined {
        player_id: String,
        name: String,
    },
    PlayerLeft {
        player_id: String,
    },
    PlayerHit {
        attacker: String,
        victim: String,
        damage: i64,
        remaining_hp: i64,
        position: Point,
    },
    PlayerDied {
        player_id: String,
        killer: String,
    },
    ItemPicked {
        player_id: String,
        item_id: String,
    },
    /// A combat kill awarded a point.
    ScoreChanged {
        player_id: String,
        score: i64,
        team: Option<String>,
    },
    /// A mode script changed a player's score (`add_score`/`set_score`).
    ScoreSet {
        player_id: String,
        score: i64,
        team: Option<String>,
    },
    /// A mode script asked clients to discard its console (`lb.log_clear()`).
    ModeLogClear,
}

impl SimulationEvent {
    /// Wire name (`event`).
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::PlayerJoined { .. } => "player_joined",
            Self::PlayerLeft { .. } => "player_left",
            Self::PlayerHit { .. } => "player_hit",
            Self::PlayerDied { .. } => "player_died",
            Self::ItemPicked { .. } => "item_picked",
            Self::ScoreChanged { .. } | Self::ScoreSet { .. } => "score_changed",
            Self::ModeLogClear => "mode_log_clear",
        }
    }

    /// Variant name, used by tests and diagnostics.
    pub fn variant(&self) -> &'static str {
        match self {
            Self::PlayerJoined { .. } => "PlayerJoined",
            Self::PlayerLeft { .. } => "PlayerLeft",
            Self::PlayerHit { .. } => "PlayerHit",
            Self::PlayerDied { .. } => "PlayerDied",
            Self::ItemPicked { .. } => "ItemPicked",
            Self::ScoreChanged { .. } => "ScoreChanged",
            Self::ScoreSet { .. } => "ScoreSet",
            Self::ModeLogClear => "ModeLogClear",
        }
    }

    /// The `event` frame body: `{"type":"event","event":…,…payload}` with the
    /// exact field order of the reference implementation.
    pub fn to_frame(&self) -> Map<String, Value> {
        let mut frame = Map::new();
        frame.insert("type".into(), Value::from("event"));
        frame.insert("event".into(), Value::from(self.event_type()));

        match self {
            Self::PlayerJoined { player_id, name } => {
                frame.insert("player_id".into(), Value::from(player_id.clone()));
                frame.insert("name".into(), Value::from(name.clone()));
            }
            Self::PlayerLeft { player_id } => {
                frame.insert("player_id".into(), Value::from(player_id.clone()));
            }
            Self::PlayerHit {
                attacker,
                victim,
                damage,
                remaining_hp,
                position,
            } => {
                frame.insert("attacker".into(), Value::from(attacker.clone()));
                frame.insert("victim".into(), Value::from(victim.clone()));
                frame.insert("damage".into(), Value::from(*damage));
                frame.insert("remaining_hp".into(), Value::from(*remaining_hp));
                frame.insert(
                    "position".into(),
                    json!({ "x": position.x, "y": position.y }),
                );
            }
            Self::PlayerDied { player_id, killer } => {
                frame.insert("player_id".into(), Value::from(player_id.clone()));
                frame.insert("killer".into(), Value::from(killer.clone()));
            }
            Self::ItemPicked { player_id, item_id } => {
                frame.insert("player_id".into(), Value::from(player_id.clone()));
                frame.insert("item_id".into(), Value::from(item_id.clone()));
            }
            Self::ScoreChanged {
                player_id,
                score,
                team,
            }
            | Self::ScoreSet {
                player_id,
                score,
                team,
            } => {
                frame.insert("player_id".into(), Value::from(player_id.clone()));
                frame.insert("score".into(), Value::from(*score));
                // `team` is omitted entirely for free-for-all modes.
                if let Some(team) = team {
                    frame.insert("team".into(), Value::from(team.clone()));
                }
            }
            Self::ModeLogClear => {}
        }

        frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_json(event: &SimulationEvent) -> String {
        serde_json::to_string(&Value::Object(event.to_frame())).unwrap()
    }

    #[test]
    fn player_hit_keeps_wire_field_order_and_float_positions() {
        let event = SimulationEvent::PlayerHit {
            attacker: "p-1".into(),
            victim: "p-2".into(),
            damage: 12,
            remaining_hp: 88,
            position: Point::new(140.0, 92.0),
        };

        assert_eq!(
            to_json(&event),
            r#"{"type":"event","event":"player_hit","attacker":"p-1","victim":"p-2","damage":12,"remaining_hp":88,"position":{"x":140.0,"y":92.0}}"#
        );
        assert_eq!(event.variant(), "PlayerHit");
    }

    #[test]
    fn score_events_omit_the_team_in_free_for_all() {
        let event = SimulationEvent::ScoreSet {
            player_id: "p-1".into(),
            score: 4,
            team: None,
        };
        assert_eq!(
            to_json(&event),
            r#"{"type":"event","event":"score_changed","player_id":"p-1","score":4}"#
        );

        let event = SimulationEvent::ScoreChanged {
            player_id: "p-1".into(),
            score: 4,
            team: Some("red".into()),
        };
        assert_eq!(
            to_json(&event),
            r#"{"type":"event","event":"score_changed","player_id":"p-1","score":4,"team":"red"}"#
        );
    }

    #[test]
    fn player_died_names_the_killer() {
        let event = SimulationEvent::PlayerDied {
            player_id: "p-2".into(),
            killer: "mode".into(),
        };
        assert_eq!(
            to_json(&event),
            r#"{"type":"event","event":"player_died","player_id":"p-2","killer":"mode"}"#
        );
    }
}
