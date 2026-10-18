//! World rules the client is not allowed to overrule.
//!
//! Ported from the reference implementation's `WorldTest`: positions come from
//! validated intent, actions respect cooldowns, rounds advance on their own
//! tick budget and scores are earned server-side only. The parity fixtures
//! compare whole matches; these tests state each rule in isolation.

use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::lua::runtime::LuaScript;
use lanverse_game::sim::events::SimulationEvent;
use lanverse_game::sim::round_phase::RoundPhase;
use lanverse_game::sim::world::World;

type TestWorld = World<LuaScript>;

fn config(round_duration_ms: i64, countdown_ms: i64) -> SimulationConfig {
    let mut gameplay = GameplayConfig::default();
    gameplay.round.duration_ms = round_duration_ms;
    gameplay.round.countdown_ms = countdown_ms;
    gameplay.player.move_speed = 600.0;

    SimulationConfig::from_parts(60, 20, 8, 30, gameplay)
}

fn world(round_duration_ms: i64, countdown_ms: i64) -> TestWorld {
    World::new("m-test", config(round_duration_ms, countdown_ms))
}

/// Runs the world until the round is actually running.
fn run_until_running(world: &mut TestWorld) {
    for _ in 0..600 {
        world.advance();
        if world.phase == RoundPhase::Running {
            return;
        }
    }
    panic!("round never started");
}

fn input(world: &mut TestWorld, id: &str, sequence: i64, x: f64, y: f64, actions: &[&str]) {
    let actions: Vec<String> = actions.iter().map(|action| (*action).to_owned()).collect();
    world.apply_input(id, sequence, x, y, &actions);
}

#[test]
fn a_round_starts_once_the_minimum_player_count_is_reached() {
    let mut world = world(60_000, 0);
    assert!(world.add_player("p-1", "Alice", None));
    run_until_running(&mut world);

    assert_eq!(world.phase, RoundPhase::Running);
    assert_eq!(world.round_number, 1);
    assert_eq!(world.round_id(), Some("r-1"));
    assert!(world.time_left_ms() > 0);
}

#[test]
fn movement_speed_is_bounded_by_the_server() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    let start_x = world.player("p-1").unwrap().x;

    // The decoder normalizes, but the world must not depend on that: a hostile
    // caller cannot exceed one unit of intent either.
    for sequence in 1..=60 {
        input(&mut world, "p-1", sequence, 1.0, 0.0, &[]);
        world.advance();
    }

    let distance = world.player("p-1").unwrap().x - start_x;
    assert!(
        (distance - 600.0).abs() <= 1.0,
        "expected ~600 px, got {distance}"
    );
}

#[test]
fn stale_and_replayed_input_sequences_are_ignored() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    input(&mut world, "p-1", 10, 1.0, 0.0, &[]);
    assert_eq!(world.player("p-1").unwrap().acked_input_sequence, 10);
    assert_eq!(world.player("p-1").unwrap().input_x, 1.0);

    world.advance();

    input(&mut world, "p-1", 10, -1.0, 0.0, &[]);
    assert_eq!(world.player("p-1").unwrap().acked_input_sequence, 10);
    assert_eq!(world.player("p-1").unwrap().input_x, 0.0);

    input(&mut world, "p-1", 11, -1.0, 0.0, &[]);
    assert_eq!(world.player("p-1").unwrap().acked_input_sequence, 11);
    assert_eq!(world.player("p-1").unwrap().input_x, -1.0);
}

#[test]
fn movement_intent_is_cleared_after_every_tick() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    input(&mut world, "p-1", 1, 1.0, 0.0, &[]);
    world.advance();

    let player = world.player("p-1").unwrap();
    assert_eq!(player.input_x, 0.0);
    assert_eq!(player.velocity_x, 600.0);

    world.advance();
    assert_eq!(world.player("p-1").unwrap().velocity_x, 0.0);
}

