//! The wire contract from `protocol.md`, exercised without sockets.
//!
//! Mirrors the reference implementation's `RealtimeProtocolTest`: the service
//! records what each connection would have received, so these tests assert the
//! exact frames a client parses.

use std::sync::Arc;

use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::net::connection::QueueTransport;
use lanverse_game::net::service::identity::{AuthError, IdentityResolver};
use lanverse_game::net::service::{DefaultMatchmaker, GameIdentity, GameService};
use serde_json::{Value, json};

fn config() -> SimulationConfig {
    let mut gameplay = GameplayConfig::default();
    gameplay.round.countdown_ms = 3_000;
    SimulationConfig::from_parts(60, 20, 8, 30, gameplay)
}

fn service() -> (GameService, Arc<QueueTransport>) {
    // A fresh match per test: `tick()` uses wall-clock timestamps, so a world
    // left running by an earlier test would keep ticking here.
    let transport = Arc::new(QueueTransport::default());
    let match_id = format!("m-protocol-{:08}", rand::random::<u32>());
    let service = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_matchmaker(Arc::new(DefaultMatchmaker::fixed(&match_id)))
        .with_router(transport.clone());

    (service, transport)
}

fn connect(service: &mut GameService, connection: &str) {
    service.handle_open(connection);
}

fn hello(service: &mut GameService, connection: &str, extra: Value) {
    let mut frame = json!({
        "type": "hello",
        "protocol_version": 1,
        "player_name": format!("Player-{connection}"),
    });

    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        frame[key] = value;
    }

    service.handle_frame(connection, &frame.to_string());
}

fn input(service: &mut GameService, connection: &str, sequence: i64, x: f64, y: f64) {
    service.handle_frame(
        connection,
        &json!({
            "type": "input",
            "client_tick": 0,
            "sequence": sequence,
            "movement": {"x": x, "y": y},
            "actions": [],
        })
        .to_string(),
    );
}

/// The id of the (single) match a service owns.
fn match_id_of(service: &GameService) -> String {
    service
        .worlds()
        .keys()
        .next()
        .cloned()
        .expect("a world exists")
}

fn frames(transport: &QueueTransport, connection: &str) -> Vec<Value> {
    transport.take(connection)
}

fn frames_of(transport: &QueueTransport, connection: &str, kind: &str) -> Vec<Value> {
    frames(transport, connection)
        .into_iter()
        .filter(|frame| frame["type"] == kind)
        .collect()
}

/// Drives the loop for `milliseconds` of wall-clock time, in small steps.
///
/// The service follows the real clock (and rebases after a jump it cannot
/// catch up on), so simulated timestamps must stay close to `now`.
fn run_loop(service: &mut GameService, milliseconds: i64) -> i64 {
    let start = chrono::Utc::now().timestamp_millis();
    let mut now = start;

    while now - start < milliseconds {
        now = chrono::Utc::now().timestamp_millis();
        service.tick(now);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    now
}

#[test]
fn the_handshake_sends_connection_established_then_welcome() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");

    let first = frames(&transport, "c1");
    assert_eq!(first[0]["event"], "pusher:connection_established");

    hello(&mut service, "c1", json!({"client_version": "0.1.0"}));
    let welcome = frames_of(&transport, "c1", "welcome");
    let welcome = &welcome[0];

    assert_eq!(welcome["protocol_version"], 1);
    assert!(welcome["player_id"].as_str().unwrap().starts_with("p-"));
    assert!(welcome["session_id"].is_string());
    assert!(welcome["match_id"].is_string());
    assert_eq!(welcome["tick_rate"], 60);
    assert!(welcome["server_tick"].is_i64());
}

#[test]
fn a_protocol_mismatch_is_fatal_and_closes() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({"protocol_version": 999}));

    let error = &frames_of(&transport, "c1", "error")[0];
    assert_eq!(error["code"], "protocol_mismatch");
    assert_eq!(error["fatal"], true);
    assert!(service.connection("c1").is_none());
}

