//! The axum application: WebSocket protocol and REST control plane on one
//! listener.
//!
//! One process serves both, which keeps a LAN deployment to a single command.
//! Routes are registered here and implemented in `crate::api`.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::net::loop_driver::StatsHandle;
use crate::net::router::InProcessRouter;
use crate::net::state::AppState;
use crate::net::ws;

/// Shared state of the HTTP application.
#[derive(Clone)]
pub struct AppParts {
    pub stats: StatsHandle,
    pub router: Arc<InProcessRouter>,
    pub settings: Arc<crate::settings::Settings>,
    pub control: Arc<crate::api::ControlPlane>,
}

/// Builds the full application (sockets + REST + admin).
pub fn build(parts: AppParts) -> Router {
    let state = AppState {
        stats: parts.stats,
        router: parts.router,
        settings: parts.settings,
        control: parts.control,
    };

    // The LAN-facing listener serves the game and the public API only. The
    // admin page and the internal result endpoint live on the separate
    // localhost listener (see `admin()`), never on the LAN port.
    Router::new()
        .nest("/api/v1", crate::api::routes())
        // Game protocol. Any non-WebSocket request on `/` is answered by the
        // upgrade handler with `426 Upgrade Required`.
        .route("/", get(ws::upgrade))
        // Compatibility alias for clients that still append `/app/<key>`.
        .route("/app", get(ws::upgrade))
        .route("/app/{key}", get(ws::upgrade))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Builds the admin/internal application (localhost listener).
pub fn admin(parts: AppParts) -> Router {
    let state = AppState {
        stats: parts.stats,
        router: parts.router,
        settings: parts.settings,
        control: parts.control,
    };

    Router::new()
        .merge(admin_routes())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Builds the REST application only (tests).
pub fn routes(parts: AppParts) -> Router {
    let state = AppState {
        stats: parts.stats,
        router: parts.router,
        settings: parts.settings,
        control: parts.control,
    };

    Router::new()
        .nest("/api/v1", crate::api::routes())
        .with_state(state)
}

fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/admin", get(crate::api::admin::index))
        .route("/admin/login", post(crate::api::admin::login))
        .route("/admin/logout", post(crate::api::admin::logout))
        .route("/admin/modes", get(crate::api::admin::modes))
        .route("/admin/modes/upload", post(crate::api::admin::upload))
        .route(
            "/admin/modes/{id}/publish",
            post(crate::api::admin::publish),
        )
        .route(
            "/admin/modes/{id}/unpublish",
            post(crate::api::admin::unpublish),
        )
        .route("/admin/modes/{id}/delete", post(crate::api::admin::delete))
        .route("/admin/modes/{id}/owner", post(crate::api::admin::owner))
        .route("/admin/tokens", post(crate::api::admin::tokens))
        .route("/internal/rounds", post(crate::api::admin::internal_round))
}
