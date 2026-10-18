//! Authoritative game simulation.
//!
//! Framework-free and I/O-free: one [`world::World`] per match, advanced one
//! tick at a time. Everything the transport needs (events, snapshots, Lua
//! logs) is drained through explicit methods.

pub mod crc;
pub mod events;
pub mod items;
pub mod markers;
pub mod persist;
pub mod player;
pub mod random;
pub mod round_phase;
pub mod script;
pub mod snapshot;
pub mod world;

pub use events::{Point, SimulationEvent};
pub use player::PlayerState;
pub use round_phase::RoundPhase;
pub use script::{LuaEffects, ScriptEngine, ScriptHost};
pub use world::{LuaLogEntry, World};