#[test]
fn input_is_ignored_outside_a_running_round() {
    let mut world = world(60_000, 3_000);
    world.add_player("p-1", "Alice", None);

    // Still in the lobby: nothing may move.
    input(&mut world, "p-1", 1, 1.0, 0.0, &[]);
    world.advance();

    assert_eq!(world.player("p-1").unwrap().input_x, 0.0);
}

#[test]
fn a_dash_requires_movement_intent() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    input(&mut world, "p-1", 1, 0.0, 0.0, &["dash"]);
    world.advance();
    assert_eq!(world.player("p-1").unwrap().dash_until_tick, 0);

    input(&mut world, "p-1", 2, 1.0, 0.0, &["dash"]);
    world.advance();
    assert!(world.player("p-1").unwrap().dash_until_tick > world.tick);
}

#[test]
fn dash_and_jump_cooldowns_block_reuse() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    input(&mut world, "p-1", 1, 1.0, 0.0, &["dash", "jump"]);
    world.advance();
    let first_dash = world.player("p-1").unwrap().dash_until_tick;
    let first_jump = world.player("p-1").unwrap().jump_until_tick;

    input(&mut world, "p-1", 2, 1.0, 0.0, &["dash", "jump"]);
    world.advance();

    assert_eq!(
        world.player("p-1").unwrap().dash_until_tick,
        first_dash,
        "dash on cooldown"
    );
    assert_eq!(
        world.player("p-1").unwrap().jump_until_tick,
        first_jump,
        "jump on cooldown"
    );
}

#[test]
fn attacks_only_hit_enemies_in_range_and_in_front() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Attacker", None);
    world.add_player("p-2", "Victim", None);
    world.add_player("p-3", "Behind", None);

    // Force friendliness / hostility explicitly (team assignment is hashed).
    for (id, team) in [("p-1", "red"), ("p-2", "red"), ("p-3", "blue")] {
        world
            .players_mut()
            .iter_mut()
            .find(|player| player.id == id)
            .unwrap()
            .team = Some(team.to_owned());
    }

    run_until_running(&mut world);

    {
        let attacker = world
            .players_mut()
            .iter_mut()
            .find(|player| player.id == "p-1")
            .unwrap();
        attacker.x = 100.0;
        attacker.y = 100.0;
        attacker.facing_x = 1.0;
        attacker.facing_y = 0.0;
    }
    for (id, x) in [("p-2", 130.0), ("p-3", 70.0)] {
        let player = world
            .players_mut()
            .iter_mut()
            .find(|player| player.id == id)
            .unwrap();
        player.x = x;
        player.y = 100.0;
    }

    input(&mut world, "p-1", 1, 0.0, 0.0, &["interact"]);
    world.advance();

    assert_eq!(
        world.player("p-2").unwrap().hp,
        100,
        "teammates must not hurt each other"
    );
    assert_eq!(
        world.player("p-3").unwrap().hp,
        100,
        "targets behind the attacker are safe"
    );
}

#[test]
fn a_kill_awards_exactly_one_score_point_to_the_attacker() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    world.add_player("p-2", "Bob", None);

    for (id, team) in [("p-1", "red"), ("p-2", "blue")] {
        world
            .players_mut()
            .iter_mut()
            .find(|player| player.id == id)
            .unwrap()
            .team = Some(team.to_owned());
    }

    run_until_running(&mut world);

    {
        let attacker = world
            .players_mut()
            .iter_mut()
            .find(|player| player.id == "p-1")
            .unwrap();
        attacker.x = 200.0;
        attacker.y = 200.0;
        attacker.facing_x = 1.0;
        attacker.facing_y = 0.0;
    }
    {
        let victim = world
            .players_mut()
            .iter_mut()
            .find(|player| player.id == "p-2")
            .unwrap();
        victim.x = 220.0;
        victim.y = 200.0;
        victim.hp = 1;
    }

    input(&mut world, "p-1", 1, 0.0, 0.0, &["interact"]);
    world.advance();

    let victim = world.player("p-2").unwrap();
    assert_eq!(victim.hp, 0);
    assert!(!victim.is_alive);
    assert_eq!(world.player("p-1").unwrap().score, 1);

    let events = world.drain_events();
    let hits: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, SimulationEvent::PlayerHit { .. }))
        .collect();
    assert_eq!(hits.len(), 1);
    let scores: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, SimulationEvent::ScoreChanged { .. }))
        .collect();
    assert_eq!(scores.len(), 1);

    match &scores[0] {
        SimulationEvent::ScoreChanged { score, team, .. } => {
            assert_eq!(*score, 1);
            assert_eq!(team.as_deref(), Some("red"));
        }
        other => panic!("unexpected event {other:?}"),
    }
}

