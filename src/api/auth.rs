//! Account endpoints: register, login, me, logout.
//!
//! The same token is the HTTP credential and the game credential
//! (`hello.token`), so a client logs in once.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::auth::tokens::{hash_password, needs_rehash};
use crate::db::accounts::{Player, User};
use crate::net::state::AppState;

/// Shape of `UserResource` in the reference implementation.
pub fn user_json(user: &User) -> Value {
    json!({
        "id": user.id,
        "name": user.name,
        "email": user.email,
        "email_verified_at": user.email_verified_at.map(|at| at.to_rfc3339()),
        "created_at": user.created_at.map(|at| at.to_rfc3339()),
    })
}

/// Shape of `PlayerResource`.
pub fn player_json(player: &Player) -> Value {
    json!({
        "id": player.id,
        "display_name": player.display_name,
        "avatar": player.avatar,
        "level": player.level,
        "xp": player.xp,
        "matches_played": player.matches_played,
        "matches_won": player.matches_won,
        "total_score": player.total_score,
        "kills": player.kills,
        "deaths": player.deaths,
        "moderation_status": player.moderation_status,
        "last_seen_at": player.last_seen_at.map(|at| at.to_rfc3339()),
    })
}

/// `{"message": …, "errors": {field: [message]}}`, like the old validation.
pub fn validation_error(status: StatusCode, fields: &[(&str, &str)]) -> Response {
    let mut errors = serde_json::Map::new();

    for (field, message) in fields {
        errors.insert(field.to_string(), json!([message]));
    }

    (
        status,
        Json(json!({
            "message": fields.first().map(|(_, message)| *message).unwrap_or("The given data was invalid."),
            "errors": errors,
        })),
    )
        .into_response()
}

/// Display names are rendered in the client: no control characters, 1..=32.
pub fn valid_display_name(name: &str) -> Option<&'static str> {
    if name.trim().is_empty() {
        return Some("The display name must not be empty.");
    }
    if name.chars().any(char::is_control) {
        return Some("The display name must not contain control characters.");
    }
    if name.trim().chars().count() > 32 {
        return Some("The display name may not be longer than 32 characters.");
    }
    None
}

/// Basic RFC-style check (the old server used `email:rfc`).
pub fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };

    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.contains(char::is_whitespace)
        && email.len() <= 255
}

pub async fn register(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(tokens) = state.control.tokens.as_ref() else {
        return unavailable();
    };
    let Some(db) = state.control.db.as_ref() else {
        return unavailable();
    };

    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let email = body
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    let confirmation = body
        .get("password_confirmation")
        .and_then(Value::as_str)
        .unwrap_or("");
    let device = body
        .get("device_name")
        .and_then(Value::as_str)
        .unwrap_or("lan-bash");

    if let Some(message) = valid_display_name(name) {
        return validation_error(StatusCode::UNPROCESSABLE_ENTITY, &[("name", message)]);
    }
    if name.chars().count() < 2 {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("name", "The display name must be at least 2 characters.")],
        );
    }
    if !valid_email(email) {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("email", "The email field must be a valid email address.")],
        );
    }
    if password.chars().count() < 10 {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "password",
                "The password field must be at least 10 characters.",
            )],
        );
    }
    if password != confirmation {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[(
                "password",
                "The password field confirmation does not match.",
            )],
        );
    }

    match crate::api::blocking(db.user_by_email(email)) {
        Ok(Some(_)) => {
            return validation_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &[("email", "The email has already been taken.")],
            );
        }
        Ok(None) => {}
        Err(error) => return crate::api::routes::server_error(error),
    }

    let hash = match hash_password(password) {
        Ok(hash) => hash,
        Err(error) => return crate::api::routes::server_error(error),
    };

    let (user, player) = match crate::api::blocking(db.create_user(name, email, &hash)) {
        Ok(pair) => pair,
        Err(error) => return crate::api::routes::server_error(error),
    };

    let (token, _) = match crate::api::blocking(tokens.create_token(user.id, device)) {
        Ok(token) => token,
        Err(error) => return crate::api::routes::server_error(error),
    };

    (
        StatusCode::CREATED,
        Json(json!({
            "user": user_json(&user),
            "player": player_json(&player),
            "token": token,
        })),
    )
        .into_response()
}

