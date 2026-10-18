//! Wire-format snapshots.
//!
//! This is the hot path: up to 512 players, 10-20 snapshots per second. The
//! player list is assembled in a single pass.
//!
//! Two details are contractual and pinned by golden tests:
//!
//! * positions are rounded to two decimals, velocity to two, facing to three
//!   (PHP `round($x, $n)` semantics, see [`round_precision`]);
//! * floats keep their fractional part on the wire (`1.0`, never `1`), the
//!   equivalent of PHP's `JSON_PRESERVE_ZERO_FRACTION`.

use serde_json::{Map, Value, json};

use crate::sim::round_phase::RoundPhase;
use crate::sim::script::{Marker, ScriptEngine};
use crate::sim::world::World;

/// PHP `round($value, $precision)` for non-negative precisions.
pub fn round_precision(value: f64, precision: u32) -> f64 {
    if !value.is_finite() {
        return value;
    }

    // Negative zero is not produced by PHP's round(); normalize it so the JSON
    // never shows `-0.0`.
    let rounded = 10f64.powi(precision as i32);
    let scaled = (value * rounded).round() / rounded;

    if scaled == 0.0 { 0.0 } else { scaled }
}

/// A float as JSON number that keeps a fractional part (`1` -> `1.0`).
pub fn json_float(value: f64) -> Value {
    // `from_f64` refuses NaN/inf and renders integral values as `1.0`
    // (`serde_json` keeps the fraction for floats), which is exactly what PHP's
    // `JSON_PRESERVE_ZERO_FRACTION` produced.
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// The `round` block of a snapshot.
pub fn round_block<S: ScriptEngine>(world: &World<S>) -> Map<String, Value> {
    let mut block = Map::new();
    block.insert(
        "round_id".into(),
        Value::from(world.round_id().unwrap_or("r-0")),
    );
    block.insert("time_left_ms".into(), Value::from(world.time_left_ms()));
    block
}

/// Player payloads in roster order.
pub fn players<S: ScriptEngine>(world: &World<S>) -> Vec<Value> {
    world
        .players()
        .iter()
        .map(|player| {
            let mut row = Map::new();
            row.insert("id".into(), Value::from(player.id.clone()));
            row.insert("name".into(), Value::from(player.name.clone()));
            row.insert(
                "position".into(),
                json!({
                    "x": json_float(round_precision(player.x, 2)),
                    "y": json_float(round_precision(player.y, 2)),
                }),
            );
            row.insert(
                "velocity".into(),
                json!({
                    "x": json_float(round_precision(player.velocity_x, 2)),
                    "y": json_float(round_precision(player.velocity_y, 2)),
                }),
            );
            row.insert(
                "facing".into(),
                json!({
                    "x": json_float(round_precision(player.facing_x, 3)),
                    "y": json_float(round_precision(player.facing_y, 3)),
                }),
            );
            row.insert("hp".into(), Value::from(player.hp));
            row.insert("max_hp".into(), Value::from(player.max_hp));
            row.insert("is_alive".into(), Value::from(player.is_alive));
            row.insert("score".into(), Value::from(player.score));
            row.insert(
                "team".into(),
                player.team.clone().map(Value::from).unwrap_or(Value::Null),
            );
            Value::Object(row)
        })
        .collect()
}

/// Markers placed by a mode script.
pub fn markers(markers: &[Marker]) -> Vec<Value> {
    markers
        .iter()
        .map(|marker| {
            json!({
                "id": marker.id,
                "x": json_float(round_precision(marker.x, 2)),
                "y": json_float(round_precision(marker.y, 2)),
                "label": marker.label,
                "kind": marker.kind,
            })
        })
        .collect()
}

/// One complete `snapshot` frame.
pub struct SnapshotFrame {
    pub tick: i64,
    pub acked_input_sequence: i64,
    pub round: Map<String, Value>,
    pub players: Vec<Value>,
    pub markers: Vec<Value>,
}

impl SnapshotFrame {
    /// Builds a frame for one connection of `world`.
    pub fn for_world<S: ScriptEngine>(world: &World<S>, player_id: Option<&str>) -> Self {
        let acked = player_id
            .and_then(|id| world.player(id))
            .map_or(0, |player| player.acked_input_sequence);

        Self {
            tick: world.tick,
            acked_input_sequence: acked,
            round: round_block(world),
            players: players(world),
            markers: if world.is_script_driven() {
                markers(world.markers())
            } else {
                Vec::new()
            },
        }
    }

    pub fn to_value(&self) -> Value {
        let mut frame = Map::new();
        frame.insert("type".into(), Value::from("snapshot"));
        frame.insert("tick".into(), Value::from(self.tick));
        frame.insert(
            "acked_input_sequence".into(),
            Value::from(self.acked_input_sequence),
        );
        frame.insert("round".into(), Value::Object(self.round.clone()));
        frame.insert("players".into(), Value::Array(self.players.clone()));

        // Only present for Lua modes, so the Deathmatch frame stays identical.
        if !self.markers.is_empty() {
            frame.insert("markers".into(), Value::Array(self.markers.clone()));
        }

        Value::Object(frame)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.to_value()).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// `round_state` phase name for the wire.
pub fn wire_phase(phase: RoundPhase) -> &'static str {
    phase.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_matches_php_round() {
        assert_eq!(round_precision(1.234, 2), 1.23);
        assert_eq!(round_precision(1.235, 2), 1.24);
        assert_eq!(round_precision(-1.235, 2), -1.24);
        assert_eq!(round_precision(1440.0, 2), 1440.0);
        // PHP round(-0.0, 2) prints 0.0, not -0.0.
        assert!(round_precision(-0.001, 2).is_sign_positive());
        assert_eq!(round_precision(0.1234, 3), 0.123);
    }

    #[test]
    fn integral_floats_keep_their_fraction() {
        assert_eq!(json_float(1.0).to_string(), "1.0");
        assert_eq!(json_float(0.0).to_string(), "0.0");
        assert_eq!(json_float(1.5).to_string(), "1.5");
        assert_eq!(json_float(-2.0).to_string(), "-2.0");
    }
}
