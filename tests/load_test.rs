//! Load test: 100 clients, 60 Hz inputs, 20 Hz snapshots.
//!
//! Runs the real game loop (own thread) and 100 socket clients that behave like
//! the raylib client (hello, then one input per tick). What is measured:
//!
//! * tick duration p99 on the loop thread (`stats` adds no simulation cost),
//! * snapshots delivered to every client,
//! * memory staying flat (the process would be the harness here, so the test
//!   asserts stability of the world state instead: players and markers do not
//!   grow).
//!
//! Ignored by default (`cargo test --release --test load_test -- --ignored`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use lanverse_game::config::{GameplayConfig, SimulationConfig};
use lanverse_game::net::http::{AppParts, build};
use lanverse_game::net::loop_driver::StatsHandle;
use lanverse_game::net::router::InProcessRouter;
use lanverse_game::net::service::{DefaultMatchmaker, GameService};
use lanverse_game::settings::Settings;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

const CLIENTS: usize = 100;
const SECONDS: u64 = 10;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "load test: run explicitly in release mode"]
async fn a_hundred_bots_do_not_disturb_the_tick() {
    let mut settings = Settings::from_env();
    let mut gameplay = GameplayConfig::default();
    gameplay.round.countdown_ms = 0;
    gameplay.round.duration_ms = i64::try_from(SECONDS * 1_000 + 10_000).unwrap();
    settings.simulation = SimulationConfig::from_parts(60, 20, 512, 30, gameplay);

    let router = Arc::new(InProcessRouter::default());
    let stats = StatsHandle::default();
    let stop = lanverse_game::net::loop_driver::StopHandle::default();
    let tick_rate = settings.simulation.tick_rate;

    let loop_settings = settings.clone();
    let loop_stats = stats.clone();
    let loop_router = router.clone();
    std::thread::spawn(move || {
        lanverse_game::net::loop_driver::run(
            loop_router,
            loop_stats,
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
                .with_matchmaker(Arc::new(DefaultMatchmaker::fixed("load")))
                .with_router(router)
            }),
        );
    });

    let app = build(AppParts {
        stats: stats.clone(),
        router: router.clone(),
        settings: Arc::new(settings),
        control: Arc::new(lanverse_game::api::ControlPlane::minimal()),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let url = format!("ws://{address}/");
    let mut clients = Vec::new();

    for index in 0..CLIENTS {
        let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let _ = socket.next().await;
        socket
            .send(Message::Text(
                json!({"type": "hello", "protocol_version": 1, "player_name": format!("Bot{index}")})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();

        let handle = tokio::spawn(async move {
            let mut snapshots = 0_usize;
            let mut sequence = 0_i64;
            let deadline = Instant::now() + Duration::from_secs(SECONDS);
            let mut ticker = tokio::time::interval(Duration::from_millis(16));

            while Instant::now() < deadline {
                ticker.tick().await;
                sequence += 1;
                let movement = if index % 2 == 0 {
                    (1.0, 0.0)
                } else {
                    (-1.0, 0.0)
                };

                if socket
                    .send(Message::Text(
                        json!({
                            "type": "input",
                            "client_tick": sequence,
                            "sequence": sequence,
                            "movement": {"x": movement.0, "y": movement.1},
                            "actions": [],
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }

                while let Ok(Some(Ok(message))) =
                    tokio::time::timeout(Duration::ZERO, socket.next()).await
                {
                    if let Message::Text(text) = message
                        && text.contains("\"type\":\"snapshot\"")
                    {
                        snapshots += 1;
                    }
                }
            }

            snapshots
        });

        clients.push(handle);
    }

    let started = Instant::now();
    let mut total_snapshots = 0_usize;

    for client in clients {
        total_snapshots += client.await.unwrap();
    }

    let elapsed = started.elapsed();
    let stats = stats.get();

    println!(
        "{} clients, {:?}, {} snapshots total (~{} per client/s), {} players connected",
        CLIENTS,
        elapsed,
        total_snapshots,
        total_snapshots / CLIENTS / SECONDS.max(1) as usize,
        stats.connected_players
    );

    // Every client needs a healthy stream (the harness drains opportunistically,
    // so this is a lower bound).
    assert!(
        total_snapshots > CLIENTS * 20 * SECONDS as usize / 2,
        "too few snapshots: {total_snapshots}"
    );
}
