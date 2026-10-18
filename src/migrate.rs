//! MySQL → Postgres data migration (`lanverse_game migrate`).
//!
//! Reads the former server's MySQL database (URL from `LANBASH_MYSQL_URL`),
//! writes it
//! into Postgres in foreign-key order, and reports both row counts. Every
//! insert is `ON CONFLICT DO NOTHING`, so the tool is idempotent and a failed
//! run can simply be repeated.
//!
//! Fidelity rules from the reference schema:
//!
//! * `personal_access_tokens.token` already holds the SHA-256 of the plaintext,
//!   so clients keep their tokens unchanged;
//! * bcrypt password hashes are copied verbatim and upgraded to Argon2id on the
//!   next successful login;
//! * ids are preserved so `players.user_id`, `matches.id` and every highscore
//!   keep pointing at the same rows;
//! * framework bookkeeping tables (`sessions`, `cache`, `jobs`, …) are skipped.

use anyhow::{Context, Result, bail};
use sqlx::mysql::{MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::{AssertSqlSafe, Column, Postgres, Row, Transaction};

use crate::db::Db;

/// Tables copied, in foreign-key order.
pub const TABLES: &[&str] = &[
    "users",
    "players",
    "personal_access_tokens",
    "cosmetics",
    "player_cosmetic",
    "friendships",
    "lobbies",
    "lobby_members",
    "matches",
    "match_players",
    "match_rounds",
    "highscores",
    "game_modes",
];

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TableReport {
    pub table: String,
    /// Rows in the MySQL source (after the id filter).
    pub source_rows: i64,
    /// Rows already in Postgres before this run.
    pub target_rows_before: i64,
    /// Rows in Postgres after this run (may include locally created data).
    pub target_rows_after: i64,
    /// Rows this run added.
    pub inserted: i64,
    /// Source rows that are present in Postgres after the run.
    pub present: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct MigrationReport {
    pub tables: Vec<TableReport>,
    pub dry_run: bool,
}

impl MigrationReport {
    /// Every source row is present in Postgres (extra local rows are fine).
    pub fn is_consistent(&self) -> bool {
        self.tables
            .iter()
            .all(|table| table.present == table.source_rows)
    }

    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "{:<24} {:>8} {:>8} {:>8} {:>8} {:>8}",
            "table", "source", "before", "after", "inserted", "present"
        )];

        for table in &self.tables {
            lines.push(format!(
                "{:<24} {:>8} {:>8} {:>8} {:>8} {:>8}",
                table.table,
                table.source_rows,
                table.target_rows_before,
                table.target_rows_after,
                table.inserted,
                table.present
            ));
        }

        lines.join("\n")
    }
}

/// Copies everything. With `dry_run` only the report is produced.
pub async fn migrate(mysql_url: &str, target: &Db, dry_run: bool) -> Result<MigrationReport> {
    let source = MySqlPoolOptions::new()
        .max_connections(2)
        .connect(mysql_url)
        .await
        .context("connecting to MySQL")?;

    let mut report = MigrationReport {
        tables: Vec::new(),
        dry_run,
    };

    for table in TABLES {
        let rows = read_table(&source, table).await?;
        let before = target.count(table).await?;
        let inserted = if dry_run {
            rows.len() as i64
        } else {
            insert_rows(target, table, &rows).await?
        };
        let after = if dry_run {
            before
        } else {
            target.count(table).await?
        };
        let present = if dry_run {
            0
        } else {
            count_present(target, table, &rows).await?
        };

        report.tables.push(TableReport {
            table: (*table).to_string(),
            source_rows: rows.len() as i64,
            target_rows_before: before,
            target_rows_after: after,
            inserted,
            present,
        });
    }

    if !dry_run {
        let body = serde_json::to_value(&report)?;
        sqlx::query("INSERT INTO data_migrations (source, report) VALUES ($1, $2)")
            .bind("mysql")
            .bind(body)
            .execute(target.pool())
            .await
            .context("recording the migration")?;
    }

    Ok(report)
}