#[test]
fn an_invalid_token_is_fatal() {
    struct RejectingTokens;

    impl IdentityResolver for RejectingTokens {
        fn resolve(&self, _token: Option<&str>, _name: &str) -> Result<GameIdentity, AuthError> {
            Err(AuthError::new("Invalid token."))
        }
    }

    let transport = Arc::new(QueueTransport::default());
    let mut service = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_identity(Arc::new(RejectingTokens))
        .with_router(transport.clone());

    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({"token": "1|nope"}));

    let error = &frames_of(&transport, "c1", "error")[0];
    assert_eq!(error["code"], "auth_failed");
    assert_eq!(error["fatal"], true);
}

#[test]
fn an_authenticated_profile_gets_a_stable_player_id_and_its_own_name() {
    struct Alice;

    impl IdentityResolver for Alice {
        fn resolve(&self, _token: Option<&str>, _name: &str) -> Result<GameIdentity, AuthError> {
            Ok(GameIdentity::authenticated(
                "Admin",
                1,
                17,
                Some("Alice".into()),
            ))
        }
    }

    let transport = Arc::new(QueueTransport::default());
    let mut service = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_identity(Arc::new(Alice))
        .with_matchmaker(Arc::new(DefaultMatchmaker::fixed("m-auth")))
        .with_router(transport.clone());

    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({"player_name": "Admin"}));

    let welcome = &frames_of(&transport, "c1", "welcome")[0];
    assert_eq!(welcome["player_id"], "p-17");

    let id = match_id_of(&service);
    let world = service.world(&id).expect("world");
    assert_eq!(world.player("p-17").unwrap().name, "Alice");
    assert_eq!(world.player("p-17").unwrap().user_id, Some(1));
}

#[test]
fn guests_are_allowed_without_a_token() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    assert_eq!(frames_of(&transport, "c1", "welcome").len(), 1);
    assert!(frames_of(&transport, "c1", "error").is_empty());
}

#[test]
fn duplicate_hellos_are_ignored() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));
    hello(&mut service, "c1", json!({}));

    assert_eq!(frames_of(&transport, "c1", "welcome").len(), 1);
}

#[test]
fn ping_is_answered_with_a_pong_that_echoes_the_client_time() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    service.handle_frame("c1", r#"{"type":"ping","client_time_ms":1739184020123}"#);

    let pong = &frames_of(&transport, "c1", "pong")[0];
    assert_eq!(pong["client_time_ms"], 1739184020123_i64);
    assert!(pong["server_time_ms"].is_i64());
}

#[test]
fn unknown_types_are_ignored_without_an_error() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    service.handle_frame("c1", r#"{"type":"blockchain_event","payload":{}}"#);

    assert!(frames_of(&transport, "c1", "error").is_empty());
}

#[test]
fn a_malformed_frame_gets_a_single_non_fatal_error() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    service.handle_frame("c1", "{definitely not json");
    let errors = frames_of(&transport, "c1", "error");

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["fatal"], false);
}

#[test]
fn a_flood_of_garbage_closes_the_connection() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    for _ in 0..40 {
        service.handle_frame("c1", "garbage");
    }

    assert!(service.connection("c1").is_none());
    let _ = transport;
}

#[test]
fn snapshots_follow_the_protocol_shape_and_are_monotonic() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));
    connect(&mut service, "c2");
    hello(&mut service, "c2", json!({}));

    run_loop(&mut service, 500);

    let snapshots = frames_of(&transport, "c1", "snapshot");
    assert!(!snapshots.is_empty());

    let snapshot = snapshots.last().unwrap();
    assert!(snapshot["tick"].is_i64());
    assert!(snapshot["acked_input_sequence"].is_i64());
    assert!(snapshot["round"]["round_id"].is_string());
    assert!(snapshot["round"]["time_left_ms"].is_i64());
    assert_eq!(snapshot["players"].as_array().unwrap().len(), 2);

    let player = &snapshot["players"][0];
    for key in [
        "id", "name", "position", "velocity", "facing", "hp", "max_hp", "is_alive", "score",
    ] {
        assert!(player.get(key).is_some(), "missing {key}");
    }

    let mut previous = -1;
    for snapshot in &snapshots {
        let tick = snapshot["tick"].as_i64().unwrap();
        assert!(tick > previous, "snapshot ticks must increase");
        previous = tick;
    }
}

