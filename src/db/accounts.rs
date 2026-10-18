//! Accounts, tokens and profiles.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::Row;

use super::Db;

/// The reference server stored an account morph type in this column; the
/// migration copies it verbatim so old and new tokens resolve alike.
pub const USER_MORPH: &str = "App\\Models\\User";

#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub password: String,
    pub email_verified_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Player {
    pub id: i64,
    pub user_id: i64,
    pub display_name: String,
    pub avatar: Option<String>,
    pub level: i64,
    pub xp: i64,
    pub matches_played: i64,
    pub matches_won: i64,
    pub total_score: i64,
    pub kills: i64,
    pub deaths: i64,
    pub moderation_status: String,
    pub last_seen_at: Option<DateTime<Utc>>,
}

impl Player {
    pub fn is_banned(&self) -> bool {
        self.moderation_status == "banned"
    }
}

/// A row of `personal_access_tokens`.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessToken {
    pub id: i64,
    pub user_id: i64,
    pub token_hash: String,
    pub name: String,
    pub expires_at: Option<DateTime<Utc>>,
}

impl Db {
    // -----------------------------------------------------------------
    // Users and players
    // -----------------------------------------------------------------

    pub async fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        let row = sqlx::query(
            "SELECT id, name, email, password, email_verified_at, created_at FROM users WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(user_from_row))
    }