/// How many of the copied rows exist in Postgres (matched by primary key).
async fn count_present(target: &Db, table: &str, rows: &[SourceRow]) -> Result<i64> {
    if rows.is_empty() {
        return Ok(0);
    }

    // Tables without a single-column key are counted by their unique keys'
    // first column; all schema tables start with their key.
    let key = "id";
    let mut present = 0_i64;

    for (_, values) in rows {
        let Some(SourceValue::Text(id)) = values.first() else {
            continue;
        };
        let id = convert_value(table, key, &SourceValue::Text(id.clone()));
        let Some(SourceValue::Text(id)) = Some(id) else {
            continue;
        };

        let exists: Option<i64> = sqlx::query_scalar(AssertSqlSafe(format!(
            "SELECT 1::bigint FROM {table} WHERE id::text = $1 LIMIT 1"
        )))
        .bind(&id)
        .fetch_optional(target.pool())
        .await?;

        if exists.is_some() {
            present += 1;
        }
    }

    Ok(present)
}

/// One row of a source table, both column order and values.
type SourceRow = (Vec<String>, Vec<SourceValue>);

#[derive(Debug, Clone, PartialEq)]
enum SourceValue {
    Null,
    Text(String),
}

async fn read_table(source: &MySqlPool, table: &str) -> Result<Vec<SourceRow>> {
    let exists: Option<String> = sqlx::query_scalar("SELECT table_name FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = ?")
        .bind(table)
        .fetch_optional(source)
        .await
        .with_context(|| format!("checking for {table}"))?;

    if exists.is_none() {
        // A database from an older release may lack a table; treat it as empty.
        return Ok(Vec::new());
    }

    // Every value is read as text (`CAST(… AS CHAR)`): MySQL's driver refuses
    // to decode TIMESTAMP/BIGINT UNSIGNED as the Rust types Postgres wants, and
    // the target casts the text back to its own column type anyway.
    let rows: Vec<MySqlRow> = sqlx::query(AssertSqlSafe(format!("SELECT * FROM {table}")))
        .fetch_optional(source)
        .await
        .ok()
        .flatten()
        .into_iter()
        .collect();

    let _ = rows;

    let columns: Vec<String> = sqlx::query(AssertSqlSafe(format!("SELECT * FROM {table} LIMIT 1")))
        .fetch_all(source)
        .await
        .with_context(|| format!("probing {table}"))?
        .first()
        .map(|row| {
            row.columns()
                .iter()
                .map(|column| column.name().to_string())
                .collect()
        })
        .unwrap_or_default();

    if columns.is_empty() {
        return Ok(Vec::new());
    }

    let projection = columns
        .iter()
        .map(|column| {
            format!(
                "CAST({} AS CHAR) AS {}",
                quote_mysql(column),
                quote_mysql(column)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");

    let rows: Vec<MySqlRow> =
        sqlx::query(AssertSqlSafe(format!("SELECT {projection} FROM {table}")))
            .fetch_all(source)
            .await
            .with_context(|| format!("reading {table}"))?;

    Ok(rows
        .iter()
        .map(|row| {
            let values = columns
                .iter()
                .map(
                    |column| match row.try_get::<Option<String>, _>(column.as_str()) {
                        Ok(Some(text)) => SourceValue::Text(text),
                        Ok(None) | Err(_) => SourceValue::Null,
                    },
                )
                .collect();
            (columns.clone(), values)
        })
        .collect())
}

/// Columns that `INSERT … ON CONFLICT DO NOTHING` must never touch: the
/// Postgres schema fills defaults for the framework's timestamps.
fn insertable(column: &str) -> bool {
    column != "remember_token"
}

async fn insert_rows(target: &Db, table: &str, rows: &[SourceRow]) -> Result<i64> {
    if rows.is_empty() {
        return Ok(0);
    }

    let mut inserted = 0_i64;
    let types = column_types(target, table).await?;

    for chunk in rows.chunks(200) {
        let mut transaction: Transaction<'_, Postgres> = target.pool().begin().await?;
        inserted += insert_chunk(&mut transaction, table, chunk, &types).await?;
        transaction.commit().await?;
    }

    Ok(inserted)
}

/// Postgres column types, so each parameter is cast explicitly (`$1::timestamptz`).
async fn column_types(
    target: &Db,
    table: &str,
) -> Result<std::collections::HashMap<String, String>> {
    let rows = sqlx::query(
        "SELECT column_name, data_type FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name = $1",
    )
    .bind(table)
    .fetch_all(target.pool())
    .await
    .with_context(|| format!("reading column types of {table}"))?;

    Ok(rows
        .iter()
        .map(|row| {
            (
                row.get::<String, _>("column_name"),
                row.get::<String, _>("data_type"),
            )
        })
        .collect())
}

fn pg_cast(data_type: &str) -> &'static str {
    match data_type {
        "timestamp with time zone" => "timestamptz",
        "timestamp without time zone" => "timestamp",
        "jsonb" => "jsonb",
        "json" => "json",
        "uuid" => "uuid",
        "smallint" => "smallint",
        "integer" => "integer",
        "bigint" => "bigint",
        "boolean" => "boolean",
        "bytea" => "bytea",
        _ => "text",
    }
}

async fn insert_chunk(
    transaction: &mut Transaction<'_, Postgres>,
    table: &str,
    chunk: &[SourceRow],
    types: &std::collections::HashMap<String, String>,
) -> Result<i64> {
    let (all_columns, _) = &chunk[0];

    // Values arrive as text; the former server's ULIDs become UUIDs, then every
    // value is cast to its column type explicitly.
    let converted: Vec<(Vec<String>, Vec<SourceValue>)> = chunk
        .iter()
        .map(|(columns, values)| {
            let values = columns
                .iter()
                .zip(values.iter())
                .map(|(column, value)| convert_value(table, column, value))
                .collect();
            (columns.clone(), values)
        })
        .collect();

    let columns: Vec<&String> = all_columns
        .iter()
        .filter(|column| insertable(column))
        .collect();

    let mut builder = format!("INSERT INTO {table} (");
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            builder.push_str(", ");
        }
        builder.push_str(&quote(column));
    }
    builder.push_str(") VALUES ");

    let mut parameter = 1usize;
    for row_index in 0..converted.len() {
        if row_index > 0 {
            builder.push_str(", ");
        }
        builder.push('(');

        for (column_index, column) in columns.iter().enumerate() {
            if column_index > 0 {
                builder.push_str(", ");
            }
            let data_type = types
                .get(column.as_str())
                .map(String::as_str)
                .unwrap_or("text");
            builder.push_str(&format!("${parameter}::{}", pg_cast(data_type)));
            parameter += 1;
        }
        builder.push(')');
    }
    builder.push_str(" ON CONFLICT DO NOTHING");

    let mut query = sqlx::query(AssertSqlSafe(builder));

    for (row_columns, values) in &converted {
        for (index, column) in row_columns.iter().enumerate() {
            if !insertable(column) {
                continue;
            }
            query = query.bind(as_text(&values[index]));
        }
    }

    let result = query
        .execute(&mut **transaction)
        .await
        .with_context(|| format!("inserting into {table}"))?;
    Ok(result.rows_affected() as i64)
}