pub async fn login(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(tokens) = state.control.tokens.as_ref() else {
        return unavailable();
    };
    let Some(db) = state.control.db.as_ref() else {
        return unavailable();
    };

    let email = body
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let password = body.get("password").and_then(Value::as_str).unwrap_or("");
    let device = body
        .get("device_name")
        .and_then(Value::as_str)
        .unwrap_or("lan-bash");

    if !valid_email(email) {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("email", "The email field must be a valid email address.")],
        );
    }
    if password.is_empty() {
        return validation_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &[("password", "The password field is required.")],
        );
    }

    let (user_id, player) = match crate::api::blocking(tokens.authenticate(email, password)) {
        Ok(pair) => pair,
        Err(error) => {
            return validation_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &[("email", &error.message)],
            );
        }
    };

    let user = match crate::api::blocking(db.user_by_id(user_id)) {
        Ok(Some(user)) => user,
        Ok(None) => return unavailable(),
        Err(error) => return crate::api::routes::server_error(error),
    };

    let (token, _) = match crate::api::blocking(tokens.create_token(user_id, device)) {
        Ok(token) => token,
        Err(error) => return crate::api::routes::server_error(error),
    };

    Json(json!({
        "user": user_json(&user),
        "player": player_json(&player),
        "token": token,
    }))
    .into_response()
}

pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some((user, player)) = crate::api::authenticate(&state, &headers) else {
        return unauthorised();
    };

    Json(json!({
        "user": user_json(&user),
        "player": player_json(&player),
    }))
    .into_response()
}

/// Revokes only the token used for this request.
pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(tokens) = state.control.tokens.as_ref() else {
        return unavailable();
    };

    let Some(token) = crate::api::bearer_from(&headers) else {
        return unauthorised();
    };

    // Resolve (and therefore validate) it first, then delete that row.
    let token_hash = crate::auth::tokens::hash_token(token);
    let Some(db) = state.control.db.as_ref() else {
        return unavailable();
    };

    match crate::api::blocking(db.token_by_hash(&token_hash)) {
        Ok(Some(row)) => {
            let _ = crate::api::blocking(db.revoke_token(row.id));
            tokens.forget(token);
            Json(json!({ "revoked": true })).into_response()
        }
        Ok(None) => unauthorised(),
        Err(error) => crate::api::routes::server_error(error),
    }
}

/// Rehashes a legacy password hash in the background after a login (used by
/// the token service; kept public for tests).
pub async fn upgrade_password_if_needed(state: &AppState, user: &User, password: &str) {
    if !needs_rehash(&user.password) {
        return;
    }

    let Some(db) = state.control.db.as_ref() else {
        return;
    };

    if let Ok(hash) = hash_password(password) {
        let _ = crate::api::blocking(db.update_password_hash(user.id, &hash));
    }
}

pub fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "message": "Accounts are not available on this server." })),
    )
        .into_response()
}

pub fn unauthorised() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "message": "Unauthenticated." })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_reject_control_characters() {
        assert!(valid_display_name("Alice").is_none());
        assert!(valid_display_name("  ").is_some());
        assert!(valid_display_name("Ali\nce").is_some());
        assert!(valid_display_name(&"a".repeat(33)).is_some());
    }

    #[test]
    fn emails_need_a_domain_with_a_dot() {
        assert!(valid_email("alice@example.test"));
        assert!(!valid_email("alice@localhost"));
        assert!(!valid_email("alice"));
        assert!(!valid_email("@example.test"));
        assert!(!valid_email("a b@example.test"));
    }
}
