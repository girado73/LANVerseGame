//! Blocking access to async stores from the game loop thread.
//!
//! The simulation is synchronous on purpose (Lua must never migrate between
//! threads). Mode lookups and shutdown persistence therefore drive a small
//! current-thread runtime to completion right where they happen. Both are rare
//! (once per created match / on shutdown), never per tick.

use std::sync::Arc;

use crate::store::redis::RedisStore;

/// Runs `future` to completion.
///
/// Inside a multi-threaded runtime (HTTP handlers) the current worker is parked
/// while that runtime drives the future; on the runtime-less game loop thread a
/// throwaway runtime is used.
pub fn run<F: std::future::Future>(future: F) -> Option<F::Output> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => Some(tokio::task::block_in_place(|| handle.block_on(future))),
        Err(_) => {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            Some(runtime.block_on(future))
        }
    }
}

pub fn redis_get(redis: &Arc<RedisStore>, key: &str) -> Option<String> {
    run(redis.get(key)).flatten()
}

pub fn redis_set_ex(redis: &Arc<RedisStore>, key: &str, value: &str, seconds: u64) -> bool {
    run(redis.set_ex(key, value, seconds)).unwrap_or(false)
}

pub fn redis_del(redis: &Arc<RedisStore>, key: &str) -> bool {
    run(redis.del(key)).unwrap_or(false)
}
