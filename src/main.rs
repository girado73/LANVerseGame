//! LAN Verse game server.
//!
//! One binary serves the whole LAN:
//!
//! ```text
//! lanverse_game serve        WebSocket + REST on $LANBASH_BIND (default :8080)
//! lanverse_game daemon       simulation only (opt-in split deployment)
//! lanverse_game migrate      apply the Postgres schema
//! lanverse_game token        issue a builder token
//! lanverse_game token revoke revoke an account's tokens
//! lanverse_game mode         inspect / transfer mode ownership
//! lanverse_game export       export parity fixtures metadata
//! ```

use std::sync::Arc;

use anyhow::{Context, Result};
use lanverse_game::net::http::AppParts;
use lanverse_game::net::loop_driver::StatsHandle;
use lanverse_game::net::router::InProcessRouter;
use lanverse_game::settings::Settings;
use tokio::signal;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("serve");

    init_tracing();

    match command {
        // The server is async…
        "serve" | "daemon" => {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("creating the server runtime")?;

            runtime.block_on(async {
                match command {
                    "serve" => serve().await,
                    _ => daemon().await,
                }
            })
        }
        // …and the operator commands own their runtime (they print plain text
        // and run to completion).
        "migrate" => lanverse_game::cli::migrate(&args[1..]),
        "token" => token(&args[1..]),
        "mode" => mode(&args[1..]),
        "export" => export(&args[1..]),
        "-h" | "--help" | "help" => {
            print_help();
            Ok(())
        }
        other => anyhow::bail!("unknown command `{other}` (try --help)"),
    }
}

fn print_help() {
    println!(
        "\
LAN Verse game server

USAGE:
    lanverse_game <COMMAND>

COMMANDS:
    serve        WebSocket + REST + admin (default)
    daemon       simulation only, frames via Redis
    migrate      apply the Postgres schema
    token        issue a builder token (--email=…) | --revoke
    mode         list modes, transfer ownership (--owner=email)
    export       write parity fixture metadata

ENVIRONMENT (defaults):
    LANBASH_BIND               0.0.0.0:8080
    LANBASH_ADMIN_BIND         127.0.0.1:8081
    LANBASH_DATABASE_URL       postgres://…
    LANBASH_REDIS_URL          redis://127.0.0.1:6379/2
    LANBASH_TICK_RATE          60
    LANBASH_SNAPSHOT_RATE      20
    LANBASH_DEFAULT_MATCH      lan
    LANBASH_RESULT_SECRET      HMAC secret for round results
    LANBASH_ADMIN_KEY          admin page / internal endpoints
    RUST_LOG                   tracing filter (default info)"
    );
}

fn init_tracing() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info,lanverse_game=info".to_owned());

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .init();
}

async fn serve() -> Result<()> {
    let settings = Settings::from_env();
    let bind = settings.bind.clone();
    let admin_bind = settings.admin_bind.clone();

    tracing::info!(
        bind = %bind,
        tick_rate = settings.simulation.tick_rate,
        snapshot_rate = settings.simulation.snapshot_rate,
        "starting LAN Verse"
    );

    let router = Arc::new(InProcessRouter::default());
    let stats = StatsHandle::default();
    let stop = lanverse_game::net::loop_driver::StopHandle::default();
    let control = lanverse_game::runtime_service::Control::connect(&settings).await;
    let parts = AppParts {
        stats: stats.clone(),
        router: router.clone(),
        settings: Arc::new(settings.clone()),
        control: Arc::new(control.control_plane(&settings)),
    };
    let admin_app = lanverse_game::net::http::admin(parts.clone());

    // The loop builds and owns the service on its own thread (Lua is not
    // `Send`); HTTP handlers read the published statistics only.
    let loop_settings = settings.clone();
    {
        let router = router.clone();
        let stop = stop.clone();
        let loop_control = control.shared();
        let tick_rate = settings.simulation.tick_rate;
        std::thread::Builder::new()
            .name("lanverse-game".to_owned())
            .spawn(move || {
                lanverse_game::net::loop_driver::run(
                    router,
                    stats,
                    stop,
                    tick_rate,
                    lanverse_game::net::loop_driver::ServiceFactory::from_runtime(
                        loop_settings,
                        loop_control,
                    ),
                );
            })
            .context("spawning the game loop")?;
    }

    let app = lanverse_game::net::http::build(parts);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;

    let admin_listener = tokio::net::TcpListener::bind(&admin_bind)
        .await
        .with_context(|| format!("binding admin listener {admin_bind}"));

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    match admin_listener {
        Ok(admin_listener) => {
            tracing::info!(admin_bind = %admin_bind, "admin listener up");

            tokio::select! {
                result = server => result?,
                result = axum::serve(admin_listener, admin_app) => result?,
            }
        }
        Err(error) => {
            tracing::warn!(%error, "admin listener unavailable, continuing without it");
            server.await?;
        }
    }

    tracing::info!("shutting down");
    stop.stop();
    Ok(())
}

/// Optional split deployment: simulation only (sockets and REST stay in a
/// `serve` process).
async fn daemon() -> Result<()> {
    let settings = Settings::from_env();
    tracing::info!("simulation daemon starting (no sockets)");

    let router = Arc::new(InProcessRouter::default());
    let stats = StatsHandle::default();
    let stop = lanverse_game::net::loop_driver::StopHandle::default();
    let tick_rate = settings.simulation.tick_rate;
    let control = lanverse_game::runtime_service::Control::connect(&settings).await;

    let loop_stop = stop.clone();
    std::thread::Builder::new()
        .name("lanverse-game".to_owned())
        .spawn(move || {
            lanverse_game::net::loop_driver::run(
                router,
                stats,
                loop_stop,
                tick_rate,
                lanverse_game::net::loop_driver::ServiceFactory::from_runtime(settings, control),
            );
        })
        .context("spawning the game loop")?;

    shutdown_signal().await;
    tracing::info!("daemon shutting down");
    stop.stop();
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn token(args: &[String]) -> Result<()> {
    lanverse_game::cli::token(args)
}

fn mode(args: &[String]) -> Result<()> {
    lanverse_game::cli::mode(args)
}

fn export(args: &[String]) -> Result<()> {
    lanverse_game::cli::export(args)
}
