//! Redis: shared state between processes and the mode-package cache.
//!
//! Key layout is the one the reference implementation used, so an exported
//! world state or cached package keeps working:
//!
//! | key | content |
//! |---|---|
//! | `lanbash:match:<id>:state` | serialized world (TTL 1 h) |
//! | `lanbash:match:<id>:owner` | process id of the simulating process (TTL 30 s) |
//! | `lanbash:matches:active` | set of match ids |
//! | `lanbash:mode-package:<id>` | published mode package (TTL 60 s) |
//! | `lanbash:default-match:<mode>` | shared auto-match id (TTL 1 day) |
//! | `lanbash:frames` | daemon → worker frame list |
//! | `lanbash:framerelays` | worker → daemon frame list |

use std::time::Duration;

use redis::AsyncCommands;
use serde_json::Value;

/// Seconds the shared default match id lives (reference: one day).
pub const DEFAULT_MATCH_TTL: u64 = 86_400;
/// TTL of a persisted world.
pub const WORLD_STATE_TTL: u64 = 3_600;
/// TTL of the owner heartbeat.
const OWNER_TTL: u64 = 30;

type Connection = redis::aio::MultiplexedConnection;

/// Thin async Redis facade. A failed command drops the connection and retries
/// once, so a Redis restart does not require a server restart.
pub struct RedisStore {
    // `redis::Client` is cheap to rebuild and not `Sync`; the URL is kept and
    // a fresh client is created whenever a connection has to be established.
    url: String,
    connection: tokio::sync::Mutex<Option<Connection>>,
}

impl RedisStore {
    pub fn connect(url: &str) -> redis::RedisResult<Self> {
        // Fail fast on a malformed URL.
        redis::Client::open(url)?;

        Ok(Self {
            url: url.to_owned(),
            connection: tokio::sync::Mutex::new(None),
        })
    }

    async fn connect_or_reuse(&self) -> redis::RedisResult<Connection> {
        let mut guard = self.connection.lock().await;

        if let Some(connection) = guard.as_ref() {
            return Ok(connection.clone());
        }

        let client = redis::Client::open(self.url.as_str()).map_err(|error| {
            redis::RedisError::from((
                redis::ErrorKind::InvalidClientConfig,
                "reopening the Redis client failed",
                error.to_string(),
            ))
        })?;
        let connection = client.get_multiplexed_async_connection().await?;
        *guard = Some(connection.clone());
        Ok(connection)
    }

    async fn drop_connection(&self) {
        *self.connection.lock().await = None;
    }

    /// Runs `operation`; on connection errors the socket is replaced and the
    /// command is retried once (commands used here are idempotent).
    async fn run<T, F, Fut>(&self, operation: F) -> redis::RedisResult<T>
    where
        F: Fn(Connection) -> Fut,
        Fut: std::future::Future<Output = redis::RedisResult<T>>,
    {
        // Commands are retried once, so closures must not own their payload
        // (`Fn`, not `FnOnce`); they borrow it instead.
        let _ = &operation;
        let first = operation(self.connect_or_reuse().await?).await;
        let Err(error) = first else {
            return first;
        };

        tracing::debug!(%error, "redis command failed, reconnecting");
        self.drop_connection().await;
        operation(self.connect_or_reuse().await?).await
    }

    pub async fn ping(&self) -> bool {
        self.run(|mut connection| async move {
            redis::cmd("PING")
                .query_async::<String>(&mut connection)
                .await
        })
        .await
        .is_ok()
    }

    pub async fn get(&self, key: &str) -> Option<String> {
        self.run(|mut connection| async move { connection.get::<_, Option<String>>(key).await })
            .await
            .ok()
            .flatten()
    }

    pub async fn set_ex(&self, key: &str, value: &str, seconds: u64) -> bool {
        self.run(|mut connection| async move {
            connection.set_ex::<_, _, ()>(key, value, seconds).await
        })
        .await
        .is_ok()
    }

