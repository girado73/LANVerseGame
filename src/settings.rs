//! Configuration from the environment.
//!
//! Variable names are the ones the old server used (`LANBASH_*`), so an
//! existing deployment keeps working. Secrets (DB URL, admin key, HMAC secret)
//! never have defaults and are never logged.

use std::env;

use crate::config::{GameplayConfig, ModeConfig, RealtimeConfig, SimulationConfig};

/// Everything one process needs.
#[derive(Debug, Clone)]
pub struct Settings {
    pub bind: String,
    pub database_url: Option<String>,
    /// Former MySQL database, read by `lanverse_game migrate`.
    pub mysql_url: Option<String>,
    pub redis_url: String,
    pub simulation: SimulationConfig,
    pub realtime: RealtimeConfig,
    pub modes: ModeConfig,
    pub result_secret: Option<String>,
    pub admin_key: Option<String>,
    pub admin_bind: String,
    pub version: String,
    /// Public base URL of the control plane (printed in the admin page and the
    /// token hint).
    pub public_url: String,
}

impl Settings {
    /// Reads the environment. Unparsable numbers fall back to the defaults and
    /// are reported as warnings instead of failing the start.
    pub fn from_env() -> Self {
        let gameplay = GameplayConfig {
            world: crate::config::WorldConfig {
                width: number("LANBASH_WORLD_WIDTH", 3200.0),
                height: number("LANBASH_WORLD_HEIGHT", 1800.0),
            },
            round: crate::config::RoundConfig {
                duration_ms: integer("LANBASH_ROUND_DURATION", 180_000),
                countdown_ms: integer("LANBASH_ROUND_COUNTDOWN", 3_000),
                min_players: integer("LANBASH_MIN_PLAYERS", 1),
                ..GameplayConfig::default().round
            },
            ..GameplayConfig::default()
        };

        let simulation = SimulationConfig::from_parts(
            integer("LANBASH_TICK_RATE", 64),
            integer("LANBASH_SNAPSHOT_RATE", 64), // sollte genau gleich mit Tick_Rate sein
            integer("LANBASH_MAX_PLAYERS", 512),
            integer("LANBASH_MAX_CATCHUP_TICKS", 30),
            gameplay,
        );

        if simulation.tick_rate != simulation.snapshot_rate {
            tracing::warn!(
                "Tick rate and snapshot rate are not equal. TickRate: {}, SnapshotRate: {}",
                simulation.tick_rate,
                simulation.snapshot_rate
            );
        }

        let realtime = RealtimeConfig {
            protocol_version: integer("LANBASH_PROTOCOL_VERSION", 1),
            default_match: text("LANBASH_DEFAULT_MATCH", "lan"),
            game_mode: text("LANBASH_GAME_MODE", "deathmatch"),
            idle_timeout_ms: integer("LANBASH_IDLE_TIMEOUT", 10) * 1_000,
            flush_interval_ms: integer("LANBASH_FLUSH_INTERVAL", 5) * 1_000,
        };

        let modes = ModeConfig {
            max_script_bytes: integer("LANBASH_MODE_MAX_SCRIPT_BYTES", 65_536) as usize,
            tick_timeout_ms: integer("LANBASH_MODE_TICK_TIMEOUT", 20) as u64,
            validation_timeout_ms: integer("LANBASH_MODE_VALIDATION_TIMEOUT", 750) as u64,
            allow_publish: boolean("LANBASH_MODE_ALLOW_PUBLISH", true),
            ..ModeConfig::default()
        };

        Self {
            bind: text("LANBASH_BIND", "0.0.0.0:8080"),
            database_url: env::var("LANBASH_DATABASE_URL")
                .ok()
                .filter(|url| !url.is_empty()),
            mysql_url: env::var("LANBASH_MYSQL_URL")
                .ok()
                .filter(|url| !url.is_empty()),
            redis_url: text("LANBASH_REDIS_URL", "redis://127.0.0.1:6379/2"),
            simulation,
            realtime,
            modes,
            result_secret: secret("LANBASH_RESULT_SECRET"),
            admin_key: secret("LANBASH_ADMIN_KEY"),
            admin_bind: text("LANBASH_ADMIN_BIND", "127.0.0.1:8081"),
            version: text("LANBASH_VERSION", env!("CARGO_PKG_VERSION")),
            public_url: text("LANBASH_PUBLIC_URL", "http://127.0.0.1:8080"),
        }
    }
}

fn text(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

fn secret(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn integer(key: &str, default: i64) -> i64 {
    match env::var(key) {
        Ok(raw) => match raw.trim().parse() {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(%key, %raw, default, "ignoring unparsable numeric setting");
                default
            }
        },
        Err(_) => default,
    }
}

fn number(key: &str, default: f64) -> f64 {
    match env::var(key) {
        Ok(raw) => match raw.trim().parse() {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(%key, %raw, default, "ignoring unparsable numeric setting");
                default
            }
        },
        Err(_) => default,
    }
}

fn boolean(key: &str, default: bool) -> bool {
    match env::var(key) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_reference_server() {
        let settings = Settings::from_env();

        assert_eq!(settings.simulation.tick_rate, 64);
        assert_eq!(settings.simulation.snapshot_rate, 64);
        assert_eq!(settings.simulation.max_players, 512);
        assert_eq!(settings.simulation.gameplay.world.width, 3200.0);
        assert_eq!(settings.simulation.gameplay.round.duration_ms, 180_000);
        assert_eq!(settings.simulation.gameplay.round.countdown_ms, 3_000);
        assert_eq!(settings.realtime.idle_timeout_ms, 10_000);
        assert_eq!(settings.realtime.flush_interval_ms, 5_000);
    }

    #[test]
    fn protocol_defaults_are_stable() {
        assert_eq!(Settings::from_env().realtime.protocol_version, 1);
    }
}
