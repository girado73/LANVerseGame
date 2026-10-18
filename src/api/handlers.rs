//! Profile, friends, lobbies, matches, highscores and mode writes.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::api::auth::{player_json, valid_display_name, validation_error};
use crate::net::state::AppState;

// ---------------------------------------------------------------------------
// Profile
// ---------------------------------------------------------------------------

pub async fn profile(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };

    Json(json!({ "player": player_json(&player) })).into_response()
}

pub async fn update_profile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    if player.is_banned() {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "display_name",
                "A banned account cannot change its profile.",
            )],
        );
    }

    let display_name = body.get("display_name").and_then(Value::as_str);

    if let Some(name) = display_name
        && let Some(message) = valid_display_name(name)
    {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("display_name", message)],
        );
    }

    let avatar = match body.get("avatar") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(value)) => {
            if value.chars().count() > 64 {
                return validation_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &[("avatar", "The avatar may not be longer than 64 characters.")],
                );
            }
            Some(Some(value.as_str()))
        }
        Some(_) => {
            return validation_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &[("avatar", "The avatar must be a string.")],
            );
        }
    };

    match crate::api::blocking(db.update_profile(player.id, display_name.map(str::trim), avatar)) {
        Ok(Some(updated)) => Json(json!({ "player": player_json(&updated) })).into_response(),
        Ok(None) => crate::api::auth::unauthorised(),
        Err(error) => {
            // A unique display name violation is a validation error, not a 500.
            let message = error.to_string();
            if message.contains("duplicate key") || message.contains("unique") {
                validation_error(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    &[("display_name", "The display name has already been taken.")],
                )
            } else {
                crate::api::routes::server_error(error)
            }
        }
    }
}

pub async fn cosmetics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    match crate::api::blocking(db.cosmetics_for(player.id)) {
        Ok((owned, locked)) => Json(json!({ "owned": owned, "locked": locked })).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

pub async fn equip_cosmetic(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    match crate::api::blocking(db.equip_cosmetic(player.id, &slug)) {
        Ok(true) => Json(json!({ "equipped": slug })).into_response(),
        Ok(false) => validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("cosmetic", "This cosmetic is not unlocked.")],
        ),
        Err(error) => crate::api::routes::server_error(error),
    }
}

// ---------------------------------------------------------------------------
// Friends
// ---------------------------------------------------------------------------

pub async fn friends(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    match crate::api::blocking(db.friends_for(player.id)) {
        Ok(value) => Json(value).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

pub async fn add_friend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    let Some(friend_id) = body.get("player_id").and_then(Value::as_i64) else {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("player_id", "The player id field is required.")],
        );
    };

    if friend_id == player.id {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("player_id", "You cannot add yourself.")],
        );
    }

    match crate::api::blocking(db.player_by_id(friend_id)) {
        Ok(None) => {
            return validation_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &[("player_id", "The selected player id is invalid.")],
            );
        }
        Ok(Some(_)) => {}
        Err(error) => return crate::api::routes::server_error(error),
    }

    match crate::api::blocking(db.request_friend(player.id, friend_id)) {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "status": "requested" }))).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

pub async fn remove_friend(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(friend): Path<i64>,
) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    match crate::api::blocking(db.remove_friend(player.id, friend)) {
        Ok(()) => Json(json!({ "removed": true })).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

// ---------------------------------------------------------------------------
// Lobbies
// ---------------------------------------------------------------------------

pub async fn lobbies(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(_) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    match crate::api::blocking(db.lobbies()) {
        Ok(lobbies) => Json(json!({ "lobbies": lobbies })).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

pub async fn create_lobby(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some((_, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    if player.is_banned() {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("name", "A banned account cannot create lobbies.")],
        );
    }

    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();

    if !(3..=64).contains(&name.chars().count()) {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "name",
                "The name field must be between 3 and 64 characters.",
            )],
        );
    }

    let game_mode = body
        .get("game_mode")
        .and_then(Value::as_str)
        .unwrap_or(&state.settings.realtime.game_mode)
        .to_owned();
    let max_players = body
        .get("max_players")
        .and_then(Value::as_i64)
        .unwrap_or(16);

    if !(2..=64).contains(&max_players) {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "max_players",
                "The max players field must be between 2 and 64.",
            )],
        );
    }

    match crate::api::blocking(db.create_lobby(name, &game_mode, max_players, player.id)) {
        Ok(lobby) => (StatusCode::CREATED, Json(json!({ "lobby": lobby }))).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

// ---------------------------------------------------------------------------
// Matches and highscores
// ---------------------------------------------------------------------------

pub async fn match_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(_) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    let matched = crate::api::blocking(db.match_by_id(&id));

    match matched {
        Ok(Some(row)) => {
            let participants = crate::api::blocking(db.match_participants(&id)).unwrap_or_default();

            Json(json!({
                "match": {
                    "id": row.id,
                    "game_mode": row.game_mode,
                    "status": row.status,
                    "round_number": row.round_number,
                    "tick_rate": row.tick_rate,
                    "started_at": row.started_at.map(|at| at.to_rfc3339()),
                    "closed_at": row.closed_at.map(|at| at.to_rfc3339()),
                    "participants": participants,
                },
            }))
            .into_response()
        }
        Ok(None) => crate::api::routes::not_found("Match not found."),
        Err(error) => crate::api::routes::server_error(error),
    }
}

