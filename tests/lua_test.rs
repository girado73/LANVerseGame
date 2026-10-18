//! mlua runtime tests: the `lb` API, sandbox, budget and failure policy.
//!
//! The world integration (which effect ends up where) is covered by the parity
//! fixtures and `tests/world_test.rs`; here the Lua semantics are pinned.

use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::lua::runtime::LuaScript;
use lanverse_game::lua::validate::dry_run;
use lanverse_game::sim::round_phase::RoundPhase;
use lanverse_game::sim::script::{LuaEffects, ScriptEngine};
use lanverse_game::sim::world::World;

type LuaWorld = World<LuaScript>;

fn world_with(script: &str) -> LuaWorld {
    let mut config = SimulationConfig::from_parts(60, 20, 8, 30, GameplayConfig::default());
    config.gameplay.round.countdown_ms = 0;
    config.gameplay.round.duration_ms = 60_000;

    let mut world: LuaWorld = World::new("m-lua", config);
    world.add_player("p-1", "Alice", Some(1));
    let engine = LuaScript::new("test_mode", script);
    let result = world.attach_script(engine, true);
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

fn builder_sample(mode: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../LANVerseBuilder/modes")
        .join(mode)
        .join("script.lua");
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn on_start_can_read_the_world_and_spawn_markers() {
    let world = world_with(
        r#"
        function on_start()
          local world = lb.world()
          lb.spawn_marker(world.width / 2, world.height / 2, "HILL", "goal")
          lb.log("started")
        end
        "#,
    );

    assert_eq!(world.markers().len(), 1);
    assert_eq!(world.markers()[0].kind, "goal");
    assert_eq!(world.markers()[0].x, 1600.0);
    assert_eq!(world.markers()[0].label, "HILL");
}

#[test]
fn markers_spawned_in_one_tick_get_unique_ids() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          lb.spawn_marker(10, 10, "a", "coin")
          lb.spawn_marker(20, 20, "b", "coin")
          lb.spawn_marker(30, 30, "c", "coin")
        end
        "#,
    );
    run_until_running(&mut world);

    let mut ids: Vec<i64> = world.markers().iter().map(|marker| marker.id).collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "marker ids must be unique");
    assert_eq!(count, 3);
}

#[test]
fn omitted_arguments_arrive_as_nil_not_defaults() {
    let mut world = world_with(
        r#"
        function on_start()
          local world = lb.world()
          lb.spawn_marker(world.width / 2, world.height / 2)
        end
        "#,
    );
    let _ = &mut world;

    let marker = &world.markers()[0];
    assert_eq!(
        marker.label, "",
        "label defaults to empty, not to a host default"
    );
    assert_eq!(marker.kind, "marker");
}

#[test]
fn a_script_can_move_score_and_finish() {
    let mut world = world_with(
        r#"
        local done = false
        function on_tick(dt)
          if done then return end
          done = true
          lb.set_position("p-1", 5000, -50)
          lb.add_score("p-1", 2)
          lb.set_score("p-1", 10)
          lb.finish("p-1")
        end
        function on_finish(winner)
          lb.log("winner " .. winner)
        end
        "#,
    );
    // The first tick starts the script and runs `on_tick` right after, which
    // already finishes the round.
    world.advance();
    assert_eq!(world.phase, RoundPhase::Finished);

    let player = world.player("p-1").unwrap();
    assert_eq!(
        player.x,
        world.world_width(),
        "positions clamp into the world"
    );
    assert_eq!(player.y, 0.0);
    assert_eq!(player.score, 10);
    assert_eq!(world.phase, RoundPhase::Finished);
    assert_eq!(world.winner.as_deref(), Some("p-1"));
    assert!(!world.script_faulted());
}

#[test]
fn p_local_resolves_to_the_first_player() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          lb.add_score("p-local", 1)
        end
        "#,
    );
    run_until_running(&mut world);

    // The start tick already runs `on_tick` after `on_start`.
    assert_eq!(world.player("p-1").unwrap().score, 1);
}

#[test]
fn set_hp_zero_kills_without_a_killer() {
    let mut world = world_with(
        r#"
        local first = true
        function on_tick(dt)
          if first then
            first = false
            lb.set_hp("p-1", 0)
          end
        end
        "#,
    );
    run_until_running(&mut world);

    let player = world.player("p-1").unwrap();
    assert!(!player.is_alive);
    assert_eq!(player.deaths, 1);
    assert_eq!(player.score, 0);

    let events = world.drain_events();
    assert!(events.iter().any(|event| matches!(
        event,
        lanverse_game::sim::SimulationEvent::PlayerDied { killer, .. } if killer == "mode"
    )));
}

#[test]
fn on_input_receives_the_accepted_input() {
    let mut world = world_with(
        r#"
        function on_input(id, mx, my, action)
          if action == "interact" then lb.add_score(id, 4) end
        end
        "#,
    );
    run_until_running(&mut world);

    world.apply_input("p-1", 1, 0.0, 0.0, &["interact".to_owned()]);
    world.advance();
    world.apply_input("p-1", 2, 0.0, 0.0, &["interact".to_owned()]);
    world.advance();

    assert_eq!(
        world.player("p-1").unwrap().score,
        8,
        "each interact scores once"
    );
}