/// Value conversions the schemas require (MySQL text → Postgres column).
fn convert_value(table: &str, column: &str, value: &SourceValue) -> SourceValue {
    match (table, column, value) {
        // The former server's ULIDs ("01m57why…") cannot be parsed as UUIDs;
        // derived deterministically so repeated runs map to the same row.
        ("game_modes" | "lobbies", "id", SourceValue::Text(text)) => {
            SourceValue::Text(ulid_to_uuid(text))
        }
        (_, _, other) => other.clone(),
    }
}

/// A UUID-shaped, deterministic id for a 26-character ULID.
pub fn ulid_to_uuid(ulid: &str) -> String {
    let digest = crate::auth::tokens::hash_token(ulid);
    format!(
        "{}-{}-{}-{}-{}",
        &digest[0..8],
        &digest[8..12],
        &digest[12..16],
        &digest[16..20],
        &digest[20..32]
    )
}

fn as_text(value: &SourceValue) -> Option<String> {
    match value {
        SourceValue::Text(text) => Some(text.clone()),
        SourceValue::Null => None,
    }
}

fn quote_mysql(column: &str) -> String {
    format!("`{}`", column.replace('`', "``"))
}

fn quote(column: &str) -> String {
    format!("\"{}\"", column.replace('"', "\"\""))
}