pub async fn match_results(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(_) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    match crate::api::blocking(db.match_rounds(&id)) {
        Ok(rounds) => Json(json!({ "match_id": id, "rounds": rounds })).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct HighscoreQuery {
    pub game_mode: Option<String>,
    pub limit: Option<i64>,
}

pub async fn highscores(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HighscoreQuery>,
) -> Response {
    let Some(_) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    let game_mode = query
        .game_mode
        .unwrap_or_else(|| state.settings.realtime.game_mode.clone());
    let limit = query.limit.unwrap_or(25).clamp(1, 100);

    match crate::api::blocking(db.highscores(&game_mode, limit)) {
        Ok(scores) => Json(json!({ "game_mode": game_mode, "scores": scores })).into_response(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

// ---------------------------------------------------------------------------
// Mode writes (author token)
// ---------------------------------------------------------------------------

pub async fn upload_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let Some((user, player)) = crate::api::authenticate(&state, &headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(library) = state.control.modes.as_ref() else {
        return crate::api::auth::unavailable();
    };

    if player.is_banned() {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("name", "A banned account cannot upload game modes.")],
        );
    }

    let Some(manifest) = body.get("manifest") else {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("manifest", "The manifest field is required.")],
        );
    };
    let Some(script) = body.get("script").and_then(Value::as_str) else {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("script", "The script field is required.")],
        );
    };
    let publish = body.get("publish").and_then(Value::as_bool).unwrap_or(true);

    let id = manifest.get("id").and_then(Value::as_str).unwrap_or("");

    if id.is_empty() || id.chars().count() > 64 {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "manifest.id",
                "The manifest.id field must be 1..64 characters.",
            )],
        );
    }

    match library.upload(manifest, script, publish, Some(user.id)) {
        Ok(outcome) => {
            let status = if outcome.report.is_valid() {
                StatusCode::CREATED
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };

            let mut payload = json!({
                "mode": outcome.mode.to_summary(),
                "validation": outcome.report.to_value(),
            });

            // The old API also reported blocking errors in `errors`, which the
            // builder's publish command reads.
            if !outcome.report.is_valid() {
                payload["errors"] = json!({ "validation": outcome.report.errors });
            }

            (status, Json(payload)).into_response()
        }
        Err(crate::store::modes::LibraryError::NotOwner) => validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "manifest.id",
                "This mode id already belongs to another account.",
            )],
        ),
        Err(error) => validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("manifest", &error.to_string())],
        ),
    }
}

pub async fn publish_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mode): Path<String>,
) -> Response {
    moderate(&state, &headers, &mode, Moderate::Publish)
}

pub async fn unpublish_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mode): Path<String>,
) -> Response {
    moderate(&state, &headers, &mode, Moderate::Unpublish)
}

pub async fn delete_mode(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mode): Path<String>,
) -> Response {
    moderate(&state, &headers, &mode, Moderate::Delete)
}

enum Moderate {
    Publish,
    Unpublish,
    Delete,
}

/// Owner-only mode management (the admin page has its own path).
fn moderate(state: &AppState, headers: &HeaderMap, slug: &str, action: Moderate) -> Response {
    let Some((user, _)) = crate::api::authenticate(state, headers) else {
        return crate::api::auth::unauthorised();
    };
    let Some(library) = state.control.modes.as_ref() else {
        return crate::api::auth::unavailable();
    };
    let Some(db) = state.control.db.as_ref() else {
        return crate::api::auth::unavailable();
    };

    let existing = match crate::api::blocking(db.mode_by_slug(slug)) {
        Ok(Some(row)) => row,
        Ok(None) => return crate::api::routes::not_found("Mode not found."),
        Err(error) => return crate::api::routes::server_error(error),
    };

    if existing.created_by.is_some_and(|owner| owner != user.id) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "message": "Forbidden" })),
        )
            .into_response();
    }

    let outcome = match action {
        Moderate::Publish => library
            .publish(slug)
            .map(|row| json!({ "mode": row.to_summary() })),
        Moderate::Unpublish => library
            .unpublish(slug)
            .map(|row| json!({ "mode": row.to_summary() })),
        Moderate::Delete => library.delete(slug).map(|()| json!({ "deleted": slug })),
    };

    match outcome {
        Ok(value) => Json(value).into_response(),
        Err(crate::store::modes::LibraryError::NotFound) => {
            crate::api::routes::not_found("Mode not found.")
        }
        Err(error) => validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("mode", &error.to_string())],
        ),
    }
}
