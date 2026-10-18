//! Simulation persistence: `snapshotState()` / `restore()`.
//!
//! The on-the-wire/Redis shape is byte-compatible with the reference
//! implementation (snake_case keys, PHP `JSON_PRESERVE_ZERO_FRACTION`), so a
//! state written by an older process can still be restored.

use serde_json::{Map, Value, json};

use crate::config::SimulationConfig;
use crate::sim::items::ItemState;
use crate::sim::player::PlayerState;
use crate::sim::round_phase::RoundPhase;
use crate::sim::script::{Marker, ScriptEngine};
use crate::sim::snapshot::json_float;
use crate::sim::world::World;

impl<S: ScriptEngine> World<S> {
    /// Serializes the world into a plain JSON object suitable for Redis.
    pub fn snapshot_state(&self) -> Value {
        let players: Vec<Value> = self
            .players()
            .iter()
            .map(|player| {
                let mut row = Map::new();
                row.insert("id".into(), Value::from(player.id.clone()));
                row.insert("name".into(), Value::from(player.name.clone()));
                row.insert(
                    "team".into(),
                    player.team.clone().map(Value::from).unwrap_or(Value::Null),
                );
                row.insert(
                    "user_id".into(),
                    player.user_id.map(Value::from).unwrap_or(Value::Null),
                );
                row.insert("joined_at_tick".into(), Value::from(player.joined_at_tick));
                row.insert("x".into(), json_float(player.x));
                row.insert("y".into(), json_float(player.y));
                row.insert("max_hp".into(), Value::from(player.max_hp));
                row.insert("hp".into(), Value::from(player.hp));
                row.insert("is_alive".into(), Value::from(player.is_alive));
                row.insert(
                    "respawn_at_tick".into(),
                    Value::from(player.respawn_at_tick),
                );
                row.insert("score".into(), Value::from(player.score));
                row.insert("deaths".into(), Value::from(player.deaths));
                row.insert(
                    "acked_input_sequence".into(),
                    Value::from(player.acked_input_sequence),
                );
                Value::Object(row)
            })
            .collect();

        let markers: Vec<Value> = self
            .markers()
            .iter()
            .map(|marker| {
                json!({
                    "id": marker.id,
                    "x": json_float(marker.x),
                    "y": json_float(marker.y),
                    "label": marker.label,
                    "kind": marker.kind,
                })
            })
            .collect();

        json!({
            "match_id": self.match_id,
            "tick": self.tick,
            "phase": self.phase.as_str(),
            "round_number": self.round_number,
            "round_id": self.round_id(),
            "winner": self.winner,
            "countdown_ends_at_tick": self.countdown_ends_at_tick(),
            "round_ends_at_tick": self.round_ends_at_tick(),
            "finished_at_tick": self.finished_at_tick(),
            "team_count": self.team_count(),
            "teams": Value::Object(self.teams_map()),
            "players": players,
            "markers": markers,
            "next_marker_id": self.next_marker_id(),
        })
    }

    /// Rebuilds a world from its persisted state. Players are re-added without
    /// emitting join events.
    pub fn restore(match_id: &str, config: SimulationConfig, state: &Value) -> Self {
        let mut world = Self::with_teams(match_id, config, 0, integer(state, "team_count", 2));
        world.restore_from(state);

        // The persisted tick is authoritative; `with_teams` also wrote it, but
        // restoring must not depend on the state carrying it.
        world.tick = integer(state, "tick", 0);
        world
    }

    /// Overlays a persisted state on an existing world (used by restore).
    pub fn restore_from(&mut self, state: &Value) {
        self.phase = RoundPhase::parse(string(state, "phase").unwrap_or("lobby"));
        self.round_number = integer(state, "round_number", 0);
        self.round_id = string(state, "round_id").map(str::to_owned);
        self.winner = string(state, "winner").map(str::to_owned);
        self.countdown_ends_at_tick = integer(state, "countdown_ends_at_tick", 0);
        self.round_ends_at_tick = integer(state, "round_ends_at_tick", 0);
        self.finished_at_tick = integer(state, "finished_at_tick", 0);

        if let Some(teams) = state.get("teams").and_then(Value::as_object) {
            for (player_id, team) in teams {
                if let Some(team) = team.as_str() {
                    self.teams.insert(player_id.clone(), team.to_owned());
                }
            }
        }

        for marker in state
            .get("markers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (Some(id), Some(x), Some(y)) = (
                marker.get("id").and_then(Value::as_i64),
                marker.get("x").and_then(Value::as_f64),
                marker.get("y").and_then(Value::as_f64),
            ) else {
                continue;
            };

            self.markers.push(Marker {
                id,
                x,
                y,
                label: marker
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                kind: marker
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("marker")
                    .to_owned(),
            });
        }

        self.markers_observe(integer(state, "next_marker_id", 1) - 1);

        let mut restored_players = Vec::new();
        for row in state
            .get("players")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };

            let max_hp = integer(row, "max_hp", 100);
            let mut player = PlayerState::new(
                id,
                row.get("name").and_then(Value::as_str).unwrap_or(""),
                row.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                row.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                max_hp,
            );
            player.team = row.get("team").and_then(Value::as_str).map(str::to_owned);
            player.user_id = row.get("user_id").and_then(Value::as_i64);
            player.joined_at_tick = integer(row, "joined_at_tick", 0);
            player.score = integer(row, "score", 0);
            player.deaths = integer(row, "deaths", 0);
            player.hp = integer(row, "hp", max_hp);
            player.is_alive = row.get("is_alive").and_then(Value::as_bool).unwrap_or(true);
            player.respawn_at_tick = integer(row, "respawn_at_tick", 0);
            player.acked_input_sequence = integer(row, "acked_input_sequence", 0);
            restored_players.push(player);
        }

        self.replace_players(restored_players);
    }

    /// Replaces the item list (tests and restore).
    pub fn replace_items(&mut self, items: Vec<ItemState>) {
        self.items = items;
    }
}

fn string<'a>(state: &'a Value, key: &str) -> Option<&'a str> {
    state.get(key).and_then(Value::as_str)
}

fn integer(state: &Value, key: &str, default: i64) -> i64 {
    state.get(key).and_then(Value::as_i64).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GameplayConfig;

    fn config() -> SimulationConfig {
        SimulationConfig::from_parts(60, 20, 8, 30, GameplayConfig::default())
    }

    #[test]
    fn state_round_trips_through_restore() {
        let mut world = World::<crate::lua::runtime::LuaScript>::new("m-test", config());
        assert!(world.add_player("p-1", "Alice", Some(7)));

        for _ in 0..600 {
            if world.phase == RoundPhase::Running {
                break;
            }
            world.advance();
        }
        assert_eq!(world.phase, RoundPhase::Running);

        let state = world.snapshot_state();
        let mut restored =
            World::<crate::lua::runtime::LuaScript>::restore("m-test", config(), &state);

        assert_eq!(restored.tick, world.tick);
        assert_eq!(restored.phase, world.phase);
        assert_eq!(restored.round_number, world.round_number);
        assert_eq!(restored.player("p-1").unwrap().name, "Alice");
        assert_eq!(restored.player("p-1").unwrap().user_id, Some(7));
        assert!(
            restored.drain_events().is_empty(),
            "restore must not replay join events"
        );
        assert_eq!(state["next_marker_id"], json!(1));
    }
}
