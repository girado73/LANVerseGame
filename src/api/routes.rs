//! `/api/v1` handlers.
//!
//! The minimal set (health, mode list) is complete; the account, profile and
//! match endpoints are filled in by the control-plane phase and answer `501`
//! until then so a client gets a clear error instead of a 404.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::net::state::AppState;

#[axum::debug_handler]
pub async fn health(State(state): State<AppState>) -> Response {
    let stats = state.stats.get();
    let (worlds, players) = (stats.active_worlds, stats.connected_players);

    let database = match &state.control.db {
        Some(db) => db.ping().await,
        None => false,
    };
    let redis = state.control.redis.as_ref().is_some_and(|redis| {
        // Health must never block the loop: a short timeout is enough.
        futures_lite_block(async { redis.ping().await })
    });

    let healthy = state.control.db.is_none() || database;

    let body = json!({
        "status": if healthy { "ok" } else { "degraded" },
        "version": state.settings.version,
        "protocol_version": state.settings.realtime.protocol_version,
        "services": {
            "database": if database { "up" } else { "down" },
            "redis": if redis { "up" } else { "down" },
        },
        "realtime": {
            "driver": "inprocess",
            "active_worlds": worlds,
            "connected_players": players,
            "tick_rate": state.settings.simulation.tick_rate,
        },
        "time": chrono::Utc::now().to_rfc3339(),
    });

    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (status, Json(body)).into_response()
}

/// Runs a future to completion on the current thread (the health endpoint is
/// called at most a few times per second).
fn futures_lite_block<F: std::future::Future<Output = bool>>(future: F) -> bool {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

pub async fn list_modes(State(state): State<AppState>) -> Response {
    let Some(library) = &state.control.modes else {
        return Json(json!({ "modes": [] })).into_response();
    };

    match library.list(false) {
        Ok(modes) => Json(json!({ "modes": modes })).into_response(),
        Err(error) => server_error(error),
    }
}

pub async fn show_mode(State(state): State<AppState>, Path(mode): Path<String>) -> Response {
    let Some(library) = &state.control.modes else {
        return not_found("Mode not found.");
    };

    match library.package(&mode) {
        Ok(Some(package)) => Json(json!({ "mode": package })).into_response(),
        Ok(None) => not_found("Mode not found."),
        Err(error) => server_error(error),
    }
}

macro_rules! pending {
    ($name:ident($($arg:ident: $ty:ty),*)) => {
        pub async fn $name(State(state): State<AppState>, $($arg: $ty),*) -> Response {
            let _ = (&state, $(&$arg),*);
            (StatusCode::NOT_IMPLEMENTED, Json(json!({
                "message": "This endpoint is not available yet.",
            }))).into_response()
        }
    };
}

pending!(register(body: Json<Value>));
pending!(login(body: Json<Value>));
pending!(me());
pending!(logout());
pending!(profile());
pending!(update_profile(body: Json<Value>));
pending!(cosmetics());
pending!(equip_cosmetic(slug: Path<String>));
pending!(friends());
pending!(add_friend(body: Json<Value>));
pending!(remove_friend(friend: Path<i64>));
pending!(lobbies());
pending!(create_lobby(body: Json<Value>));
pending!(match_detail(id: Path<String>));
pending!(match_results(id: Path<String>));
pending!(highscores());
pending!(upload_mode(body: Json<Value>));
pending!(publish_mode(mode: Path<String>));
pending!(unpublish_mode(mode: Path<String>));
pending!(delete_mode(mode: Path<String>));

pub fn not_found(message: &str) -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "message": message }))).into_response()
}

pub fn server_error(error: impl std::fmt::Display) -> Response {
    tracing::error!(%error, "api error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "message": "Server error." })),
    )
        .into_response()
}
