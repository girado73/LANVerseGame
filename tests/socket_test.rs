//! End-to-end over a real socket: axum serves the upgrade, the loop ticks, the
//! client parses frames exactly like the raylib client does.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::net::http::{AppParts, build};
use lanverse_game::net::loop_driver::StatsHandle;
use lanverse_game::net::router::InProcessRouter;
use lanverse_game::net::service::{DefaultMatchmaker, GameService};
use lanverse_game::settings::Settings;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

async fn start() -> (String, Arc<InProcessRouter>) {
    let mut settings = Settings::from_env();
    let mut gameplay = GameplayConfig::default();
    gameplay.round.countdown_ms = 0;
    settings.simulation = SimulationConfig::from_parts(60, 20, 8, 30, gameplay);

    let router = Arc::new(InProcessRouter::default());
    let stats = StatsHandle::default();
    let stop = lanverse_game::net::loop_driver::StopHandle::default();

    // The real loop driver owns the service on its own thread.
    {
        let router = router.clone();
        let stats = stats.clone();
        let stop = stop.clone();
        let tick_rate = settings.simulation.tick_rate;
        let loop_settings = settings.clone();
        std::thread::spawn(move || {
            lanverse_game::net::loop_driver::run(
                router,
                stats,
                stop,
                tick_rate,
                lanverse_game::net::loop_driver::ServiceFactory::new(move |router| {
                    GameService::new(
                        loop_settings.simulation.clone(),
                        1,
                        10_000,
                        5_000,
                        "deathmatch",
                    )
                    .with_matchmaker(Arc::new(DefaultMatchmaker::fixed("m-e2e")))
                    .with_router(router)
                }),
            );
        });
    }

    let app = build(AppParts {
        stats,
        router: router.clone(),
        settings: Arc::new(settings),
        control: Arc::new(lanverse_game::api::ControlPlane::minimal()),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("ws://{address}/"), router)
}

async fn next_json(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        let message = tokio::time::timeout_at(deadline, socket.next())
            .await
            .expect("timed out waiting for a frame")
            .expect("socket closed")
            .expect("websocket error");

        if let Message::Text(text) = message {
            return serde_json::from_str(&text).expect("frame is JSON");
        }
    }
}

async fn wait_for(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    kind: &str,
) -> Value {
    loop {
        let frame = next_json(socket).await;

        if frame["type"] == kind {
            return frame;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_clients_see_each_other_and_move() {
    let (url, _router) = start().await;

    let (mut alice, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let first = next_json(&mut alice).await;
    assert_eq!(first["event"], "pusher:connection_established");

    alice
        .send(Message::Text(
            json!({"type": "hello", "protocol_version": 1, "player_name": "Alice"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let welcome = wait_for(&mut alice, "welcome").await;
    assert_eq!(welcome["tick_rate"], 60);
    assert_eq!(welcome["match_id"], "m-e2e");
    let alice_id = welcome["player_id"].as_str().unwrap().to_owned();
    wait_for(&mut alice, "round_state").await;

    let (mut bob, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let _ = next_json(&mut bob).await;
    bob.send(Message::Text(
        json!({"type": "hello", "protocol_version": 1, "player_name": "Bob"})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    let bob_welcome = wait_for(&mut bob, "welcome").await;
    let bob_id = bob_welcome["player_id"].as_str().unwrap().to_owned();

    // Alice learns about Bob.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut joined = None;
    for _ in 0..10 {
        let frame = next_json(&mut alice).await;
        if frame["event"] == "player_joined" {
            joined = Some(frame);
            break;
        }
    }
    assert_eq!(joined.unwrap()["player_id"], bob_id.as_str());

    // Alice moves right for a while and her snapshots acknowledge it. Inputs
    // are sent at roughly the client's pace.
    for sequence in 1..=20 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        alice
            .send(Message::Text(
                json!({
                    "type": "input",
                    "client_tick": sequence,
                    "sequence": sequence,
                    "movement": {"x": 1.0, "y": 0.0},
                    "actions": [],
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    }

    let mut saw_ack = false;
    let mut saw_bob = false;
    let mut first_x = None;
    let mut last_x = 0.0;
    let mut trace = Vec::new();

    for _ in 0..120 {
        let frame = wait_for(&mut alice, "snapshot").await;
        let players = frame["players"].as_array().unwrap();
        saw_bob |= players.iter().any(|player| player["id"] == bob_id.as_str());

        if let Some(me) = players
            .iter()
            .find(|player| player["id"] == alice_id.as_str())
        {
            let x = me["position"]["x"].as_f64().unwrap();
            first_x.get_or_insert(x);
            last_x = x;
            let acked = frame["acked_input_sequence"].as_i64().unwrap();
            saw_ack |= acked >= 1;
            if trace.len() < 80 {
                trace.push((x, acked));
            }
        }

        if saw_ack && saw_bob {
            break;
        }
    }

    assert!(saw_bob, "Alice must see Bob in snapshots");
    assert!(saw_ack, "the server acknowledges Alice's input");
    assert!(
        last_x > first_x.unwrap() + 5.0,
        "Alice moved right ({first_x:?} -> {last_x}); trace {trace:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ping_gets_a_pong_on_the_socket() {
    let (url, _router) = start().await;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let _ = next_json(&mut socket).await;

    socket
        .send(Message::Text(
            json!({"type": "ping", "client_time_ms": 42})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();

    let pong = wait_for(&mut socket, "pong").await;
    assert_eq!(pong["client_time_ms"], 42);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_alias_still_accepts_a_websocket() {
    let (url, _router) = start().await;
    let alias = format!("{}/app/some-key", url.trim_end_matches('/'));

    let (mut socket, _) = tokio_tungstenite::connect_async(&alias).await.unwrap();
    let first = next_json(&mut socket).await;
    assert_eq!(first["event"], "pusher:connection_established");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_and_modes_answer_over_http() {
    let (url, _router) = start().await;
    let base = url.trim_end_matches('/').replace("ws://", "http://");

    let health: Value = reqwest_get(&format!("{base}/api/v1/health")).await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["protocol_version"], 1);

    let modes: Value = reqwest_get(&format!("{base}/api/v1/modes")).await;
    assert!(modes["modes"].is_array());
}

/// Minimal HTTP GET (the crate has no HTTP client dependency; this is a tiny
/// hand-rolled request to `127.0.0.1`).
async fn reqwest_get(url: &str) -> Value {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let without_scheme = url.strip_prefix("http://").expect("http url");
    let (authority, path) = without_scheme
        .split_once('/')
        .unwrap_or((without_scheme, ""));
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    stream
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response);
    let body = text.split("\r\n\r\n").nth(1).expect("body");
    serde_json::from_str(body).expect("JSON body")
}