    pub async fn del(&self, key: &str) -> bool {
        self.run(|mut connection| async move { connection.del::<_, i64>(key).await })
            .await
            .is_ok()
    }

    pub async fn sadd(&self, key: &str, member: &str) -> bool {
        self.run(|mut connection| async move { connection.sadd::<_, _, i64>(key, member).await })
            .await
            .is_ok()
    }

    pub async fn srem(&self, key: &str, member: &str) -> bool {
        self.run(|mut connection| async move { connection.srem::<_, _, i64>(key, member).await })
            .await
            .is_ok()
    }

    pub async fn smembers(&self, key: &str) -> Vec<String> {
        self.run(|mut connection| async move { connection.smembers(key).await })
            .await
            .unwrap_or_default()
    }

    /// `SET key value EX ttl NX`; `Some(true)` when this call created the key.
    pub async fn set_nx_ex(&self, key: &str, value: &str, seconds: u64) -> Option<bool> {
        self.run(|mut connection| async move {
            let result: Option<String> = redis::cmd("SET")
                .arg(key)
                .arg(value)
                .arg("EX")
                .arg(seconds)
                .arg("NX")
                .query_async(&mut connection)
                .await?;
            Ok(result.is_some())
        })
        .await
        .ok()
    }

    pub async fn expire(&self, key: &str, seconds: u64) -> bool {
        self.run(
            |mut connection| async move { connection.expire::<_, i64>(key, seconds as i64).await },
        )
        .await
        .is_ok()
    }

    pub async fn lpush(&self, key: &str, value: &str) -> bool {
        self.run(|mut connection| async move { connection.lpush::<_, _, i64>(key, value).await })
            .await
            .is_ok()
    }

    pub async fn rpop(&self, key: &str) -> Option<String> {
        self.run(
            |mut connection| async move { connection.rpop::<_, Option<String>>(key, None).await },
        )
        .await
        .ok()
        .flatten()
    }

    /// Writes a batch of world states: one `SET` per match plus owner
    /// heartbeat and the active-match set.
    pub async fn save_world_states(&self, owner: &str, states: &[(String, Value)]) {
        if states.is_empty() {
            return;
        }

        let payload: Vec<(String, String)> = states
            .iter()
            .map(|(id, state)| (id.clone(), state.to_string()))
            .collect();

        let shared = &payload;
        let _ = self
            .run(|mut connection| async move {
                for (match_id, state) in shared {
                    let state_key = format!("lanbash:match:{match_id}:state");
                    let owner_key = format!("lanbash:match:{match_id}:owner");
                    connection
                        .set_ex::<_, _, ()>(&state_key, state, WORLD_STATE_TTL)
                        .await?;
                    connection
                        .set_ex::<_, _, ()>(&owner_key, owner, OWNER_TTL)
                        .await?;
                    connection
                        .sadd::<_, _, i64>("lanbash:matches:active", match_id)
                        .await?;
                }
                Ok(())
            })
            .await;
    }

    pub async fn load_world_state(&self, match_id: &str) -> Option<Value> {
        let raw = self.get(&format!("lanbash:match:{match_id}:state")).await?;
        serde_json::from_str(&raw).ok()
    }

    pub async fn forget_match(&self, match_id: &str) {
        let _ = self.del(&format!("lanbash:match:{match_id}:state")).await;
        let _ = self.del(&format!("lanbash:match:{match_id}:owner")).await;
        let _ = self.srem("lanbash:matches:active", match_id).await;
    }
}

/// Suspends the current task (polling loops and tests).
pub async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(test)]
mod tests {

    #[test]
    fn key_layout_is_stable() {
        // Documented layout: a rename would orphan running matches.
        assert_eq!(
            format!("lanbash:match:{}:state", "m-1"),
            "lanbash:match:m-1:state"
        );
        assert_eq!(
            format!("lanbash:mode-package:{}", "coin_rush"),
            "lanbash:mode-package:coin_rush"
        );
        assert_eq!(
            format!("lanbash:default-match:{}", "deathmatch"),
            "lanbash:default-match:deathmatch"
        );
    }
}
