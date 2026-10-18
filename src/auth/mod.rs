//! Maps a token (REST header or `hello.token`) to an account and profile.

pub mod tokens;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;

use crate::auth::tokens::{hash_token, needs_rehash, new_token, split_token};
use crate::db::Db;
use crate::db::accounts::Player;
use crate::net::service::{AuthError, GameIdentity, IdentityResolver};

/// Cached lookups live for a minute: a LAN client reconnects often and the
/// database is not on the critical path of a join.
pub const CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct TokenCacheEntry {
    pub identity: GameIdentity,
    pub expires_at: Instant,
}

/// Shared by the realtime resolver and the REST extractor.
pub struct TokenService {
    db: Db,
    cache: std::sync::Mutex<HashMap<String, TokenCacheEntry>>,
}

impl TokenService {
    pub fn new(db: Db) -> Self {
        Self {
            db,
            cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Synchronous wrapper for call sites outside an async runtime.
    pub fn block<F: std::future::Future>(&self, future: F) -> F::Output {
        self.db.block(future)
    }

    /// Issues a token for the builder (`lanverse_game token`), creating the
    /// account and profile when they do not exist.
    pub async fn issue_for_email(
        &self,
        email: &str,
        display_name: Option<&str>,
        device: &str,
    ) -> anyhow::Result<(String, i64, String)> {
        let (user, player) = match self.db.user_by_email(email).await? {
            Some(user) => {
                let player = match self.db.player_by_user(user.id).await? {
                    Some(player) => player,
                    None => {
                        let password = crate::auth::tokens::hash_password(&random_password())?;
                        let inserted: (i64,) = sqlx::query_as(
                            "INSERT INTO players (user_id, display_name, created_at, updated_at)
                             VALUES ($1, $2, now(), now()) RETURNING id",
                        )
                        .bind(user.id)
                        .bind(display_name.unwrap_or(&user.name))
                        .fetch_one(self.db.pool())
                        .await?;
                        let _ = password;
                        self.db
                            .player_by_id(inserted.0)
                            .await?
                            .ok_or_else(|| anyhow::anyhow!("profile vanished"))?
                    }
                };
                (user, player)
            }
            None => {
                let password = crate::auth::tokens::hash_password(&random_password())?;
                let name = display_name.map(str::to_owned).unwrap_or_else(|| {
                    email
                        .split('@')
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or("Builder")
                        .to_owned()
                });
                self.db.create_user(&name, email, &password).await?
            }
        };

        let (token, _) = self.create_token(user.id, device).await?;
        Ok((token, user.id, player.display_name))
    }

    /// Creates a token row and returns the plaintext token plus its id.
    pub async fn create_token(&self, user_id: i64, device: &str) -> anyhow::Result<(String, i64)> {
        // Insert with a random placeholder hash, then write the real hash that
        // contains the row id (the Sanctum `<id>|<secret>` layout).
        let placeholder = hash_token(&format!(
            "placeholder-{}",
            crate::sim::random::alphanumeric(32)
        ));
        let row_id = self
            .db
            .create_token(user_id, device, &placeholder, None)
            .await?;
        let token = new_token(row_id);
        sqlx::query("UPDATE personal_access_tokens SET token = $2 WHERE id = $1")
            .bind(row_id)
            .bind(hash_token(&token))
            .execute(self.db.pool())
            .await?;

        Ok((token, row_id))
    }

    /// Resolves a plaintext token to a live account.
    pub async fn resolve_token(&self, token: &str) -> Result<(i64, Player), AuthError> {
        let token = token.trim();
        let cache_key = hash_token(token);

        if let Some(entry) = self.cached(&cache_key)
            && let Some(profile_id) = entry.identity.player_profile_id
            && let Some(player) = self.db.player_by_id(profile_id).await.map_err(internal)?
            && !player.is_banned()
        {
            return Ok((player.user_id, player));
        }

        let Some((id, _)) = split_token(token) else {
            return Err(AuthError::new("Invalid token."));
        };

        let row = self
            .db
            .token_by_hash(&cache_key)
            .await
            .map_err(internal)?
            .ok_or_else(|| AuthError::new("Invalid token."))?;

        if row.id != id {
            return Err(AuthError::new("Invalid token."));
        }

        if row.expires_at.is_some_and(|expires| expires < Utc::now()) {
            return Err(AuthError::new("Token expired."));
        }

        let user = self
            .db
            .user_by_id(row.user_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| AuthError::new("Token is not bound to a user."))?;

        let player = self
            .db
            .player_by_user(user.id)
            .await
            .map_err(internal)?
            .ok_or_else(|| AuthError::new("User has no player profile."))?;

        if player.is_banned() {
            return Err(AuthError::new("Account is banned."));
        }

        self.db.touch_token(row.id).await;
        self.remember(cache_key, &player);

        Ok((user.id, player))
    }

    fn cached(&self, key: &str) -> Option<TokenCacheEntry> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match cache.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.clone()),
            Some(_) => {
                cache.remove(key);
                None
            }
            None => None,
        }
    }

    fn remember(&self, key: String, player: &Player) {
        let identity = GameIdentity::authenticated(
            player.display_name.clone(),
            player.user_id,
            player.id,
            Some(player.display_name.clone()),
        );

        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                key,
                TokenCacheEntry {
                    identity,
                    expires_at: Instant::now() + CACHE_TTL,
                },
            );
    }

    pub fn forget(&self, token: &str) {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&hash_token(token));
    }

    /// Verifies credentials and upgrades legacy password hashes.
    pub async fn authenticate(
        &self,
        email: &str,
        password: &str,
    ) -> Result<(i64, Player), AuthError> {
        let user = self
            .db
            .user_by_email(email)
            .await
            .map_err(internal)?
            .ok_or_else(|| AuthError::new("These credentials do not match our records."))?;

        if !crate::auth::tokens::verify_password(password, &user.password) {
            return Err(AuthError::new(
                "These credentials do not match our records.",
            ));
        }

        let player = self.db.player_by_user(user.id).await.map_err(internal)?;

        if player.as_ref().is_some_and(Player::is_banned) {
            return Err(AuthError::new("This account has been banned."));
        }

        if needs_rehash(&user.password)
            && let Ok(hash) = crate::auth::tokens::hash_password(password)
        {
            let _ = self.db.update_password_hash(user.id, &hash).await;
        }

        let player = player.ok_or_else(|| AuthError::new("User has no player profile."))?;
        Ok((user.id, player))
    }
}

