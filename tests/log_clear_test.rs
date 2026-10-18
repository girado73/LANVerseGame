//! `lb.log_clear()`: the server tells the clients to drop the mode console.
//!
//! The lines a client shows were already broadcast, so clearing cannot be a
//! server-side buffer reset – it is an event, like a score change.

use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::lua::runtime::LuaScript;
use lanverse_game::sim::round_phase::RoundPhase;
use lanverse_game::sim::world::World;

type LuaWorld = World<LuaScript>;

fn world_with(script: &str) -> LuaWorld {
    let mut config = SimulationConfig::from_parts(60, 20, 8, 30, GameplayConfig::default());
    config.gameplay.round.countdown_ms = 0;
    config.gameplay.round.duration_ms = 60_000;

    let mut world: LuaWorld = World::new("m-log-clear", config);
    world.add_player("p-1", "Alice", Some(1));
    let result = world.attach_script(LuaScript::new("log_clear_mode", script), true);
    assert!(result.ok, "script failed to start: {:?}", result.failure);
    world
}

fn run_until_running(world: &mut LuaWorld) {
    for _ in 0..30 {
        world.advance();
        if world.phase == RoundPhase::Running {
            return;
        }
    }
    panic!("round never started");
}

#[test]
fn log_clear_is_sent_once_after_the_lines_of_the_hook() {
    let mut world = world_with(
        r#"
        local tick = 0
        function on_tick(dt)
          tick = tick + 1
          if tick == 1 then
            lb.log("Coin collected (1/10)")
            lb.log_clear()
          end
        end
        "#,
    );
    run_until_running(&mut world);
    world.drain_events();
    world.drain_lua_logs();

    // `on_tick` starts one tick after the round is running.
    let mut event = None;
    for _ in 0..4 {
        world.advance();
        if let Some(found) = world
            .drain_events()
            .into_iter()
            .find(|event| event.variant() == "ModeLogClear")
        {
            event = Some((world.tick, found));
            break;
        }
    }

    let (tick, event) = event.expect("lb.log_clear() produced an event");
    assert_eq!(tick, 2, "the clear is sent on the tick that asked");
    assert_eq!(event.event_type(), "mode_log_clear");
    assert_eq!(
        event
            .to_frame()
            .get("event")
            .and_then(serde_json::Value::as_str),
        Some("mode_log_clear")
    );

    // The line logged before the clear still reaches the client first; the
    // client applies it and then drops the console.
    let logs = world.drain_lua_logs();
    assert!(
        logs.iter().any(|log| log.text == "Coin collected (1/10)"),
        "{logs:?}"
    );
}

#[test]
fn log_clear_without_a_log_only_sends_the_event() {
    let mut world = world_with(
        r#"
        local tick = 0
        function on_tick(dt)
          tick = tick + 1
          if tick == 1 then
            lb.log_clear()
          end
        end
        "#,
    );
    run_until_running(&mut world);
    world.drain_events();

    let mut cleared = false;
    for _ in 0..4 {
        world.advance();
        cleared |= world
            .drain_events()
            .iter()
            .any(|event| event.variant() == "ModeLogClear");
    }

    assert!(cleared, "lb.log_clear() without lb.log() still clears");
}
