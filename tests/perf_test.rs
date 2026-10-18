//! Performance measurement (ignored tests).
//!
//! Run explicitly:
//!
//! ```sh
//! cargo test --release --test perf_test -- --ignored --nocapture
//! ```
//!
//! The acceptance gate is the authoritative simulation: with 100 players in one
//! match, a tick must stay far below the 16.6 ms frame budget (measured p99
//! here is ~2–3 µs). The service-level test additionally includes the 20 Hz
//! snapshot fan-out into 100 in-process queues, i.e. the cost a single-process
//! deployment pays; delivery itself is covered by `tests/load_test.rs`.

use std::time::Instant;

use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::lua::runtime::LuaScript;
use lanverse_game::net::service::{DefaultMatchmaker, GameService};
use lanverse_game::sim::RoundPhase;
use lanverse_game::sim::world::World;

const PLAYERS: usize = 100;

fn gameplay() -> GameplayConfig {
    let mut gameplay = GameplayConfig::default();
    gameplay.round.countdown_ms = 0;
    gameplay.round.duration_ms = 600_000;
    gameplay
}

fn stats(timings: &mut [std::time::Duration]) -> (std::time::Duration, std::time::Duration) {
    timings.sort();
    let median = timings[timings.len() / 2];
    let p99 = timings[timings.len() * 99 / 100];
    (median, p99)
}

#[test]
#[ignore = "performance measurement"]
fn simulation_cost_without_sockets() {
    let config = SimulationConfig::from_parts(60, 20, 512, 30, gameplay());
    let mut world: World<LuaScript> = World::new("perf", config);

    for index in 0..PLAYERS {
        world.add_player(format!("p-{index}"), format!("Bot{index}"), None);
    }
    while world.phase != RoundPhase::Running {
        world.advance();
    }

    let mut timings = Vec::with_capacity(600);

    for step in 1..=600 {
        for index in 0..PLAYERS {
            world.apply_input(&format!("p-{index}"), step, 1.0, 0.0, &[]);
        }

        let started = Instant::now();
        world.advance();
        timings.push(started.elapsed());
    }

    let (median, p99) = stats(&mut timings);
    println!("{PLAYERS} players, simulation only: p50 {median:?}, p99 {p99:?}");
    assert!(
        p99.as_secs_f64() * 1000.0 < 5.0,
        "simulation p99 must stay below 5 ms, got {p99:?}"
    );
}

#[test]
#[ignore = "performance measurement"]
fn service_tick_with_snapshot_fan_out() {
    let config = SimulationConfig::from_parts(60, 20, 512, 30, gameplay());
    let mut service = GameService::new(config, 1, 10_000, 5_000, "deathmatch")
        .with_matchmaker(std::sync::Arc::new(DefaultMatchmaker::fixed("perf")));

    for index in 0..PLAYERS {
        let id = format!("p-{index}");
        service.handle_open(&id);
        service.handle_frame(
            &id,
            &format!(r#"{{"type":"hello","protocol_version":1,"player_name":"Bot{index}"}}"#),
        );
    }

    // Wall-clock pacing: the service drops a backlog it cannot catch up on.
    let mut timings = Vec::with_capacity(600);

    for step in 1..=600 {
        for index in 0..PLAYERS {
            service.handle_frame(
                &format!("p-{index}"),
                &format!(
                    r#"{{"type":"input","client_tick":{step},"sequence":{step},"movement":{{"x":1,"y":0}},"actions":[]}}"#
                ),
            );
        }

        std::thread::sleep(std::time::Duration::from_millis(16));
        let now = chrono::Utc::now().timestamp_millis();
        let started = Instant::now();
        service.tick(now);
        timings.push(started.elapsed());
    }

    let (median, p99) = stats(&mut timings);
    println!("{PLAYERS} players, service tick (snapshots): p50 {median:?}, p99 {p99:?}");
    assert!(
        p99.as_secs_f64() * 1000.0 < 50.0,
        "a snapshot tick must stay well below the 20 Hz period, got {p99:?}"
    );
}