/// Realtime side: `hello.token` to identity.
pub struct Resolver {
    tokens: Arc<TokenService>,
}

impl Resolver {
    pub fn new(tokens: Arc<TokenService>) -> Self {
        Self { tokens }
    }

    fn sanitize(name: &str) -> String {
        let cleaned: String = name
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        let trimmed = cleaned.trim();

        if trimmed.is_empty() {
            "Player".to_owned()
        } else {
            trimmed.chars().take(32).collect()
        }
    }
}

impl IdentityResolver for Resolver {
    fn resolve(
        &self,
        token: Option<&str>,
        requested_name: &str,
    ) -> Result<GameIdentity, AuthError> {
        let name = Self::sanitize(requested_name);

        let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) else {
            return Ok(GameIdentity::guest(name));
        };

        // The game loop is synchronous and owns a current-thread runtime, so
        // `block_in_place` is unavailable: the shared database runtime runs the
        // lookup to completion (short query, never per tick).
        let (user_id, player) = self.tokens.block(self.tokens.resolve_token(token))?;

        Ok(GameIdentity::authenticated(
            name,
            user_id,
            player.id,
            Some(player.display_name),
        ))
    }
}

fn internal<E: std::fmt::Display>(error: E) -> AuthError {
    tracing::error!(%error, "token lookup failed");
    AuthError::new("Authentication is unavailable.")
}

fn random_password() -> String {
    crate::sim::random::alphanumeric(64)
}

/// Extracts a bearer token from an `Authorization` header.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let (scheme, token) = value.split_once(' ')?;

    if scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty() {
        Some(token.trim())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_are_parsed() {
        assert_eq!(bearer(Some("Bearer 1|abc")), Some("1|abc"));
        assert_eq!(bearer(Some("bearer 1|abc")), Some("1|abc"));
        assert_eq!(bearer(Some("Basic xyz")), None);
        assert_eq!(bearer(None), None);
    }
}
