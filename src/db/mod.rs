//! Postgres access (sqlx, `PgPool`).
//!
//! One pool per process. Queries are written by hand (no compile-time checked
//! macros) so the crate builds without a database; the schema lives in
//! `migrations/` and is applied by `lanverse_game migrate`.

use anyhow::{Context, Result};
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

pub mod accounts;
pub mod matches;
pub mod modes;

/// Connection pool plus the queries the control plane needs.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Opens a pool and verifies it with one round trip.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await
            .with_context(|| "connecting to Postgres")?;

        Ok(Self { pool })
    }

    /// Runs one query to completion from synchronous code.
    ///
    /// HTTP handlers own a multi-threaded runtime: parking that worker lets the
    /// same runtime drive the query. The game loop has no runtime (it must stay
    /// synchronous and cheap), so it gets a short-lived one for the single
    /// lookup instead.
    pub fn block<F: std::future::Future>(&self, future: F) -> F::Output {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("database runtime");
                runtime.block_on(future)
            }
        }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    /// Applies the embedded migrations (idempotent).
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations")
            .run(&self.pool)
            .await
            .context("applying migrations")?;
        Ok(())
    }

    /// Row count of a table (used by the migration report).
    pub async fn count(&self, table: &str) -> Result<i64> {
        // Table names are internal constants, never user input.
        let row = sqlx::query(AssertSqlSafe(format!(
            "SELECT COUNT(*)::bigint AS n FROM {table}"
        )))
        .fetch_one(&self.pool)
        .await
        .with_context(|| format!("counting {table}"))?;
        Ok(row.get::<i64, _>("n"))
    }
}

/// Ensures the schema exists (used by `serve` on start and by tests).
pub async fn migrate(url: &str) -> Result<()> {
    let db = Db::connect(url, 4).await?;
    db.migrate().await
}