/// The service's tick clock follows the wall clock; tests that care about
/// exact ticks let it settle first instead of racing a pending tick.
fn settle(service: &mut GameService) {
    service.tick(chrono::Utc::now().timestamp_millis());
    std::thread::sleep(std::time::Duration::from_millis(20));
}

#[test]
fn the_acknowledged_sequence_is_per_connection() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));
    connect(&mut service, "c2");
    hello(&mut service, "c2", json!({}));
    settle(&mut service);

    input(&mut service, "c1", 7, 1.0, 0.0);
    input(&mut service, "c2", 3, -1.0, 0.0);
    let match_id = match_id_of(&service);
    let acked = |service: &GameService, connection: &str| {
        let player = service
            .connection(connection)
            .and_then(|c| c.player_id.clone())
            .unwrap();
        service
            .world(&match_id)
            .unwrap()
            .player(&player)
            .unwrap()
            .acked_input_sequence
    };
    assert_eq!(
        acked(&service, "c1"),
        7,
        "the first input of a tick is accepted"
    );
    assert_eq!(acked(&service, "c2"), 3);

    run_loop(&mut service, 200);

    let last_c1 = frames_of(&transport, "c1", "snapshot");
    let last_c2 = frames_of(&transport, "c2", "snapshot");
    assert!(!last_c1.is_empty(), "snapshots reach c1");
    assert!(!last_c2.is_empty(), "snapshots reach c2");
    assert!(
        last_c1
            .iter()
            .all(|frame| frame["acked_input_sequence"] == 7),
        "c1 always sees its own sequence: {last_c1:?}"
    );
    assert!(
        last_c2
            .iter()
            .all(|frame| frame["acked_input_sequence"] == 3),
        "c2 always sees its own sequence: {last_c2:?}"
    );
}

#[test]
fn round_state_reaches_running() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    run_loop(&mut service, 4_000);

    let phases: Vec<String> = frames_of(&transport, "c1", "round_state")
        .into_iter()
        .filter_map(|frame| frame["phase"].as_str().map(str::to_owned))
        .collect();

    assert!(phases.contains(&"countdown".to_owned()), "{phases:?}");
    assert!(phases.contains(&"running".to_owned()), "{phases:?}");
}

#[test]
fn a_disconnect_broadcasts_player_left() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));
    connect(&mut service, "c2");
    hello(&mut service, "c2", json!({}));

    // Clear what the joins produced.
    let _ = frames(&transport, "c1");

    service.handle_close("c2");

    let left: Vec<Value> = frames(&transport, "c1")
        .into_iter()
        .filter(|frame| frame["event"] == "player_left")
        .collect();
    assert_eq!(left.len(), 1);
}

#[test]
fn a_reconnect_replaces_the_ghost_and_is_not_announced() {
    struct Alice;

    impl IdentityResolver for Alice {
        fn resolve(&self, _token: Option<&str>, _name: &str) -> Result<GameIdentity, AuthError> {
            Ok(GameIdentity::authenticated(
                "Alice",
                1,
                17,
                Some("Alice".into()),
            ))
        }
    }

    let transport = Arc::new(QueueTransport::default());
    let mut service = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_identity(Arc::new(Alice))
        .with_matchmaker(Arc::new(DefaultMatchmaker::fixed("m-reconnect")))
        .with_router(transport.clone());

    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));
    let joins = frames(&transport, "c2");
    assert!(joins.is_empty());

    connect(&mut service, "c2");
    hello(&mut service, "c2", json!({}));

    let id = match_id_of(&service);
    let world = service.world(&id).unwrap();
    assert_eq!(
        world.player_count(),
        1,
        "a reconnect must not pile up players"
    );
    assert_eq!(world.players()[0].id, "p-17");

    let announced: Vec<Value> = frames(&transport, "c1")
        .into_iter()
        .filter(|frame| frame["event"] == "player_joined")
        .collect();
    assert!(
        announced.is_empty(),
        "a reconnect must not be announced as a join"
    );
}