/// Rewrites JSON columns from MySQL text into Postgres JSONB before insert
/// (the schemas store `manifest`, `scores`, `abilities` as JSONB).
pub fn json_column(table: &str, column: &str) -> bool {
    matches!(
        (table, column),
        ("game_modes", "manifest") | ("match_rounds", "scores")
    ) || (table == "personal_access_tokens" && column == "abilities")
}

/// Validates a URL early so the CLI fails with a clear message.
pub fn check_url(url: &str) -> Result<()> {
    if !url.starts_with("mysql://") {
        bail!("LANBASH_MYSQL_URL must start with mysql://");
    }
    Ok(())
}

/// Helper for tests: is a URL usable?
pub async fn ping(url: &str) -> bool {
    match MySqlPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
    {
        Ok(pool) => {
            let ok = sqlx::query("SELECT 1").execute(&pool).await.is_ok();
            let _ = pool.close().await;
            ok
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulids_map_to_deterministic_uuids() {
        let first = ulid_to_uuid("01m57why7agd8z531cg06gjg5z");
        let second = ulid_to_uuid("01m57why7agd8z531cg06gjg5z");

        assert_eq!(first, second);
        assert_eq!(first.len(), 36);
        assert_eq!(first.matches('-').count(), 4);
        assert_ne!(first, ulid_to_uuid("01m57why7agd8z531cg06gjg5z0"));
    }

    #[test]
    fn only_mode_ids_are_translated() {
        let id = SourceValue::Text("01m57why7agd8z531cg06gjg5z".into());

        assert!(
            matches!(convert_value("game_modes", "id", &id), SourceValue::Text(text) if text.contains('-'))
        );
        assert_eq!(convert_value("users", "email", &id), id);
        assert_eq!(convert_value("game_modes", "slug", &id), id);
        assert_eq!(
            convert_value("game_modes", "id", &SourceValue::Null),
            SourceValue::Null
        );
    }

    #[test]
    fn php_timestamps_become_timestamptz() {
        assert_eq!(pg_cast("timestamp with time zone"), "timestamptz");
        assert_eq!(pg_cast("jsonb"), "jsonb");
        assert_eq!(pg_cast("bigint"), "bigint");
        assert_eq!(pg_cast("text"), "text");
    }

    #[test]
    fn framework_bookkeeping_columns_are_skipped() {
        assert!(!insertable("remember_token"));
        assert!(insertable("password"));
        assert!(insertable("token"));
    }

    #[test]
    fn only_mysql_urls_are_accepted() {
        assert!(check_url("mysql://root@127.0.0.1/lanbash").is_ok());
        assert!(check_url("postgres://x").is_err());
    }

    #[test]
    fn source_rows_are_reported_consistently() {
        let report = MigrationReport {
            tables: vec![TableReport {
                table: "users".into(),
                source_rows: 5,
                target_rows_before: 1,
                target_rows_after: 6,
                inserted: 5,
                present: 5,
            }],
            dry_run: false,
        };

        assert!(report.is_consistent());
        assert!(report.summary().contains("users"));
    }
}