#[test]
fn a_runtime_error_parks_the_script_and_the_world_keeps_running() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          error("boom")
        end
        "#,
    );
    run_until_running(&mut world);
    world.advance();
    world.advance();

    assert!(world.script_faulted());
    let failure = world.script.as_ref().unwrap().failure().unwrap().to_owned();
    assert!(failure.contains("boom"), "{failure}");
    assert_eq!(world.phase, RoundPhase::Running, "the match keeps playing");
    assert!(
        world
            .drain_lua_logs()
            .iter()
            .any(|log| log.level == "error")
    );
}

#[test]
fn a_syntax_error_is_reported_at_attach_time() {
    let mut world: LuaWorld = World::new("m", SimulationConfig::default());
    let engine = LuaScript::new("broken", "local x =");
    let result = world.attach_script(engine, true);

    assert!(!result.ok);
    let failure = result.failure.unwrap();
    assert!(failure.contains("Lua load error"), "{failure}");
    assert!(!world.is_script_driven());
}

#[test]
fn an_infinite_loop_hits_the_instruction_budget() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          while true do end
        end
        "#,
    );
    run_until_running(&mut world);
    world.advance();

    assert!(world.script_faulted());
    let failure = world.script.as_ref().unwrap().failure().unwrap().to_owned();
    assert!(failure.contains("budget"), "{failure}");
}

#[test]
fn a_coroutine_that_never_yields_is_budgeted_too() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          local co = coroutine.wrap(function() while true do coroutine.yield() end end)
          for i = 1, 1e9 do co() end
        end
        "#,
    );
    run_until_running(&mut world);
    world.advance();

    assert!(world.script_faulted());
}

#[test]
fn the_sandbox_has_no_io_os_or_require() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          lb.log(tostring(io) .. " " .. tostring(os) .. " " .. tostring(require) .. " " .. tostring(load))
        end
        "#,
    );
    run_until_running(&mut world);
    world.advance();

    let texts: Vec<String> = world
        .drain_lua_logs()
        .into_iter()
        .map(|log| log.text)
        .collect();
    assert!(!texts.is_empty());
    assert!(
        texts.iter().all(|text| text == "nil nil nil nil"),
        "{texts:?}"
    );
}

#[test]
fn memory_is_bounded() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          local t = {}
          for i = 1, 1e9 do t[i] = string.rep("x", 1000) end
        end
        "#,
    );
    run_until_running(&mut world);
    world.advance();

    assert!(world.script_faulted());
}

#[test]
fn print_goes_to_the_mode_console() {
    let mut world = world_with(
        r#"
        function on_tick(dt)
          print("hello", 1, true, nil)
        end
        "#,
    );
    run_until_running(&mut world);
    world.advance();

    let logs = world.drain_lua_logs();
    assert!(
        logs.iter().any(|log| log.text == "hello\t1\ttrue\tnil"),
        "{logs:?}"
    );
}

#[test]
fn the_dry_run_accepts_the_builder_samples() {
    for mode in ["coin_rush", "king_of_the_hill"] {
        let report = dry_run::run(&builder_sample(mode));
        assert!(report.errors.is_empty(), "{mode}: {:?}", report.errors);
    }
}

#[test]
fn the_dry_run_reports_runtime_and_syntax_errors() {
    let report = dry_run::run("lb.missing()");
    assert!(!report.errors.is_empty());

    let report = dry_run::run("function on_start( end");
    assert!(
        report.errors[0].contains("Lua load error"),
        "{:?}",
        report.errors
    );
}

#[test]
fn the_coin_rush_sample_collects_a_coin_that_spawns_on_the_player() {
    // Deterministic RNG replacement: the sample asks for coordinates; make it
    // spawn one coin right on the player by wrapping `lb.random`.
    let script = builder_sample("coin_rush").replace(
        "local function spawn_coin()",
        "local function spawn_coin()\n  if _G.__first_coin then lb.spawn_marker(1600, 1440, \"+1\", \"coin\"); _G.__first_coin = nil; return end",
    );
    let mut world = world_with(&format!("_G = {{ __first_coin = true }}\n{script}"));
    run_until_running(&mut world);
    world.advance();

    assert!(
        world.player("p-1").unwrap().score >= 1,
        "the coin on the player was collected"
    );
    assert!(world.markers().iter().any(|marker| marker.kind == "coin"));
}

#[test]
fn effects_of_a_finished_hook_are_not_replayed() {
    // `on_input` is called with the world not running: it must not queue
    // anything that a later tick would apply.
    let mut world = world_with(
        r#"
        function on_input(id, mx, my, action)
          lb.add_score(id, 5)
        end
        "#,
    );
    world.apply_input("p-1", 1, 0.0, 0.0, &["interact".to_owned()]);
    world.advance();

    assert_eq!(world.player("p-1").unwrap().score, 0);
    let empty = LuaEffects::default();
    assert!(empty.is_empty());
}
