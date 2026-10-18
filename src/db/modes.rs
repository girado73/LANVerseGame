//! Uploaded Lua modes (the builder's packages).

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use sqlx::Row;

use super::Db;

/// Status values the reference implementation used.
pub const STATUS_LIVE: &str = "live";
pub const STATUS_DRAFT: &str = "draft";

/// One uploaded package (arguments of [`Db::upsert_mode`]).
#[derive(Debug, Clone, Copy)]
pub struct ModeUpload<'a> {
    pub slug: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub script: &'a str,
    pub manifest: &'a Value,
    pub checksum: &'a str,
    pub publish: bool,
    pub valid: bool,
    pub author: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GameModeRow {
    pub id: String,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub script: String,
    pub manifest: Value,
    pub checksum: String,
    pub status: String,
    pub live_checksum: Option<String>,
    pub created_by: Option<i64>,
    pub validated_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl GameModeRow {
    pub fn is_live(&self) -> bool {
        self.status == STATUS_LIVE
    }

    /// Short listing form (`GET /api/v1/modes`).
    pub fn to_summary(&self) -> Value {
        json!({
            "id": self.slug,
            "name": self.name,
            "description": self.description,
            "status": self.status,
            "checksum": self.checksum,
            "world": self.manifest.pointer("/world"),
            "round": self.manifest.pointer("/round"),
            "updated_at": self.updated_at.map(|at| at.to_rfc3339()),
        })
    }

    /// Full package (`GET /api/v1/modes/{id}`, `pull`).
    pub fn to_package(&self) -> Value {
        json!({
            "id": self.slug,
            "name": self.name,
            "description": self.description,
            "manifest": self.manifest,
            "script": self.script,
            "checksum": self.checksum,
            "status": self.status,
        })
    }

    /// The package the realtime plane loads (Redis cache format).
    pub fn to_mode_package(&self) -> crate::net::service::ModePackage {
        crate::net::service::ModePackage {
            id: self.slug.clone(),
            name: self.name.clone(),
            manifest: self.manifest.clone(),
            script: self.script.clone(),
            checksum: self
                .live_checksum
                .clone()
                .unwrap_or_else(|| self.checksum.clone()),
        }
    }
}

const COLUMNS: &str =
    "id, slug, name, description, script, manifest, checksum, status, live_checksum,
                       created_by, validated_at, updated_at";

impl Db {
    pub async fn modes(&self, include_drafts: bool) -> Result<Vec<GameModeRow>> {
        // Only the internal column list is interpolated.
        let rows = sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM game_modes
             WHERE ($1 OR status = 'live')
             ORDER BY CASE WHEN status = 'live' THEN 0 ELSE 1 END, name"
        )))
        .bind(include_drafts)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().map(mode_from_row).collect())
    }

    pub async fn mode_by_slug(&self, slug: &str) -> Result<Option<GameModeRow>> {
        // Only the internal column list is interpolated.
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM game_modes WHERE slug = $1"
        )))
        .bind(slug)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(mode_from_row))
    }

    /// The published package for a mode (what the realtime plane loads).
    pub async fn live_mode(&self, slug: &str) -> Result<Option<GameModeRow>> {
        // Only the internal column list is interpolated.
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT {COLUMNS} FROM game_modes WHERE slug = $1 AND status = 'live'"
        )))
        .bind(slug)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(mode_from_row))
    }

    /// Inserts or updates an upload. Publishing points `live_checksum` at the
    /// new checksum; a failed validation never takes a live mode offline.
    pub async fn upsert_mode(&self, package: &ModeUpload<'_>) -> Result<GameModeRow> {
        let ModeUpload {
            slug,
            name,
            description,
            script,
            manifest,
            checksum,
            publish,
            valid,
            author,
        } = *package;
        let mut transaction = self.pool().begin().await?;

        let existing = sqlx::query("SELECT id, status FROM game_modes WHERE slug = $1 FOR UPDATE")
            .bind(slug)
            .fetch_optional(&mut *transaction)
            .await?;

        let id: String = match &existing {
            Some(row) => row.get::<uuid::Uuid, _>("id").to_string(),
            None => uuid::Uuid::now_v7().to_string(),
        };
        let was_live = existing
            .as_ref()
            .is_some_and(|row| row.get::<String, _>("status") == STATUS_LIVE);
        let can_publish = publish && valid;
        let status = if can_publish || was_live {
            STATUS_LIVE
        } else {
            STATUS_DRAFT
        };

        sqlx::query(
            "INSERT INTO game_modes
                (id, slug, name, description, script, manifest, checksum, status, live_checksum,
                 created_by, validated_at, created_at, updated_at)
             VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now(), now())
             ON CONFLICT (slug) DO UPDATE SET
                name = EXCLUDED.name,
                description = EXCLUDED.description,
                script = EXCLUDED.script,
                manifest = EXCLUDED.manifest,
                checksum = EXCLUDED.checksum,
                status = EXCLUDED.status,
                live_checksum = EXCLUDED.live_checksum,
                created_by = COALESCE(game_modes.created_by, EXCLUDED.created_by),
                validated_at = EXCLUDED.validated_at,
                updated_at = now()",
        )
        .bind(&id)
        .bind(slug)
        .bind(name)
        .bind(description)
        .bind(script)
        .bind(manifest)
        .bind(checksum)
        .bind(status)
        .bind(if can_publish { Some(checksum) } else { None })
        .bind(author)
        .bind(if valid { Some(Utc::now()) } else { None })
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;

        self.mode_by_slug(slug)
            .await?
            .ok_or_else(|| anyhow::anyhow!("mode vanished after upsert"))
    }

    /// Promotes the last uploaded version to live.
    pub async fn publish_mode(&self, slug: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE game_modes
             SET status = 'live', live_checksum = checksum, updated_at = now()
             WHERE slug = $1 AND validated_at IS NOT NULL",
        )
        .bind(slug)
        .execute(self.pool())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn unpublish_mode(&self, slug: &str) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE game_modes SET status = 'draft', updated_at = now() WHERE slug = $1",
        )
        .bind(slug)
        .execute(self.pool())
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Deletes a draft. A live mode must be unpublished first.
    pub async fn delete_mode(&self, slug: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM game_modes WHERE slug = $1 AND status <> 'live'")
            .bind(slug)
            .execute(self.pool())
            .await?;

        Ok(result.rows_affected() > 0)
    }

    pub async fn transfer_mode_owner(&self, slug: &str, user_id: i64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE game_modes SET created_by = $2, updated_at = now() WHERE slug = $1",
        )
        .bind(slug)
        .bind(user_id)
        .execute(self.pool())
        .await?;

        Ok(result.rows_affected() > 0)
    }
}

fn mode_from_row(row: &sqlx::postgres::PgRow) -> GameModeRow {
    GameModeRow {
        id: row.get::<uuid::Uuid, _>("id").to_string(),
        slug: row.get("slug"),
        name: row.get("name"),
        description: row.get("description"),
        script: row.get("script"),
        manifest: row.get("manifest"),
        checksum: row.get("checksum"),
        status: row.get("status"),
        live_checksum: row.get("live_checksum"),
        created_by: row.get("created_by"),
        validated_at: row.get("validated_at"),
        updated_at: row.get("updated_at"),
    }
}