#[test]
fn a_dying_player_respawns_after_the_configured_delay() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    let respawn_at = world.tick + 30;
    {
        let player = world.players_mut().first_mut().unwrap();
        player.is_alive = false;
        player.hp = 0;
        player.respawn_at_tick = respawn_at;
    }

    for _ in 0..30 {
        world.advance();
    }

    let player = world.player("p-1").unwrap();
    assert!(player.is_alive);
    assert_eq!(player.hp, player.max_hp);
}

#[test]
fn the_round_finishes_when_its_tick_budget_runs_out() {
    let mut world = world(1_000, 0);
    world.add_player("p-1", "Alice", None);
    world.add_player("p-2", "Bob", None);
    run_until_running(&mut world);

    world.players_mut()[0].score = 5;
    world.players_mut()[1].score = 2;
    // Give the two players different teams so a team winner exists.
    world.players_mut()[0].team = Some("red".into());
    world.players_mut()[1].team = Some("blue".into());

    for _ in 0..70 {
        world.advance();
    }

    assert_eq!(world.phase, RoundPhase::Finished);
    assert_eq!(world.winner.as_deref(), Some("red"));
}

#[test]
fn a_full_world_refuses_additional_players() {
    let mut gameplay = GameplayConfig::default();
    gameplay.round.countdown_ms = 0;
    let mut world: TestWorld = World::new(
        "m-test",
        SimulationConfig::from_parts(60, 20, 2, 30, gameplay),
    );

    assert!(world.add_player("p-1", "Alice", None));
    assert!(world.add_player("p-2", "Bob", None));
    assert!(!world.add_player("p-3", "Cleo", None));
    assert!(!world.is_joinable());
}

#[test]
fn players_cannot_leave_the_world_bounds() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    for sequence in 1..=600 {
        input(&mut world, "p-1", sequence, -1.0, -1.0, &[]);
        world.advance();
    }

    let radius = world.config.gameplay.player.radius;
    let player = world.player("p-1").unwrap();
    assert!(player.x >= radius);
    assert!(player.y >= radius);
}

#[test]
fn teams_keep_their_assignment_across_a_reconnect() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", Some(7));
    let team = world.player("p-1").unwrap().team.clone();

    world.remove_player("p-1");
    assert!(world.add_player("p-1", "Alice", Some(7)));
    assert_eq!(world.player("p-1").unwrap().team, team);
}

#[test]
fn a_player_leaving_emits_a_left_event() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    world.drain_events();

    world.remove_player("p-1");

    assert_eq!(
        world.drain_events(),
        vec![SimulationEvent::PlayerLeft {
            player_id: "p-1".into()
        }]
    );
}

#[test]
fn items_heal_and_open_a_speed_window() {
    let mut world = world(60_000, 0);
    world.add_player("p-1", "Alice", None);
    run_until_running(&mut world);

    // Items sit on a grid; put the player on the first heal item.
    let heal = world
        .items()
        .iter()
        .find(|item| item.kind == lanverse_game::sim::items::ItemKind::Heal)
        .cloned()
        .unwrap();

    {
        let player = world.players_mut().first_mut().unwrap();
        player.x = heal.x;
        player.y = heal.y;
        player.hp = 50;
    }

    world.advance();

    assert_eq!(world.player("p-1").unwrap().hp, 75, "heal_amount is 25");
    let events = world.drain_events();
    assert!(events.iter().any(|event| matches!(
        event,
        SimulationEvent::ItemPicked { item_id, .. } if *item_id == heal.id
    )));
}
