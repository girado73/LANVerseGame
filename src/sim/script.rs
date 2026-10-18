//! The interface between the simulation and a Lua mode script.
//!
//! The world owns round timing but a *mode script* decides scoring, markers
//! and when a script-driven round ends. The script never mutates the world:
//! it queues [`LuaEffects`], the world clamps and applies them exactly like
//! client input.
//!
//! This trait keeps the simulation free of any Lua dependency so it can be
//! tested (and replayed against the reference fixtures) without an
//! interpreter.

use serde::{Deserialize, Serialize};

/// Marker as the world stores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub label: String,
    pub kind: String,
}

/// What a script wants to change, recorded per hook call and applied by the
/// world after the hook returned.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LuaEffects {
    pub positions: Vec<PositionEffect>,
    /// Folded score requests, one entry per player and call. `delta` marks
    /// `lb.add_score`, otherwise the value is absolute (`lb.set_score`).
    pub scores: Vec<ScoreEffect>,
    pub hp: Vec<HpEffect>,
    /// Marker requests in call order, each with its reserved id.
    pub markers: Vec<MarkerEffect>,
    pub removed_markers: Vec<i64>,
    pub clear_markers: bool,
    /// Absolute score set by `lb.set_score` folded into `scores`.
    pub winner: Option<String>,
}

impl LuaEffects {
    /// True when the script asked for nothing.
    pub fn is_empty(&self) -> bool {
        self.positions.is_empty()
            && self.scores.is_empty()
            && self.hp.is_empty()
            && self.markers.is_empty()
            && self.removed_markers.is_empty()
            && !self.clear_markers
            && self.winner.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PositionEffect {
    pub id: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ScoreEffect {
    pub id: String,
    pub score: i64,
    pub delta: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct HpEffect {
    pub id: String,
    pub hp: i64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MarkerEffect {
    /// Id reserved by the world when the script asked for the marker.
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub label: String,
    pub kind: String,
}

/// A player as the script sees it (builder field names).
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerView {
    pub id: String,
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub hp: i64,
    pub max_hp: i64,
    pub score: i64,
    pub team: Option<String>,
    pub is_bot: bool,
}

/// Read access and approved intents the script needs while a hook runs.
///
/// This is the complete `lb` API surface. The mlua runtime calls it; a test
/// double can record calls instead.
pub trait ScriptHost {
    fn tick(&self) -> i64;
    fn tick_rate(&self) -> i64;
    fn world_size(&self) -> (f64, f64);
    /// Players in insertion order.
    fn players(&self) -> Vec<PlayerView>;
    fn player(&self, id: &str) -> Option<PlayerView>;
    /// Resolve a script id including the builder aliases `p-local` / `p-me`.
    fn find_player(&self, id: &str) -> Option<PlayerView>;
    fn resolved_id(&self, id: &str) -> String;
    fn markers(&self) -> Vec<Marker>;
    /// Reserve an id for a `spawn_marker` call; the world keeps the counter so
    /// N markers in one tick cannot share an id.
    fn reserve_marker_id(&mut self) -> i64;
    fn log(&mut self, level: &str, text: &str);
    /// Next `lb.random(min, max)` value (inclusive).
    fn random_int(&mut self, min: i64, max: i64) -> i64;
    /// Name of the marker picking kind (`spawn_marker` 4th argument).
    fn marker_limit_reached(&self) -> bool;

    /// Approved changes, collected until the hook returns.
    fn queue_position(&mut self, id: &str, x: f64, y: f64);
    fn current_score(&self, id: &str) -> Option<i64>;
    fn queue_score(&mut self, id: &str, score: i64, delta: bool) -> Option<i64>;
    fn queue_hp(&mut self, id: &str, hp: i64);
    fn queue_marker(&mut self, id: i64, x: f64, y: f64, label: &str, kind: &str);
    fn queue_remove_marker(&mut self, id: i64) -> bool;
    fn queue_clear_markers(&mut self);
    fn queue_finish(&mut self, winner: Option<&str>);
    fn finish_requested(&self) -> bool;
}

/// A mode script (implemented by the mlua runtime and by test doubles).
pub trait ScriptEngine {
    /// Identifier of the loaded package, used for `mode_log`/`mode_error`.
    fn package_id(&self) -> &str;

    /// Lends the world for the next call. Callbacks may need `'static` and
    /// `Send` handles (mlua), so the engine keeps its own owned handle that is
    /// created for exactly one hook call and cleared right after it.
    fn set_host(&mut self, host: Box<dyn ScriptHost + '_>);

    /// Drops the world handle again (called after every hook).
    fn clear_host(&mut self);

    /// First failure message. A faulted script is never called again.
    fn failure(&self) -> Option<&str>;

    fn is_faulted(&self) -> bool {
        self.failure().is_some()
    }

    /// Loads the chunk and runs `on_start`.
    fn start(&mut self);

    /// `on_tick(dt)`.
    fn tick(&mut self, delta_seconds: f64);

    /// `on_input(id, mx, my, action)` for one accepted input.
    fn input(&mut self, player_id: &str, move_x: f64, move_y: f64, action: &str);

    /// `on_finish(winner)`, called at most once by the caller.
    fn finish(&mut self, winner: &str);

    /// Takes the effect set of the last hook call, if any.
    fn take_effects(&mut self) -> Option<LuaEffects>;

    /// Whether the last hook queued anything worth applying.
    ///
    /// Must stay true after `take_effects()`: the world merges host-queued
    /// requests with the engine's own set once per hook.
    fn has_effects(&self) -> bool;

    fn finish_hook_called(&self) -> bool;
    fn mark_finish_hook_called(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_effect_set_is_detected() {
        assert!(LuaEffects::default().is_empty());

        let cleared = LuaEffects {
            clear_markers: true,
            ..LuaEffects::default()
        };
        assert!(!cleared.is_empty());

        let finished = LuaEffects {
            winner: Some("draw".into()),
            ..LuaEffects::default()
        };
        assert!(!finished.is_empty());

        let removed = LuaEffects {
            removed_markers: vec![3],
            ..LuaEffects::default()
        };
        assert!(!removed.is_empty());
    }
}