    pub async fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        let row = sqlx::query(
            "SELECT id, name, email, password, email_verified_at, created_at FROM users WHERE lower(email) = lower($1)",
        )
        .bind(email)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(user_from_row))
    }

    /// Creates a user with its player profile in one transaction.
    pub async fn create_user(
        &self,
        name: &str,
        email: &str,
        password_hash: &str,
    ) -> Result<(User, Player)> {
        let mut transaction = self.pool().begin().await?;

        let row = sqlx::query(
            "INSERT INTO users (name, email, password, created_at, updated_at)
             VALUES ($1, $2, $3, now(), now())
             RETURNING id, name, email, password, email_verified_at, created_at",
        )
        .bind(name)
        .bind(email.to_lowercase())
        .bind(password_hash)
        .fetch_one(&mut *transaction)
        .await
        .with_context(|| format!("creating user {email}"))?;
        let user = user_from_row(row);

        let row = sqlx::query(
            "INSERT INTO players (user_id, display_name, created_at, updated_at)
             VALUES ($1, $2, now(), now())
             RETURNING id, user_id, display_name, avatar, level, xp, matches_played, matches_won,
                       total_score, kills, deaths, moderation_status, last_seen_at",
        )
        .bind(user.id)
        .bind(name)
        .fetch_one(&mut *transaction)
        .await
        .with_context(|| format!("creating the player profile for {email}"))?;
        let player = player_from_row(row);

        transaction.commit().await?;
        Ok((user, player))
    }

    pub async fn player_by_user(&self, user_id: i64) -> Result<Option<Player>> {
        let row = sqlx::query(
            "SELECT id, user_id, display_name, avatar, level, xp, matches_played, matches_won,
                    total_score, kills, deaths, moderation_status, last_seen_at
             FROM players WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(player_from_row))
    }

    pub async fn player_by_id(&self, id: i64) -> Result<Option<Player>> {
        let row = sqlx::query(
            "SELECT id, user_id, display_name, avatar, level, xp, matches_played, matches_won,
                    total_score, kills, deaths, moderation_status, last_seen_at
             FROM players WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(player_from_row))
    }

    pub async fn update_password_hash(&self, user_id: i64, password_hash: &str) -> Result<()> {
        sqlx::query("UPDATE users SET password = $2, updated_at = now() WHERE id = $1")
            .bind(user_id)
            .bind(password_hash)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn update_profile(
        &self,
        player_id: i64,
        display_name: Option<&str>,
        avatar: Option<Option<&str>>,
    ) -> Result<Option<Player>> {
        let row = sqlx::query(
            "UPDATE players
             SET display_name = COALESCE($2, display_name),
                 avatar = CASE WHEN $3 THEN $4 ELSE avatar END,
                 updated_at = now()
             WHERE id = $1
             RETURNING id, user_id, display_name, avatar, level, xp, matches_played, matches_won,
                       total_score, kills, deaths, moderation_status, last_seen_at",
        )
        .bind(player_id)
        .bind(display_name)
        .bind(avatar.is_some())
        .bind(avatar.flatten())
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(player_from_row))
    }

    // -----------------------------------------------------------------
    // Tokens (Sanctum format: `<id>|<40 chars>`, SHA-256 stored)
    // -----------------------------------------------------------------

    /// Persists a token hash and returns the row id (the token's prefix).
    pub async fn create_token(
        &self,
        user_id: i64,
        name: &str,
        token_hash: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<i64> {
        let row = sqlx::query(
            "INSERT INTO personal_access_tokens
                (tokenable_type, tokenable_id, name, token, abilities, expires_at, created_at, updated_at)
             VALUES ($1, $2, $3, $4, '[\"*\"]', $5, now(), now())
             RETURNING id",
        )
        .bind(USER_MORPH)
        .bind(user_id)
        .bind(name)
        .bind(token_hash)
        .bind(expires_at)
        .fetch_one(self.pool())
        .await?;

        Ok(row.get::<i64, _>("id"))
    }

    pub async fn token_by_hash(&self, token_hash: &str) -> Result<Option<AccessToken>> {
        let row = sqlx::query(
            "SELECT id, tokenable_id, name, token, expires_at
             FROM personal_access_tokens
             WHERE token = $1 AND tokenable_type = $2",
        )
        .bind(token_hash)
        .bind(USER_MORPH)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|row| AccessToken {
            id: row.get("id"),
            user_id: row.get("tokenable_id"),
            token_hash: row.get("token"),
            name: row.get("name"),
            expires_at: row.get("expires_at"),
        }))
    }

    pub async fn touch_token(&self, token_id: i64) {
        let _ = sqlx::query("UPDATE personal_access_tokens SET last_used_at = now() WHERE id = $1")
            .bind(token_id)
            .execute(self.pool())
            .await;
    }

    pub async fn revoke_token(&self, token_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM personal_access_tokens WHERE id = $1")
            .bind(token_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn revoke_user_tokens(&self, user_id: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM personal_access_tokens WHERE tokenable_type = $1 AND tokenable_id = $2",
        )
        .bind(USER_MORPH)
        .bind(user_id)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }

    // -----------------------------------------------------------------
    // Cosmetics
    // -----------------------------------------------------------------

    pub async fn cosmetics_for(&self, player_id: i64) -> Result<(Vec<Value>, Vec<Value>)> {
        let owned = sqlx::query(
            "SELECT c.slug, c.name, c.kind, (pc.equipped_at IS NOT NULL) AS equipped
             FROM player_cosmetic pc JOIN cosmetics c ON c.id = pc.cosmetic_id
             WHERE pc.player_id = $1 ORDER BY c.slug",
        )
        .bind(player_id)
        .fetch_all(self.pool())
        .await?;

        let locked = sqlx::query(
            "SELECT slug, name, kind, unlock_level FROM cosmetics
             WHERE unlock_level > (SELECT level FROM players WHERE id = $1)
             ORDER BY unlock_level",
        )
        .bind(player_id)
        .fetch_all(self.pool())
        .await?;

        let owned = owned
            .iter()
            .map(|row| {
                serde_json::json!({
                    "slug": row.get::<String, _>("slug"),
                    "name": row.get::<String, _>("name"),
                    "kind": row.get::<String, _>("kind"),
                    "equipped": row.get::<bool, _>("equipped"),
                })
            })
            .collect();

        let locked = locked
            .iter()
            .map(|row| {
                serde_json::json!({
                    "slug": row.get::<String, _>("slug"),
                    "name": row.get::<String, _>("name"),
                    "kind": row.get::<String, _>("kind"),
                    "unlock_level": row.get::<i32, _>("unlock_level"),
                })
            })
            .collect();

        Ok((owned, locked))
    }

    /// Equips an owned cosmetic; one cosmetic per kind stays equipped.
    pub async fn equip_cosmetic(&self, player_id: i64, slug: &str) -> Result<bool> {
        let mut transaction = self.pool().begin().await?;

        let row = sqlx::query(
            "SELECT c.id, c.kind FROM cosmetics c
             JOIN player_cosmetic pc ON pc.cosmetic_id = c.id
             WHERE pc.player_id = $1 AND c.slug = $2",
        )
        .bind(player_id)
        .bind(slug)
        .fetch_optional(&mut *transaction)
        .await?;

        let Some(row) = row else {
            return Ok(false);
        };

        let cosmetic_id: i64 = row.get("id");
        let kind: String = row.get("kind");

        sqlx::query(
            "UPDATE player_cosmetic SET equipped_at = NULL
             WHERE player_id = $1 AND cosmetic_id IN (SELECT id FROM cosmetics WHERE kind = $2)",
        )
        .bind(player_id)
        .bind(&kind)
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            "UPDATE player_cosmetic SET equipped_at = now(), updated_at = now()
             WHERE player_id = $1 AND cosmetic_id = $2",
        )
        .bind(player_id)
        .bind(cosmetic_id)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;
        Ok(true)
    }

    // -----------------------------------------------------------------
    // Friends
    // -----------------------------------------------------------------

    pub async fn friends_for(&self, player_id: i64) -> Result<Value> {
        let rows = sqlx::query(
            "SELECT f.status, p.id, p.display_name, p.last_seen_at
             FROM friendships f JOIN players p ON p.id = f.friend_id
             WHERE f.player_id = $1 AND f.status IN ('accepted', 'pending')",
        )
        .bind(player_id)
        .fetch_all(self.pool())
        .await?;

        let incoming = sqlx::query(
            "SELECT p.id, p.display_name
             FROM friendships f JOIN players p ON p.id = f.player_id
             WHERE f.friend_id = $1 AND f.status = 'pending'",
        )
        .bind(player_id)
        .fetch_all(self.pool())
        .await?;

        let now = Utc::now();
        let mut friends = Vec::new();
        let mut outgoing = Vec::new();

        for row in rows {
            let status: String = row.get("status");
            let last_seen: Option<DateTime<Utc>> = row.get("last_seen_at");

            if status == "accepted" {
                friends.push(serde_json::json!({
                    "id": row.get::<i64, _>("id"),
                    "display_name": row.get::<String, _>("display_name"),
                    "level": 1,
                    "online": last_seen.is_some_and(|seen| (now - seen).num_minutes().abs() < 5),
                }));
            } else {
                outgoing.push(serde_json::json!({
                    "id": row.get::<i64, _>("id"),
                    "display_name": row.get::<String, _>("display_name"),
                }));
            }
        }

        let incoming: Vec<Value> = incoming
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.get::<i64, _>("id"),
                    "display_name": row.get::<String, _>("display_name"),
                })
            })
            .collect();

        Ok(serde_json::json!({
            "friends": friends,
            "outgoing": outgoing,
            "incoming": incoming,
        }))
    }

    pub async fn request_friend(&self, player_id: i64, friend_id: i64) -> Result<bool> {
        if player_id == friend_id {
            bail!("You cannot add yourself.");
        }

        sqlx::query(
            "INSERT INTO friendships (player_id, friend_id, status, created_at, updated_at)
             VALUES ($1, $2, 'pending', now(), now())
             ON CONFLICT (player_id, friend_id) DO NOTHING",
        )
        .bind(player_id)
        .bind(friend_id)
        .execute(self.pool())
        .await?;

        Ok(true)
    }

    pub async fn remove_friend(&self, player_id: i64, friend_id: i64) -> Result<()> {
        sqlx::query(
            "DELETE FROM friendships
             WHERE (player_id = $1 AND friend_id = $2) OR (player_id = $2 AND friend_id = $1)",
        )
        .bind(player_id)
        .bind(friend_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

fn user_from_row(row: sqlx::postgres::PgRow) -> User {
    User {
        id: row.get("id"),
        name: row.get("name"),
        email: row.get("email"),
        password: row.get("password"),
        email_verified_at: row.get("email_verified_at"),
        created_at: row.get("created_at"),
    }
}

fn player_from_row(row: sqlx::postgres::PgRow) -> Player {
    Player {
        id: row.get("id"),
        user_id: row.get("user_id"),
        display_name: row.get("display_name"),
        avatar: row.get("avatar"),
        level: i64::from(row.get::<i32, _>("level")),
        xp: i64::from(row.get::<i32, _>("xp")),
        matches_played: i64::from(row.get::<i32, _>("matches_played")),
        matches_won: i64::from(row.get::<i32, _>("matches_won")),
        total_score: i64::from(row.get::<i32, _>("total_score")),
        kills: i64::from(row.get::<i32, _>("kills")),
        deaths: i64::from(row.get::<i32, _>("deaths")),
        moderation_status: row.get("moderation_status"),
        last_seen_at: row.get("last_seen_at"),
    }
}
