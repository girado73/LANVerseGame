//! REST control plane and admin page.

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::{delete, get, patch, post, put};

use crate::auth::TokenService;
use crate::db::Db;
use crate::db::accounts::{Player, User};
use crate::net::state::AppState;
use crate::store::modes::ModeLibrary;
use crate::store::results::ResultRecorder;

pub mod admin;
pub mod auth;
pub mod handlers;
pub mod routes;

/// Everything the handlers need beyond request data. Queries live in
/// `crate::db`.
pub struct ControlPlane {
    pub db: Option<Db>,
    pub redis: Option<std::sync::Arc<crate::store::redis::RedisStore>>,
    pub tokens: Option<std::sync::Arc<TokenService>>,
    pub modes: Option<std::sync::Arc<ModeLibrary>>,
    pub results: Option<std::sync::Arc<ResultRecorder>>,
    pub secret: Option<String>,
    pub admin_key: Option<String>,
}

impl ControlPlane {
    /// A server without database or Redis (protocol-only deployments, tests).
    pub fn minimal() -> Self {
        Self {
            db: None,
            redis: None,
            tokens: None,
            modes: None,
            results: None,
            secret: None,
            admin_key: None,
        }
    }
}

/// The bearer token of a request.
pub fn bearer_from(headers: &HeaderMap) -> Option<&str> {
    crate::auth::bearer(
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
}

/// Resolves the request's account. `None` means "respond 401".
pub fn authenticate(state: &AppState, headers: &HeaderMap) -> Option<(User, Player)> {
    authenticate_with(state, bearer_from(headers))
}

/// Resolves the request's account from a token. `None` means "respond 401".
pub fn authenticate_with(state: &AppState, token: Option<&str>) -> Option<(User, Player)> {
    let token = token?;
    let db = state.control.db.as_ref()?;
    let tokens = state.control.tokens.as_ref()?;

    let (user_id, player) = db.block(tokens.resolve_token(token)).ok()?;
    let user = db.block(db.user_by_id(user_id)).ok().flatten()?;

    Some((user, player))
}

/// Runs a database call from the synchronous control plane.
///
/// `Db` owns a small current-thread runtime for exactly this purpose; the game
/// loop never uses it. Calls are short (one query) and never happen per tick.
pub fn blocking<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("blocking runtime");
            runtime.block_on(future)
        }
    }
}

/// `/api/v1` routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(routes::health))
        .route(
            "/modes",
            get(routes::list_modes).post(handlers::upload_mode),
        )
        .route(
            "/modes/{mode}",
            get(routes::show_mode).delete(handlers::delete_mode),
        )
        .route("/modes/{mode}/publish", post(handlers::publish_mode))
        .route("/modes/{mode}/unpublish", post(handlers::unpublish_mode))
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/me", get(auth::me))
        .route("/auth/logout", post(auth::logout))
        .route("/profile", get(handlers::profile))
        .route("/profile", patch(handlers::update_profile))
        .route("/profile/cosmetics", get(handlers::cosmetics))
        .route("/profile/cosmetics/{slug}", put(handlers::equip_cosmetic))
        .route("/friends", get(handlers::friends))
        .route("/friends", post(handlers::add_friend))
        .route("/friends/{friend}", delete(handlers::remove_friend))
        .route("/lobbies", get(handlers::lobbies))
        .route("/lobbies", post(handlers::create_lobby))
        .route("/matches/{id}", get(handlers::match_detail))
        .route("/matches/{id}/results", get(handlers::match_results))
        .route("/highscores", get(handlers::highscores))
}
