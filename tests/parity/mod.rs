//! Fixture model for the reference-parity tests.
//!
//! `tests/parity/fixtures/*.json` were produced by exporting the former PHP
//! server's simulation. Each fixture carries the input script and every
//! observable outcome per tick; `parity_test.rs` replays and compares them.

use lanverse_game::config::GameplayConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Deserialize)]
pub struct Fixture {
    pub id: String,
    pub description: String,
    pub match_id: String,
    pub config: GameplayConfig,
    pub tick_rate: i64,
    pub snapshot_rate: i64,
    pub max_players: i64,
    pub steps: Vec<Value>,
    pub frames: Vec<FixtureFrame>,
    pub wire: WireSample,
}

#[derive(Debug, Deserialize)]
pub struct WireSample {
    pub snapshot_json: String,
    pub snapshot_array: Value,
}

/// Scenario frames plus the `attach` / `restored` markers share one list.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum FixtureFrame {
    Frame(ObservedFrame),
    Attach {
        #[serde(rename = "op")]
        _op: String,
        result: AttachResultJson,
    },
    Restored {
        #[serde(rename = "op")]
        _op: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct AttachResultJson {
    pub ok: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ObservedFrame {
    pub tick: i64,
    pub phase: String,
    pub round_number: i64,
    pub round_id: Option<String>,
    pub winner: Option<String>,
    pub time_left_ms: i64,
    pub countdown_ms: i64,
    pub team_scores: Map<String, Value>,
    pub script_driven: bool,
    pub script_faulted: bool,
    pub events: Vec<ObservedEvent>,
    pub lua_logs: Vec<ObservedLog>,
    pub players: Vec<ObservedPlayer>,
    pub markers: Vec<Value>,
    pub next_marker_id: i64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct ObservedEvent {
    pub class: String,
    pub wire: Map<String, Value>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct ObservedLog {
    pub level: String,
    pub text: String,
    pub tick: i64,
}

#[derive(Debug, Deserialize)]
pub struct ObservedPlayer {
    pub id: String,
    pub name: String,
    pub team: Option<String>,
    pub user_id: Option<i64>,
    pub x: Value,
    pub y: Value,
    pub hp: i64,
    pub max_hp: i64,
    pub is_alive: bool,
    pub respawn_at_tick: i64,
    pub score: i64,
    pub deaths: i64,
    pub velocity_x: Value,
    pub velocity_y: Value,
    pub facing_x: Value,
    pub facing_y: Value,
    pub dash_until_tick: i64,
    pub dash_ready_at_tick: i64,
    pub jump_until_tick: i64,
    pub jump_ready_at_tick: i64,
    pub attack_ready_at_tick: i64,
    pub acked_input_sequence: i64,
}

/// A comparable snapshot of the world after one tick.
#[derive(Debug)]
pub struct FrameSnapshot {
    pub tick: i64,
    pub phase: String,
    pub round_number: i64,
    pub round_id: Option<String>,
    pub winner: Option<String>,
    pub time_left_ms: i64,
    pub countdown_ms: i64,
    pub team_scores: Map<String, Value>,
    pub script_driven: bool,
    pub script_faulted: bool,
    pub events: Vec<ObservedEvent>,
    pub lua_logs: Vec<ObservedLog>,
    pub players: Vec<ObservedPlayer>,
    pub markers: Vec<Value>,
    pub next_marker_id: i64,
}

impl FrameSnapshot {
    pub fn capture<S: lanverse_game::sim::script::ScriptEngine>(
        world: &mut lanverse_game::sim::world::World<S>,
        events: &[lanverse_game::sim::SimulationEvent],
    ) -> Self {
        let lua_logs = world
            .drain_lua_logs()
            .into_iter()
            .map(|entry| ObservedLog {
                level: entry.level,
                text: entry.text,
                tick: entry.tick,
            })
            .collect();

        Self {
            tick: world.tick,
            phase: world.phase.as_str().to_owned(),
            round_number: world.round_number,
            round_id: world.round_id().map(str::to_owned),
            winner: world.winner.clone(),
            time_left_ms: world.time_left_ms(),
            countdown_ms: world.countdown_ms(),
            team_scores: world.team_scores_json(),
            script_driven: world.is_script_driven(),
            script_faulted: world.script_faulted(),
            events: events.iter().map(observe_event).collect(),
            lua_logs,
            players: world.players().iter().map(observe_player).collect(),
            markers: world
                .markers()
                .iter()
                .map(|marker| {
                    json!({
                        "id": marker.id,
                        "x": lanverse_game::sim::snapshot::json_float(marker.x),
                        "y": lanverse_game::sim::snapshot::json_float(marker.y),
                        "label": marker.label,
                        "kind": marker.kind,
                    })
                })
                .collect(),
            next_marker_id: world.next_marker_id(),
        }
    }
}

fn observe_event(event: &lanverse_game::sim::SimulationEvent) -> ObservedEvent {
    ObservedEvent {
        class: event.variant().to_owned(),
        wire: event.to_frame(),
    }
}

fn observe_player(player: &lanverse_game::sim::PlayerState) -> ObservedPlayer {
    use lanverse_game::sim::snapshot::json_float;

    ObservedPlayer {
        id: player.id.clone(),
        name: player.name.clone(),
        team: player.team.clone(),
        user_id: player.user_id,
        x: json_float(player.x),
        y: json_float(player.y),
        hp: player.hp,
        max_hp: player.max_hp,
        is_alive: player.is_alive,
        respawn_at_tick: player.respawn_at_tick,
        score: player.score,
        deaths: player.deaths,
        velocity_x: json_float(player.velocity_x),
        velocity_y: json_float(player.velocity_y),
        facing_x: json_float(player.facing_x),
        facing_y: json_float(player.facing_y),
        dash_until_tick: player.dash_until_tick,
        dash_ready_at_tick: player.dash_ready_at_tick,
        jump_until_tick: player.jump_until_tick,
        jump_ready_at_tick: player.jump_ready_at_tick,
        attack_ready_at_tick: player.attack_ready_at_tick,
        acked_input_sequence: player.acked_input_sequence,
    }
}

pub fn fixtures_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parity/fixtures")
}

pub fn load_fixtures() -> Vec<Fixture> {
    let dir = fixtures_dir();
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    paths
        .iter()
        .map(|path| {
            let raw = std::fs::read_to_string(path).unwrap();
            match serde_json::from_str(&raw) {
                Ok(fixture) => fixture,
                Err(error) => {
                    // Report the offending frame so a schema drift is obvious.
                    let value: Value = serde_json::from_str(&raw).unwrap();
                    for (index, frame) in value["frames"].as_array().unwrap().iter().enumerate() {
                        let single = json!({
                            "tick": 0, "phase": "lobby", "round_number": 0, "round_id": null,
                            "winner": null, "time_left_ms": 0, "countdown_ms": 0,
                            "team_scores": {}, "script_driven": false, "script_faulted": false,
                            "events": [], "lua_logs": [], "players": [], "markers": [],
                            "next_marker_id": 1,
                        });
                        if serde_json::from_value::<ObservedFrame>(frame.clone()).is_err() {
                            eprintln!("{} frame {index}: {}", path.display(), frame);
                            eprintln!(
                                "synthetic: {:?}",
                                serde_json::from_value::<ObservedFrame>(single)
                            );
                        }
                    }
                    panic!("{}: {error}", path.display())
                }
            }
        })
        .collect()
}
