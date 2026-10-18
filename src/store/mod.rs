//! Storage adapters: Redis (realtime state), Postgres (durable state) and the
//! mode library / result recorder on top of them.

pub mod modes;
pub mod redis;
pub mod results;