#[test]
fn idle_connections_are_swept_after_the_timeout() {
    let (mut service, _transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));

    // Timestamps are wall-clock based in the service; drive it far ahead.
    let now = chrono::Utc::now().timestamp_millis() + 60_000;
    service.tick(now);

    assert!(service.connection("c1").is_none());
}

#[test]
fn a_match_mode_is_named_in_the_welcome_and_served_on_pull() {
    use lanverse_game::net::service::modes::{ModePackage, ModeRepository};

    struct CoinRush;

    impl ModeRepository for CoinRush {
        fn find(&self, mode_id: &str) -> Option<ModePackage> {
            (mode_id == "coin_rush").then(|| ModePackage {
                id: "coin_rush".into(),
                name: "Coin Rush".into(),
                manifest: json!({"schema_version": 1, "id": "coin_rush", "round": {"countdown_ms": 0}}),
                script: "function on_tick(dt) end".into(),
                checksum: "abc123".into(),
            })
        }
    }

    let transport = Arc::new(QueueTransport::default());
    let mut service = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_modes(Arc::new(CoinRush))
        .with_matchmaker(Arc::new(DefaultMatchmaker::new("lan")))
        .with_router(transport.clone());

    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({"match_id": "coin_rush"}));

    let welcome = &frames_of(&transport, "c1", "welcome")[0];
    assert_eq!(welcome["mode"]["id"], "coin_rush");
    assert_eq!(welcome["mode"]["name"], "Coin Rush");
    assert_eq!(welcome["mode"]["checksum"], "abc123");

    service.handle_frame("c1", r#"{"type":"pull","what":"mode"}"#);
    let mode = &frames_of(&transport, "c1", "mode")[0];
    assert_eq!(mode["mode"]["script"], "function on_tick(dt) end");
}

#[test]
fn an_unknown_mode_falls_back_to_deathmatch() {
    let transport = Arc::new(QueueTransport::default());
    let mut service = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_matchmaker(Arc::new(DefaultMatchmaker::new("lan")))
        .with_router(transport.clone());

    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({"match_id": "does_not_exist"}));

    let welcome = &frames_of(&transport, "c1", "welcome")[0];
    assert!(welcome.get("mode").is_none());
    assert!(!service.world("does_not_exist").unwrap().is_script_driven());
}

#[test]
fn a_restarted_server_restores_a_running_world() {
    let (mut service, transport) = service();
    connect(&mut service, "c1");
    hello(&mut service, "c1", json!({}));
    run_loop(&mut service, 4_000);

    let match_id = match_id_of(&service);
    let world = service.world(&match_id).expect("world");
    assert_eq!(world.phase, lanverse_game::sim::RoundPhase::Running);
    let state = world.snapshot_state();
    let tick = world.tick;
    let round = world.round_number;
    let _ = frames(&transport, "c1");

    // A new process adopts the persisted state instead of starting over.
    let transport = Arc::new(QueueTransport::default());
    let mut restarted = GameService::new(config(), 1, 10_000, 5_000, "deathmatch")
        .with_matchmaker(Arc::new(DefaultMatchmaker::fixed(&match_id)))
        .with_router(transport.clone());
    restarted.restore_world(&match_id, &state);

    connect(&mut restarted, "c2");
    hello(&mut restarted, "c2", json!({}));

    let welcome = &frames_of(&transport, "c2", "welcome")[0];
    assert_eq!(
        welcome["server_tick"], tick,
        "the world resumes at the persisted tick"
    );
    assert_eq!(
        restarted.world(&match_id).unwrap().round_number,
        round,
        "the round continues"
    );
    assert_eq!(
        restarted.world(&match_id).unwrap().phase,
        lanverse_game::sim::RoundPhase::Running
    );
}
