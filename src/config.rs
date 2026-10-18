//! Simulation configuration.
//!
//! Immutable, built once per process from environment and CLI so the
//! simulation itself never touches the environment, the clock or the database.
//! Defaults follow the game's established rules (world size, speeds, timings).

use std::fmt;

use serde::{Deserialize, Serialize};

/// World geometry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WorldConfig {
    pub width: f64,
    pub height: f64,
}

/// Player movement and durability.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlayerConfig {
    pub max_hp: i64,
    pub radius: f64,
    pub move_speed: f64,
    pub dash_speed: f64,
    pub dash_duration_ms: i64,
    pub dash_cooldown_ms: i64,
    pub jump_duration_ms: i64,
    pub jump_cooldown_ms: i64,
}

/// Attack rules. Damage and score are integers on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CombatConfig {
    pub attack_cooldown_ms: i64,
    pub attack_range: f64,
    pub attack_arc_degrees: f64,
    pub damage: i64,
    pub score_per_kill: i64,
}

/// Round lifecycle timings and team layout.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoundConfig {
    pub duration_ms: i64,
    pub countdown_ms: i64,
    pub finished_ms: i64,
    pub min_players: i64,
    pub teams: Vec<String>,
    pub respawn_ms: i64,
}

/// Pickups.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ItemsConfig {
    pub respawn_ms: i64,
    pub pickup_radius: f64,
    pub spawn_count: i64,
    pub heal_amount: i64,
    pub speed_boost_ms: i64,
    pub speed_multiplier: f64,
}

/// The `gameplay` block: everything the world needs, nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GameplayConfig {
    pub world: WorldConfig,
    pub player: PlayerConfig,
    pub combat: CombatConfig,
    pub round: RoundConfig,
    pub items: ItemsConfig,
}

impl Default for GameplayConfig {
    fn default() -> Self {
        Self {
            world: WorldConfig {
                width: 3200.0,
                height: 1800.0,
            },
            player: PlayerConfig {
                max_hp: 100,
                radius: 14.0,
                move_speed: 220.0,
                dash_speed: 640.0,
                dash_duration_ms: 140,
                dash_cooldown_ms: 1400,
                jump_duration_ms: 320,
                jump_cooldown_ms: 450,
            },
            combat: CombatConfig {
                attack_cooldown_ms: 420,
                attack_range: 64.0,
                attack_arc_degrees: 110.0,
                damage: 12,
                score_per_kill: 1,
            },
            round: RoundConfig {
                duration_ms: 180_000,
                countdown_ms: 3_000,
                finished_ms: 8_000,
                min_players: 1,
                teams: vec!["red".to_owned(), "blue".to_owned()],
                respawn_ms: 2_500,
            },
            items: ItemsConfig {
                respawn_ms: 12_000,
                pickup_radius: 28.0,
                spawn_count: 8,
                heal_amount: 25,
                speed_boost_ms: 6_000,
                speed_multiplier: 1.35,
            },
        }
    }
}

impl GameplayConfig {
    /// The geometry-only config used for upload validation (`World::inert`
    /// in the reference implementation): a small world, no items, no countdown.
    pub fn inert() -> Self {
        Self {
            world: WorldConfig {
                width: 960.0,
                height: 540.0,
            },
            round: RoundConfig {
                duration_ms: 60_000,
                countdown_ms: 0,
                ..GameplayConfig::default().round
            },
            items: ItemsConfig {
                spawn_count: 0,
                ..GameplayConfig::default().items
            },
            ..GameplayConfig::default()
        }
    }
}

/// Everything the simulation needs about timing and limits.
#[derive(Debug, Clone, PartialEq)]
pub struct SimulationConfig {
    pub tick_rate: i64,
    pub snapshot_rate: i64,
    pub max_players: i64,
    pub max_catchup_ticks: i64,
    pub tick_duration_ms: f64,
    pub tick_duration_seconds: f64,
    pub ticks_per_snapshot: i64,
    pub gameplay: GameplayConfig,
}

impl SimulationConfig {
    /// Mirrors `SimulationConfig::fromArray`: clamped tick/snapshot rates,
    /// player cap and derived tick duration / snapshot divisor.
    pub fn from_parts(
        tick_rate: i64,
        snapshot_rate: i64,
        max_players: i64,
        max_catchup_ticks: i64,
        gameplay: GameplayConfig,
    ) -> Self {
        let tick_rate = tick_rate.clamp(1, 240);
        let snapshot_rate = snapshot_rate.clamp(1, tick_rate);

        Self {
            tick_rate,
            snapshot_rate,
            max_players: max_players.clamp(1, 512),
            max_catchup_ticks: max_catchup_ticks.max(1),
            tick_duration_ms: 1000.0 / tick_rate as f64,
            tick_duration_seconds: 1.0 / tick_rate as f64,
            // At least one tick per snapshot; rounded like the reference.
            ticks_per_snapshot: ((tick_rate as f64 / snapshot_rate as f64).round() as i64).max(1),
            gameplay,
        }
    }
}

impl Default for SimulationConfig {
    fn default() -> Self {
        Self::from_parts(60, 20, 512, 30, GameplayConfig::default())
    }
}

/// Realtime / protocol knobs that sit next to the gameplay config in
/// (protocol version, matchmaking names, timeouts).
#[derive(Debug, Clone, PartialEq)]
pub struct RealtimeConfig {
    pub protocol_version: i64,
    pub default_match: String,
    pub game_mode: String,
    pub idle_timeout_ms: i64,
    pub flush_interval_ms: i64,
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            protocol_version: 1,
            default_match: "lan".to_owned(),
            game_mode: "deathmatch".to_owned(),
            idle_timeout_ms: 10_000,
            flush_interval_ms: 5_000,
        }
    }
}

/// Lua mode budgets, ported from `lanbash.game_modes`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModeConfig {
    pub max_script_bytes: usize,
    pub tick_timeout_ms: u64,
    pub validation_timeout_ms: u64,
    pub allow_publish: bool,
    pub memory_limit_bytes: usize,
    pub cache_seconds: u64,
}

impl Default for ModeConfig {
    fn default() -> Self {
        Self {
            max_script_bytes: 65_536,
            tick_timeout_ms: 20,
            validation_timeout_ms: 750,
            allow_publish: true,
            // 8 MiB is far above anything a builder sample needs and keeps a
            // hostile script from exhausting the process heap.
            memory_limit_bytes: 8 * 1024 * 1024,
            cache_seconds: 60,
        }
    }
}

/// Optional deterministic Lua randomness: when set, `lb.random` is drawn from
/// a `ChaCha8Rng` seeded per match instead of the shared PRNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LuaRandomness {
    /// Per-process generator (the reference implementation's `mt_rand`).
    #[default]
    Shared,
    /// Deterministic stream, seeded from the match id.
    Deterministic,
}

/// Display helper for log lines, never used for logic.
impl fmt::Display for SimulationConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Hz (snapshot {} Hz, {} players max)",
            self.tick_rate, self.snapshot_rate, self.max_players
        )
    }
}
