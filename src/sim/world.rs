//! The authoritative world for exactly one match.
//!
//! Contract for anything driving this type:
//!
//! * The world performs **no I/O**. No database, no Redis, no clock. Timing is
//!   expressed in ticks, so a world is deterministic apart from the supply of
//!   inputs and late connections.
//! * Clients only ever contribute inputs. Positions, velocities, hit results,
//!   scores, cooldowns and round outcomes are computed here.
//! * `advance()` steps exactly one tick.
//!
//! This is a line-by-line port of the former PHP `World`; the parity fixtures
//! in `tests/parity/fixtures` compare every observable value against it.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::SimulationConfig;
use crate::sim::crc::php_crc32_index;
use crate::sim::events::{Point, SimulationEvent};
use crate::sim::items::{ItemKind, ItemState};
use crate::sim::markers::MarkerSequence;
use crate::sim::player::PlayerState;
use crate::sim::round_phase::RoundPhase;
use crate::sim::script::{
    HpEffect, LuaEffects, Marker, MarkerEffect, PlayerView, PositionEffect, ScoreEffect,
    ScriptEngine, ScriptHost,
};

/// Maximum number of markers a match may hold.
pub const MAX_MARKERS: usize = 256;
/// Maximum length of a marker label.
pub const MAX_MARKER_LABEL_LENGTH: usize = 24;
/// Maximum length of a marker kind.
pub const MAX_MARKER_KIND_LENGTH: usize = 16;
/// Highest score a script can set.
pub const MAX_SCORE: i64 = 1_000_000_000;
/// Number of spawn positions on the spawn ring.
const SPAWN_SLOTS: usize = 12;
/// Spawn ring radius as a fraction of the world size.
const SPAWN_RADIUS_FRACTION: f64 = 0.3;
/// Entries the Lua console may queue before it drops messages.
const MAX_LUA_LOGS: usize = 100;

/// One line of Lua console output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LuaLogEntry {
    pub level: String,
    pub text: String,
    pub tick: i64,
}

/// Result of attaching a script (health probe).
#[derive(Debug, Clone, PartialEq)]
pub struct AttachResult {
    pub ok: bool,
    pub failure: Option<String>,
}

/// PHP `mb_substr` for the BMP: count characters, keep at most `len`.
pub(crate) fn truncate_chars(text: &str, len: usize) -> String {
    text.chars().take(len).collect()
}

pub struct World<S: ScriptEngine = crate::lua::runtime::LuaScript> {
    pub match_id: String,
    pub config: SimulationConfig,
    pub tick: i64,
    team_count: i64,

    players: Vec<PlayerState>,
    player_index: HashMap<String, usize>,
    pub(crate) items: Vec<ItemState>,
    pending_events: Vec<SimulationEvent>,
    /// Team memory so a reconnect keeps its team.
    pub(crate) teams: HashMap<String, String>,

    /// Lua mode script driving this match, if any.
    pub script: Option<S>,
    /// Set while `advance()` runs for a mode script.
    script_driven: bool,
    lua_logs: Vec<LuaLogEntry>,
    pub(crate) markers: Vec<Marker>,
    marker_ids: MarkerSequence,

    /// Intents collected while a hook runs; applied once it returned.
    collected: LuaEffects,
    /// `lb.log_clear()` seen during the current hook.
    log_clear_this_hook: bool,
    /// Set when a script queued an HP change; score requests after it are
    /// ignored in the same hook (reference behaviour, see `queue_score`).
    hp_queued_this_hook: bool,
    /// Player positioned by a script since its last movement step.
    noclip_player: Option<String>,

    /// Countdown from the mode manifest (script-driven).
    mode_countdown_ms: i64,
    /// Built-in round length from the mode manifest (script-driven).
    mode_duration_ms: i64,
    reported_script_failure: Option<String>,
    script_pending: bool,

    pub phase: RoundPhase,
    pub round_number: i64,
    pub round_id: Option<String>,
    pub winner: Option<String>,

    pub(crate) countdown_ends_at_tick: i64,
    pub(crate) round_ends_at_tick: i64,
    pub(crate) finished_at_tick: i64,
    speed_multiplier: f64,
    speed_boost_until_tick: i64,
}

impl<S: ScriptEngine> std::fmt::Debug for World<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("World")
            .field("match_id", &self.match_id)
            .field("tick", &self.tick)
            .field("phase", &self.phase)
            .field("players", &self.players.len())
            .field("markers", &self.markers.len())
            .field(
                "script",
                &self
                    .script
                    .as_ref()
                    .map(|script| script.package_id().to_owned()),
            )
            .finish()
    }
}

impl<S: ScriptEngine> World<S> {
    /// Creates a world with items spawned and a marker counter at 1.
    pub fn new(match_id: impl Into<String>, config: SimulationConfig) -> Self {
        Self::with_teams(match_id, config, 0, 2)
    }

    /// Creates a world with an explicit starting tick and team count.
    pub fn with_teams(
        match_id: impl Into<String>,
        config: SimulationConfig,
        tick: i64,
        team_count: i64,
    ) -> Self {
        let speed_multiplier = config.gameplay.items.speed_multiplier;
        let mut world = Self {
            match_id: match_id.into(),
            config,
            tick,
            team_count,
            players: Vec::new(),
            player_index: HashMap::new(),
            items: Vec::new(),
            pending_events: Vec::new(),
            teams: HashMap::new(),
            script: None,
            script_driven: false,
            lua_logs: Vec::new(),
            markers: Vec::new(),
            marker_ids: MarkerSequence::new(),
            collected: LuaEffects::default(),
            log_clear_this_hook: false,
            hp_queued_this_hook: false,
            noclip_player: None,
            mode_countdown_ms: 3_000,
            mode_duration_ms: 180_000,
            reported_script_failure: None,
            script_pending: false,
            phase: RoundPhase::Lobby,
            round_number: 0,
            round_id: None,
            winner: None,
            countdown_ends_at_tick: 0,
            round_ends_at_tick: 0,
            finished_at_tick: 0,
            speed_multiplier,
            speed_boost_until_tick: 0,
        };

        world.spawn_items();
        world
    }

    /// A world with the `inert` geometry used for upload validation: no items,
    /// no countdown, 60 Hz.
    pub fn inert(match_id: impl Into<String>) -> Self {
        Self::new(
            match_id,
            SimulationConfig::from_parts(64, 64, 8, 30, crate::config::GameplayConfig::inert()),
        )
    }

    // -----------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------

    pub fn world_width(&self) -> f64 {
        self.config.gameplay.world.width
    }

    pub fn world_height(&self) -> f64 {
        self.config.gameplay.world.height
    }

    pub fn players(&self) -> &[PlayerState] {
        &self.players
    }

    pub fn items(&self) -> &[ItemState] {
        &self.items
    }

    /// Mutable roster access, for restoring scenario setups in tests.
    pub fn players_mut(&mut self) -> &mut [PlayerState] {
        &mut self.players
    }

    pub fn player(&self, id: &str) -> Option<&PlayerState> {
        self.player_index
            .get(id)
            .and_then(|index| self.players.get(*index))
    }

    pub fn marker(&self, id: i64) -> Option<&Marker> {
        self.markers.iter().find(|x| x.id == id)
    }

    pub fn player_mut(&mut self, id: &str) -> Option<&mut PlayerState> {
        match self.player_index.get(id) {
            Some(index) => self.players.get_mut(*index),
            None => None,
        }
    }

    pub fn has_player(&self, id: &str) -> bool {
        self.player_index.contains_key(id)
    }

    pub fn player_count(&self) -> usize {
        self.players.len()
    }

    pub fn player_sees_player(&self, watcher_id: &str, watched_id: &str) -> bool {
        let (Some(watcher), Some(watched)) = (
            World::player(self, watcher_id),
            World::player(self, watched_id),
        ) else {
            return false;
        };

        let dx = watched.x - watcher.x;
        let dy = watched.y - watcher.y;
        let distance = dx.hypot(dy);
        let facing_length = watcher.facing_x.hypot(watcher.facing_y);

        if !distance.is_finite()
            || distance == 0.0
            || !facing_length.is_finite()
            || facing_length <= 0.0
        {
            return false;
        }

        let alignment = (dx / distance) * (watcher.facing_x / facing_length)
            + (dy / distance) * (watcher.facing_y / facing_length);

        alignment >= 22.5_f64.to_radians().cos()
    }

    pub fn player_sees_marker(&self, player_id: &str, marker_id: i64) -> bool {
        let (Some(player), Some(marker)) = (
            World::player(self, player_id),
            World::marker(self, marker_id),
        ) else {
            return false;
        };

        let dx = marker.x - player.x;
        let dy = marker.y - player.y;
        let distance = dx.hypot(dy);
        let facing_length = player.facing_x.hypot(player.facing_y);

        if !distance.is_finite()
            || distance == 0.0
            || !facing_length.is_finite()
            || facing_length <= 0.0
        {
            return false;
        }

        let alignment = (dx / distance) * (player.facing_x / facing_length)
            + (dy / distance) * (player.facing_y / facing_length);

        alignment >= 22.5_f64.to_radians().cos()
    }

    pub fn is_empty(&self) -> bool {
        self.players.is_empty()
    }

    pub fn is_joinable(&self) -> bool {
        self.phase != RoundPhase::Finished && (self.players.len() as i64) < self.config.max_players
    }

    pub fn markers(&self) -> &[Marker] {
        &self.markers
    }

    pub fn next_marker_id(&self) -> i64 {
        self.marker_ids.peek()
    }

    /// Keeps the marker counter above an imported id.
    pub fn markers_observe(&mut self, id: i64) {
        self.marker_ids.observe(id);
    }

    pub fn countdown_ends_at_tick(&self) -> i64 {
        self.countdown_ends_at_tick
    }

    pub fn round_ends_at_tick(&self) -> i64 {
        self.round_ends_at_tick
    }

    pub fn finished_at_tick(&self) -> i64 {
        self.finished_at_tick
    }

    pub fn team_count(&self) -> i64 {
        self.team_count
    }

    pub(crate) fn teams_map(&self) -> serde_json::Map<String, serde_json::Value> {
        self.teams
            .iter()
            .map(|(id, team)| (id.clone(), serde_json::Value::from(team.clone())))
            .collect()
    }

    /// Replaces the roster (restore). Keeps insertion order and the index.
    pub fn replace_players(&mut self, players: Vec<PlayerState>) {
        self.player_index.clear();
        for (index, player) in players.iter().enumerate() {
            self.player_index.insert(player.id.clone(), index);
        }
        self.players = players;
    }

    pub fn reserve_marker_id(&mut self) -> i64 {
        self.marker_ids.take()
    }

    pub fn is_script_driven(&self) -> bool {
        self.script
            .as_ref()
            .is_some_and(|script| !script.is_faulted())
    }

    pub fn script_faulted(&self) -> bool {
        self.script
            .as_ref()
            .is_some_and(|script| script.is_faulted())
    }

    pub fn round_id(&self) -> Option<&str> {
        self.round_id.as_deref()
    }

    pub fn time_left_ms(&self) -> i64 {
        if self.phase != RoundPhase::Running {
            return 0;
        }
        ticks_to_ms(
            self.round_ends_at_tick - self.tick,
            self.config.tick_duration_ms,
        )
    }

    pub fn countdown_ms(&self) -> i64 {
        if self.phase != RoundPhase::Countdown {
            return 0;
        }
        ticks_to_ms(
            self.countdown_ends_at_tick - self.tick,
            self.config.tick_duration_ms,
        )
    }

    /// Team totals in first-seen order (PHP array insertion order).
    pub fn team_scores(&self) -> Vec<(String, i64)> {
        let mut scores: Vec<(String, i64)> = Vec::new();

        for player in &self.players {
            if let Some(team) = &player.team {
                match scores.iter_mut().find(|(name, _)| name == team) {
                    Some(entry) => entry.1 += player.score,
                    None => scores.push((team.clone(), player.score)),
                }
            }
        }

        scores
    }

    /// Team totals as a JSON object that keeps first-seen order.
    pub fn team_scores_json(&self) -> serde_json::Map<String, serde_json::Value> {
        self.team_scores()
            .into_iter()
            .map(|(team, score)| (team, serde_json::Value::from(score)))
            .collect()
    }

    /// Drains queued events; the game loop turns them into wire messages.
    pub fn drain_events(&mut self) -> Vec<SimulationEvent> {
        std::mem::take(&mut self.pending_events)
    }

    /// Script console output (`lb.log`, `print`). Drained by the game loop.
    pub fn lua_log(&mut self, level: &str, text: &str) {
        if self.lua_logs.len() >= MAX_LUA_LOGS {
            return;
        }

        self.lua_logs.push(LuaLogEntry {
            level: level.to_owned(),
            text: truncate_chars(text, 500),
            tick: self.tick,
        });
    }

    /// Asks clients to discard the script console (`lb.log_clear()`).
    ///
    /// The lines a client shows are already drained and broadcast, so this is
    /// an event like a score change. It is queued in `pending_events`, which
    /// keeps the order: everything logged before the clear in the same tick
    /// still reaches the client first.
    pub fn clear_lua_log(&mut self) {
        self.pending_events.push(SimulationEvent::ModeLogClear);
    }

    pub fn drain_lua_logs(&mut self) -> Vec<LuaLogEntry> {
        std::mem::take(&mut self.lua_logs)
    }

    // -----------------------------------------------------------------
    // Round timings
    // -----------------------------------------------------------------

    pub fn mode_countdown_ms(&self) -> i64 {
        self.mode_countdown_ms
    }

    /// Overrides round timings for a mode script (milliseconds).
    pub fn with_round_timings(&mut self, countdown_ms: i64, duration_ms: i64) {
        self.mode_countdown_ms = countdown_ms.max(0);
        self.mode_duration_ms = duration_ms.max(1_000);
    }

    // -----------------------------------------------------------------
    // Connections
    // -----------------------------------------------------------------

    /// Adds a player at a spawn point. Returns false when the world is full or
    /// the id is already taken.
    pub fn add_player(
        &mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        user_id: Option<i64>,
    ) -> bool {
        let id = id.into();

        if self.has_player(&id) || (self.players.len() as i64) >= self.config.max_players {
            return false;
        }

        let (x, y) = self.spawn_point_for(&id);
        let mut player =
            PlayerState::new(id.clone(), name, x, y, self.config.gameplay.player.max_hp);
        player.user_id = user_id;
        player.joined_at_tick = self.tick;
        player.team = if self.team_count > 0 {
            Some(self.assign_team(&id))
        } else {
            None
        };

        self.player_index.insert(id, self.players.len());
        self.players.push(player);

        // No join event here: only the game loop knows whether this is a fresh
        // join or a reconnect replacing a ghost.
        self.maybe_start_countdown();

        true
    }

    pub fn remove_player(&mut self, id: &str) {
        let Some(index) = self.player_index.remove(id) else {
            return;
        };

        self.players.remove(index);

        for value in self.player_index.values_mut() {
            if *value > index {
                *value -= 1;
            }
        }

        self.pending_events.push(SimulationEvent::PlayerLeft {
            player_id: id.to_owned(),
        });
    }

    // -----------------------------------------------------------------
    // Input
    // -----------------------------------------------------------------

    /// Applies a client input. Stale or duplicate sequences are dropped,
    /// movement is only stored as intent and actions become edge-triggered
    /// requests.
    pub fn apply_input(
        &mut self,
        player_id: &str,
        sequence: i64,
        movement_x: f64,
        movement_y: f64,
        actions: &[String],
    ) {
        let Some(index) = self.player_index.get(player_id).copied() else {
            return;
        };

        if sequence <= self.players[index].acked_input_sequence {
            return;
        }

        self.players[index].acked_input_sequence = sequence;

        if !self.phase.accepts_gameplay_input() || !self.players[index].is_alive {
            self.players[index].reset_movement_intent();
            return;
        }

        // Non-finite intent would poison positions; the decoder neutralizes it,
        // the world does not rely on that.
        let movement_x = if movement_x.is_finite() {
            movement_x
        } else {
            0.0
        };
        let movement_y = if movement_y.is_finite() {
            movement_y
        } else {
            0.0
        };

        self.players[index].input_x = movement_x;
        self.players[index].input_y = movement_y;
        self.players[index].has_movement_intent = movement_x != 0.0 || movement_y != 0.0;

        // Requests are applied at input time, exactly like the reference (the
        // tick budget depends on it). The service accepts at most one input per
        // connection and tick, so a fast client cannot replace its own
        // movement intent before the tick consumes it.
        let has_intent = self.players[index].has_movement_intent;
        for action in actions {
            match action.as_str() {
                "dash" => self.request_dash(player_id, has_intent),
                "jump" => self.request_jump(player_id),
                "interact" => self.players[index].wants_attack = true,
                _ => {}
            }
        }

        self.forward_input_to_script(player_id, movement_x, movement_y, actions);
    }

    /// Hands accepted input to the mode script. Only the *last* action of a
    /// frame is reported so one call maps to one action string.
    fn forward_input_to_script(
        &mut self,
        player_id: &str,
        movement_x: f64,
        movement_y: f64,
        actions: &[String],
    ) {
        if !self.is_script_driven() || self.phase != RoundPhase::Running {
            return;
        }

        let action = actions.last().cloned().unwrap_or_default();
        let player_id = player_id.to_owned();
        self.run_hook(|script| {
            script.input(&player_id, movement_x, movement_y, &action);
        });
    }

    // -----------------------------------------------------------------
    // Tick
    // -----------------------------------------------------------------

    /// Steps the world forward by exactly one tick.
    pub fn advance(&mut self) {
        self.tick += 1;
        self.script_driven = self.is_script_driven();

        match self.phase {
            RoundPhase::Lobby => self.advance_lobby(),
            RoundPhase::Countdown => self.advance_countdown(),
            RoundPhase::Running => self.advance_running(),
            RoundPhase::Finished => self.advance_finished(),
            RoundPhase::Connecting => {}
        }

        self.advance_script();
    }

    fn advance_lobby(&mut self) {
        if self.tick % self.config.tick_rate != 0 {
            return;
        }

        self.maybe_start_countdown();
    }

    fn advance_countdown(&mut self) {
        if self.tick < self.countdown_ends_at_tick {
            return;
        }

        if (self.players.len() as i64) < self.min_players() {
            self.phase = RoundPhase::Lobby;
            return;
        }

        self.begin_round();
    }

    fn advance_running(&mut self) {
        // A script has ended the round from `on_tick`; do nothing else this
        // tick so its result sticks until the normal reset.
        if self.script_driven && self.phase != RoundPhase::Running {
            return;
        }

        if (self.players.len() as i64) < self.min_players() {
            self.return_to_lobby();
            return;
        }

        if self.tick >= self.round_ends_at_tick {
            self.finish_round();
            return;
        }

        for index in 0..self.players.len() {
            if !self.players[index].is_alive {
                if self.tick >= self.players[index].respawn_at_tick {
                    self.respawn(index);
                }
                continue;
            }

            self.step_player(index);
        }

        self.resolve_items();
    }

    fn advance_finished(&mut self) {
        if self.script_driven && self.phase != RoundPhase::Finished {
            return;
        }

        if self.tick >= self.finished_at_tick {
            self.return_to_lobby();
        }
    }

    /// One script step, run once per tick after the built-in rules:
    ///
    /// 1. the pending `on_start` runs and its effects apply;
    /// 2. `on_tick` runs and its effects apply;
    /// 3. `on_finish` runs once a winner is known.
    ///
    /// `on_input` is dispatched from [`World::apply_input`] so a script sees the
    /// input that was actually accepted for this tick.
    fn advance_script(&mut self) {
        if self.script.is_none() {
            return;
        }

        if self.script_pending {
            self.script_pending = false;
            self.run_hook(|script| script.start());

            // A script that started this tick also begins its round now.
            if self.phase == RoundPhase::Lobby {
                self.maybe_start_countdown();
            }
        } else if self.script_faulted() {
            return;
        }

        if self.phase == RoundPhase::Running && !self.script_faulted() {
            let delta = 1.0 / self.config.tick_rate as f64;
            self.run_hook(|script| script.tick(delta));
        }

        self.notify_finished();
    }

    /// Runs one hook, applies whatever it queued and records a failure once.
    ///
    /// The engine gets an owned [`ScriptHost`] handle for the call. It is the
    /// same world (a view over the same fields), never a copy, so effects land
    /// in the running match.
    fn run_hook(&mut self, hook: impl FnOnce(&mut S)) {
        let Some(mut script) = self.script.take() else {
            return;
        };

        self.collected = LuaEffects::default();
        self.log_clear_this_hook = false;
        self.hp_queued_this_hook = false;

        // The engine may erase the handle's lifetime (Lua callbacks must be
        // `'static`), but it never keeps it past `clear_host`, and this borrow
        // of `self` lasts exactly as long as the hook call.
        let view = WorldView { world: self };
        script.set_host(Box::new(view));
        hook(&mut script);
        script.clear_host();

        // A hook that already took its effects (the input hook only applies
        // them while the round runs) must not leave stale requests behind.
        if !script.has_effects() {
            self.collected = LuaEffects::default();
        }

        // `take_effects` empties the set, so never ask `has_effects` first.
        if let Some(effects) = script.take_effects() {
            let mut merge = std::mem::take(&mut self.collected);
            merge.scores.extend(effects.scores);
            merge.positions.extend(effects.positions);
            merge.hp.extend(effects.hp);
            merge.markers.extend(effects.markers);
            merge.removed_markers.extend(effects.removed_markers);
            merge.clear_markers |= effects.clear_markers;
            merge.winner = effects.winner.or(merge.winner);
            self.collected = merge;
        }

        self.script = Some(script);

        let effects = std::mem::take(&mut self.collected);
        self.apply_lua_effects(&effects);

        if std::mem::take(&mut self.log_clear_this_hook) {
            self.clear_lua_log();
        }
        self.report_script_failure();
    }

    /// `on_finish(winner)`, called at most once after a winner is known.
    fn notify_finished(&mut self) {
        let Some(script) = self.script.as_ref() else {
            return;
        };

        if script.finish_hook_called() {
            return;
        }

        let Some(winner) = self.winner.clone() else {
            return;
        };

        if let Some(script) = self.script.as_mut() {
            script.mark_finish_hook_called();
        }

        self.run_hook(|script| script.finish(&winner));
    }

    /// Records a script failure once. The game loop drains the log, reports it
    /// and notifies the players; the world itself keeps running.
    fn report_script_failure(&mut self) {
        if self.reported_script_failure.is_some() {
            return;
        }

        let Some(failure) = self.script.as_ref().and_then(|script| script.failure()) else {
            return;
        };

        let failure = failure.to_owned();
        self.reported_script_failure = Some(failure.clone());
        self.lua_log("error", &failure);
    }

    // -----------------------------------------------------------------
    // Script attachment
    // -----------------------------------------------------------------

    /// Attaches a mode script and (with `prepare`) runs its `on_start` right
    /// away, which the game loop uses as a health probe.
    pub fn attach_script(&mut self, script: S, prepare: bool) -> AttachResult {
        self.script = Some(script);

        if !prepare {
            self.script_pending = true;
            return AttachResult {
                ok: true,
                failure: None,
            };
        }

        self.run_hook(|script| script.start());

        AttachResult {
            ok: !self.script_faulted(),
            failure: self
                .script
                .as_ref()
                .and_then(|script| script.failure())
                .map(str::to_owned),
        }
    }

    /// Applies the intents a script queued. Same clamps as any other change:
    /// positions stay inside the world, HP inside 0..max (=> death), scores
    /// clamped to a sane range.
    fn apply_lua_effects(&mut self, effects: &LuaEffects) {
        for PositionEffect { id, x, y } in &effects.positions {
            let width = self.world_width();
            let height = self.world_height();
            if let Some(index) = self.player_index.get(id).copied() {
                let x = clamp(*x, 0.0, width);
                let y = clamp(*y, 0.0, height);
                // A scripted position is authoritative: the next movement step
                // must not clamp it again by the player radius (a script
                // placing someone at the edge would otherwise get shoved
                // inwards without any input). Velocity is cleared so the
                // placement does not carry stale momentum.
                self.players[index].x = x;
                self.players[index].y = y;
                self.players[index].velocity_x = 0.0;
                self.players[index].velocity_y = 0.0;
                self.noclip_player = Some(id.clone());
            }
        }

        for HpEffect { id, hp } in &effects.hp {
            let Some(index) = self.player_index.get(id).copied() else {
                continue;
            };

            let old_hp = self.players[index].hp;
            let alive = self.players[index].is_alive;
            let value = (*hp).clamp(0, self.players[index].max_hp);

            if value < old_hp && alive {
                let position = Point::new(self.players[index].x, self.players[index].y);
                self.pending_events.push(SimulationEvent::PlayerHit {
                    attacker: "mode".into(),
                    victim: id.clone(),
                    damage: old_hp - value,
                    remaining_hp: value,
                    position,
                });
            }

            if value == 0 && alive {
                self.kill_player(index, None);
            }

            self.players[index].hp = value;
        }

        for ScoreEffect { id, score, delta } in &effects.scores {
            let Some(index) = self.player_index.get(id).copied() else {
                continue;
            };

            let target = if *delta {
                self.players[index].score.saturating_add(*score)
            } else {
                *score
            };
            self.players[index].score = target.clamp(0, MAX_SCORE);
            let team = self.players[index].team.clone();
            self.pending_events.push(SimulationEvent::ScoreSet {
                player_id: id.clone(),
                score: self.players[index].score,
                team,
            });
        }

        if effects.clear_markers {
            self.markers.clear();
        }

        for id in &effects.removed_markers {
            self.markers.retain(|marker| marker.id != *id);
        }

        for MarkerEffect {
            id: reserved,
            x,
            y,
            label,
            kind,
        } in &effects.markers
        {
            if self.markers.len() >= MAX_MARKERS {
                break;
            }

            // The id was reserved when the script asked for the marker; a
            // missing reservation gets a fresh one so ids stay unique.
            let id = if *reserved > 0 {
                *reserved
            } else {
                self.marker_ids.take()
            };
            self.marker_ids.observe(id);

            let width = self.world_width();
            let height = self.world_height();
            self.markers.push(Marker {
                id,
                x: clamp(*x, 0.0, width),
                y: clamp(*y, 0.0, height),
                label: label.clone(),
                kind: kind.clone(),
            });
        }

        if let Some(winner) = &effects.winner
            && self.script_driven
            && self.phase != RoundPhase::Finished
        {
            self.script_finished(winner);
        }
    }

    fn script_finished(&mut self, winner: &str) {
        self.phase = RoundPhase::Finished;
        self.winner = Some(winner.to_owned());
        self.finished_at_tick =
            self.tick + self.ticks_for_ms(self.config.gameplay.round.finished_ms);
    }

    // -----------------------------------------------------------------
    // Phase transitions
    // -----------------------------------------------------------------

    fn maybe_start_countdown(&mut self) {
        // Script-driven modes own countdown, duration and the end of a round.
        if self.is_script_driven() {
            self.start_script_round();
            return;
        }

        if self.phase != RoundPhase::Lobby || (self.players.len() as i64) < self.min_players() {
            return;
        }

        self.phase = RoundPhase::Countdown;
        self.round_number += 1;
        self.round_id = Some(format!("r-{}", self.round_number));
        self.winner = None;
        self.countdown_ends_at_tick =
            self.tick + self.ticks_for_ms(self.config.gameplay.round.countdown_ms);
    }

    fn start_script_round(&mut self) {
        if self.phase != RoundPhase::Lobby || (self.players.len() as i64) < self.min_players() {
            return;
        }

        self.phase = RoundPhase::Countdown;
        self.round_number += 1;
        self.round_id = Some(format!("r-{}", self.round_number));
        self.winner = None;
        self.countdown_ends_at_tick = self.tick + self.ticks_for_ms(self.mode_countdown_ms);

        if self.mode_countdown_ms <= 0 {
            self.countdown_ends_at_tick = self.tick + 1;
        }
    }

    fn begin_round(&mut self) {
        self.phase = RoundPhase::Running;
        let duration_ms = if self.is_script_driven() {
            self.mode_duration_ms
        } else {
            self.config.gameplay.round.duration_ms
        };
        self.round_ends_at_tick = self.tick + self.ticks_for_ms(duration_ms);

        for player in &mut self.players {
            player.reset_for_round();
        }
    }

    fn finish_round(&mut self) {
        self.phase = RoundPhase::Finished;
        self.winner = self.compute_winner();
        self.finished_at_tick =
            self.tick + self.ticks_for_ms(self.config.gameplay.round.finished_ms);
    }

    fn return_to_lobby(&mut self) {
        self.phase = RoundPhase::Lobby;
        self.winner = None;

        for player in &mut self.players {
            player.reset_for_round();
        }

        self.maybe_start_countdown();
    }

    /// Highest team score, or the best player in free-for-all. Ties follow the
    /// reference behaviour (`arsort` is stable, so the first team in roster
    /// order wins; the first player to reach the best score wins).
    fn compute_winner(&self) -> Option<String> {
        let scores = self.team_scores();

        if !scores.is_empty() {
            let mut best: Option<(String, i64)> = None;
            for (team, score) in scores {
                if best
                    .as_ref()
                    .is_none_or(|(_, best_score)| score > *best_score)
                {
                    best = Some((team, score));
                }
            }
            return best.map(|(team, _)| team);
        }

        let mut best: Option<(String, i64)> = None;
        for player in &self.players {
            if best
                .as_ref()
                .is_none_or(|(_, best_score)| player.score > *best_score)
            {
                best = Some((player.id.clone(), player.score));
            }
        }

        best.map(|(id, _)| id)
    }

    // -----------------------------------------------------------------
    // Player actions (server-validated)
    // -----------------------------------------------------------------

    fn request_dash(&mut self, player_id: &str, has_intent: bool) {
        let tick = self.tick;
        let duration = self.ticks_for_ms(self.config.gameplay.player.dash_duration_ms);
        let cooldown = self.ticks_for_ms(self.config.gameplay.player.dash_cooldown_ms);
        let Some(index) = self.player_index.get(player_id).copied() else {
            return;
        };

        if tick < self.players[index].dash_ready_at_tick {
            return;
        }

        // A dash without a direction would be a free teleport.
        if !has_intent {
            return;
        }

        self.players[index].dash_until_tick = tick + duration;
        self.players[index].dash_ready_at_tick = tick + cooldown;
    }

    fn request_jump(&mut self, player_id: &str) {
        let tick = self.tick;
        let duration = self.ticks_for_ms(self.config.gameplay.player.jump_duration_ms);
        let cooldown = self.ticks_for_ms(self.config.gameplay.player.jump_cooldown_ms);
        let Some(index) = self.player_index.get(player_id).copied() else {
            return;
        };

        if tick < self.players[index].jump_ready_at_tick {
            return;
        }

        self.players[index].jump_until_tick = tick + duration;
        self.players[index].jump_ready_at_tick = tick + cooldown;
    }

    // -----------------------------------------------------------------
    // Movement and combat
    // -----------------------------------------------------------------

    fn step_player(&mut self, index: usize) {
        let player_config = self.config.gameplay.player;
        let mut speed = player_config.move_speed;

        if self.tick < self.speed_boost_until_tick {
            speed *= self.speed_multiplier;
        }

        {
            let player = &mut self.players[index];

            if player.is_dashing(self.tick) {
                speed = player_config.dash_speed;
            } else if player.is_jumping(self.tick) {
                speed *= 0.5;
            }

            player.velocity_x = player.input_x * speed;
            player.velocity_y = player.input_y * speed;

            if player.has_movement_intent {
                player.facing_x = player.input_x;
                player.facing_y = player.input_y;
            }

            let delta_seconds = self.config.tick_duration_seconds;
            player.x += player.velocity_x * delta_seconds;
            player.y += player.velocity_y * delta_seconds;
        }

        // A position set by a script this tick is kept as-is.
        if self.noclip_player.as_deref() == Some(self.players[index].id.as_str()) {
            self.noclip_player = None;
        } else {
            self.clamp_to_world(index);
        }

        if self.players[index].wants_attack {
            self.resolve_attack(index);
        }

        self.players[index].reset_movement_intent();
    }

    fn resolve_attack(&mut self, attacker_index: usize) {
        self.players[attacker_index].wants_attack = false;

        if self.tick < self.players[attacker_index].attack_ready_at_tick {
            return;
        }

        let combat = self.config.gameplay.combat;
        self.players[attacker_index].attack_ready_at_tick =
            self.tick + self.ticks_for_ms(combat.attack_cooldown_ms);

        let range_squared = combat.attack_range * combat.attack_range;
        let cos_arc = (combat.attack_arc_degrees.to_radians() / 2.0).cos();

        let attacker_x = self.players[attacker_index].x;
        let attacker_y = self.players[attacker_index].y;
        let facing_x = self.players[attacker_index].facing_x;
        let facing_y = self.players[attacker_index].facing_y;

        let mut victim: Option<usize> = None;
        let mut best_distance = f64::INFINITY;

        for (index, candidate) in self.players.iter().enumerate() {
            if index == attacker_index
                || !candidate.is_alive
                || is_friendly(&self.players[attacker_index], candidate)
            {
                continue;
            }

            let dx = candidate.x - attacker_x;
            let dy = candidate.y - attacker_y;
            let distance_squared = dx * dx + dy * dy;

            if distance_squared == 0.0
                || distance_squared > range_squared
                || distance_squared >= best_distance
            {
                continue;
            }

            let distance = distance_squared.sqrt();
            let alignment = (dx / distance) * facing_x + (dy / distance) * facing_y;

            if alignment < cos_arc {
                continue;
            }

            victim = Some(index);
            best_distance = distance_squared;
        }

        let Some(victim) = victim else {
            return;
        };

        let damage = combat.damage;
        self.players[victim].hp = (self.players[victim].hp - damage).max(0);

        let position = Point::new(self.players[victim].x, self.players[victim].y);
        let remaining_hp = self.players[victim].hp;
        self.pending_events.push(SimulationEvent::PlayerHit {
            attacker: self.players[attacker_index].id.clone(),
            victim: self.players[victim].id.clone(),
            damage,
            remaining_hp,
            position,
        });

        if remaining_hp > 0 {
            return;
        }

        self.kill_player(victim, Some(attacker_index));
    }

    /// `killer` is `None` when a mode script removed the HP.
    fn kill_player(&mut self, victim: usize, killer: Option<usize>) {
        self.players[victim].is_alive = false;
        self.players[victim].deaths += 1;
        self.players[victim].velocity_x = 0.0;
        self.players[victim].velocity_y = 0.0;
        self.players[victim].respawn_at_tick =
            self.tick + self.ticks_for_ms(self.config.gameplay.round.respawn_ms);

        if let Some(killer) = killer {
            self.players[killer].score += self.config.gameplay.combat.score_per_kill;
            let team = self.players[killer].team.clone();
            self.pending_events.push(SimulationEvent::ScoreChanged {
                player_id: self.players[killer].id.clone(),
                score: self.players[killer].score,
                team,
            });
        }

        self.pending_events.push(SimulationEvent::PlayerDied {
            player_id: self.players[victim].id.clone(),
            killer: killer
                .map(|index| self.players[index].id.clone())
                .unwrap_or_else(|| "mode".to_owned()),
        });
    }

    fn resolve_items(&mut self) {
        let items = self.config.gameplay.items;
        let pickup_radius_squared = items.pickup_radius * items.pickup_radius;
        let tick = self.tick;

        for item_index in 0..self.items.len() {
            if !self.items[item_index].available {
                if tick >= self.items[item_index].respawn_at_tick {
                    self.items[item_index].available = true;
                }
                continue;
            }

            let item_x = self.items[item_index].x;
            let item_y = self.items[item_index].y;

            for player_index in 0..self.players.len() {
                if !self.players[player_index].is_alive {
                    continue;
                }

                let dx = item_x - self.players[player_index].x;
                let dy = item_y - self.players[player_index].y;

                if dx * dx + dy * dy > pickup_radius_squared {
                    continue;
                }

                self.items[item_index].available = false;
                self.items[item_index].respawn_at_tick = tick + self.ticks_for_ms(items.respawn_ms);

                match self.items[item_index].kind {
                    ItemKind::Heal => {
                        let max_hp = self.players[player_index].max_hp;
                        self.players[player_index].hp = self.players[player_index]
                            .hp
                            .saturating_add(items.heal_amount)
                            .min(max_hp);
                    }
                    ItemKind::Speed => {
                        self.speed_boost_until_tick =
                            tick + self.ticks_for_ms(items.speed_boost_ms);
                    }
                }

                self.pending_events.push(SimulationEvent::ItemPicked {
                    player_id: self.players[player_index].id.clone(),
                    item_id: self.items[item_index].id.clone(),
                });

                break;
            }
        }
    }

    fn respawn(&mut self, index: usize) {
        let (mut x, mut y) = self.spawn_point_for(&self.players[index].id);

        // Script-driven modes place respawns themselves (e.g. at a spawn zone).
        if self.script_driven {
            x = self.players[index].x;
            y = self.players[index].y;
        }

        let max_hp = self.players[index].max_hp;
        let player = &mut self.players[index];
        player.x = x;
        player.y = y;
        player.hp = max_hp;
        player.is_alive = true;
        player.velocity_x = 0.0;
        player.velocity_y = 0.0;
    }

    // -----------------------------------------------------------------
    // Geometry
    // -----------------------------------------------------------------

    fn clamp_to_world(&mut self, index: usize) {
        let radius = self.config.gameplay.player.radius;
        let max_x = self.world_width() - radius;
        let max_y = self.world_height() - radius;

        let player = &mut self.players[index];

        if player.x < radius || player.x > max_x {
            player.x = player.x.max(radius).min(max_x);
            player.velocity_x = 0.0;
        }

        if player.y < radius || player.y > max_y {
            player.y = player.y.max(radius).min(max_y);
            player.velocity_y = 0.0;
        }
    }

    /// Deterministic spawn ring so late joiners never stack on one spot.
    fn spawn_point_for(&self, player_id: &str) -> (f64, f64) {
        let index = php_crc32_index(player_id, SPAWN_SLOTS);
        let angle = (index as f64 / SPAWN_SLOTS as f64) * 2.0 * std::f64::consts::PI;

        let width = self.world_width();
        let height = self.world_height();

        (
            width / 2.0 + angle.cos() * (width * SPAWN_RADIUS_FRACTION),
            height / 2.0 + angle.sin() * (height * SPAWN_RADIUS_FRACTION),
        )
    }

    fn assign_team(&mut self, player_id: &str) -> String {
        if let Some(team) = self.teams.get(player_id) {
            return team.clone();
        }

        let teams = &self.config.gameplay.round.teams;
        let team = teams[php_crc32_index(player_id, teams.len())].clone();
        self.teams.insert(player_id.to_owned(), team.clone());
        team
    }

    fn spawn_items(&mut self) {
        let count = self.config.gameplay.items.spawn_count;
        let kinds = [
            ItemKind::Heal,
            ItemKind::Heal,
            ItemKind::Speed,
            ItemKind::Speed,
        ];

        let width = self.world_width();
        let height = self.world_height();

        for index in 0..count {
            let id = format!("i-{index}");
            let kind = kinds[(index as usize) % kinds.len()];
            let x = width * ((index + 1) as f64 / (count + 1) as f64);
            let y = height * (((index % 3) as f64 + 0.5) / 3.0);
            self.items.push(ItemState::new(id, kind, x, y));
        }
    }

    fn min_players(&self) -> i64 {
        self.config.gameplay.round.min_players
    }

    fn ticks_for_ms(&self, milliseconds: i64) -> i64 {
        let ticks = (milliseconds as f64 / self.config.tick_duration_ms).ceil() as i64;
        ticks.max(1)
    }
}

/// The host half of a world, for handing to a script engine.
///
/// It borrows the world for one call and only exposes [`ScriptHost`], so a
/// script can read state and queue intents but cannot touch the simulation.
struct WorldView<'a, S: ScriptEngine> {
    world: &'a mut World<S>,
}

impl<S: ScriptEngine> ScriptHost for WorldView<'_, S> {
    fn tick(&self) -> i64 {
        self.world.tick()
    }
    fn tick_rate(&self) -> i64 {
        self.world.tick_rate()
    }
    fn world_size(&self) -> (f64, f64) {
        self.world.world_size()
    }
    fn players(&self) -> Vec<PlayerView> {
        ScriptHost::players(&*self.world)
    }
    fn player(&self, id: &str) -> Option<PlayerView> {
        ScriptHost::player(&*self.world, id)
    }
    fn find_player(&self, id: &str) -> Option<PlayerView> {
        ScriptHost::find_player(&*self.world, id)
    }

    fn find_marker(&self, id: i64) -> Option<&Marker> {
        ScriptHost::find_marker(&*self.world, id)
    }

    fn player_sees_player(&self, watcher_id: &str, watched_id: &str) -> bool {
        ScriptHost::player_sees_player(&*self.world, watcher_id, watched_id)
    }
    fn player_sees_marker(&self, player_id: &str, marker_id: i64) -> bool {
        ScriptHost::player_sees_marker(&*self.world, player_id, marker_id)
    }
    fn resolved_id(&self, id: &str) -> String {
        ScriptHost::resolved_id(&*self.world, id)
    }

    fn resolved_marker_id(&self, id: i64) -> i64 {
        ScriptHost::resolved_marker_id(&*self.world, id)
    }

    fn markers(&self) -> Vec<Marker> {
        ScriptHost::markers(&*self.world)
    }
    fn reserve_marker_id(&mut self) -> i64 {
        ScriptHost::reserve_marker_id(&mut *self.world)
    }
    fn log(&mut self, level: &str, text: &str) {
        ScriptHost::log(self.world, level, text)
    }

    fn log_clear(&mut self) {
        // Collects on the same effect set the world applies after the hook.
        ScriptHost::log_clear(self.world)
    }

    fn random_int(&mut self, min: i64, max: i64) -> i64 {
        ScriptHost::random_int(self.world, min, max)
    }
    fn marker_limit_reached(&self) -> bool {
        ScriptHost::marker_limit_reached(&*self.world)
    }
    fn queue_position(&mut self, id: &str, x: f64, y: f64) {
        ScriptHost::queue_position(self.world, id, x, y)
    }
    fn current_score(&self, id: &str) -> Option<i64> {
        ScriptHost::current_score(&*self.world, id)
    }
    fn queue_score(&mut self, id: &str, score: i64, delta: bool) -> Option<i64> {
        ScriptHost::queue_score(self.world, id, score, delta)
    }
    fn queue_hp(&mut self, id: &str, hp: i64) {
        ScriptHost::queue_hp(self.world, id, hp)
    }
    fn queue_marker(&mut self, id: i64, x: f64, y: f64, label: &str, kind: &str) {
        ScriptHost::queue_marker(self.world, id, x, y, label, kind)
    }
    fn queue_remove_marker(&mut self, id: i64) -> bool {
        ScriptHost::queue_remove_marker(self.world, id)
    }
    fn queue_clear_markers(&mut self) {
        ScriptHost::queue_clear_markers(self.world)
    }
    fn queue_finish(&mut self, winner: Option<&str>) {
        ScriptHost::queue_finish(self.world, winner)
    }
    fn finish_requested(&self) -> bool {
        ScriptHost::finish_requested(&*self.world)
    }
}

fn ticks_to_ms(ticks: i64, tick_duration_ms: f64) -> i64 {
    // PHP: (int) round($ticks * $tickDurationMs).
    (ticks as f64 * tick_duration_ms).round() as i64
}

/// PHP `is_finite($value) ? max($min, min($max, $value)) : $min`.
fn clamp(value: f64, min: f64, max: f64) -> f64 {
    if value.is_finite() {
        value.max(min).min(max)
    } else {
        min
    }
}

fn is_friendly(first: &PlayerState, second: &PlayerState) -> bool {
    if first.user_id.is_some() && first.user_id == second.user_id {
        return true;
    }

    first.team.is_some() && first.team == second.team
}

fn player_view(player: &PlayerState) -> PlayerView {
    PlayerView {
        id: player.id.clone(),
        name: player.name.clone(),
        x: player.x,
        y: player.y,
        hp: player.hp,
        max_hp: player.max_hp,
        score: player.score,
        team: player.team.clone(),
        is_bot: false,
    }
}

impl<S: ScriptEngine> ScriptHost for World<S> {
    fn tick(&self) -> i64 {
        self.tick
    }

    fn tick_rate(&self) -> i64 {
        self.config.tick_rate
    }

    fn world_size(&self) -> (f64, f64) {
        (self.world_width(), self.world_height())
    }

    fn players(&self) -> Vec<PlayerView> {
        self.players.iter().map(player_view).collect()
    }

    fn player(&self, id: &str) -> Option<PlayerView> {
        World::player(self, id).map(player_view)
    }

    fn find_player(&self, id: &str) -> Option<PlayerView> {
        if let Some(player) = World::player(self, id) {
            return Some(player_view(player));
        }

        if id != "p-local" && id != "p-me" {
            return None;
        }

        self.players.first().map(player_view)
    }

    fn find_marker(&self, id: i64) -> Option<&Marker> {
        World::markers(self).iter().find(|marker| marker.id == id)
    }

    fn player_sees_player(&self, watcher_id: &str, watched_id: &str) -> bool {
        let watcher = self.resolved_id(watcher_id);
        let watched = self.resolved_id(watched_id);
        World::player_sees_player(self, &watcher, &watched)
    }

    fn player_sees_marker(&self, player_id: &str, marker_id: i64) -> bool {
        let player = self.resolved_id(player_id);
        let marker = self.resolved_marker_id(marker_id);
        World::player_sees_marker(self, &player, marker)
    }

    fn resolved_id(&self, id: &str) -> String {
        ScriptHost::find_player(self, id).map_or_else(|| id.to_owned(), |player| player.id)
    }

    fn resolved_marker_id(&self, id: i64) -> i64 {
        ScriptHost::find_marker(self, id).map_or_else(|| id.to_owned(), |marker| marker.id)
    }

    fn markers(&self) -> Vec<Marker> {
        self.markers.clone()
    }

    fn reserve_marker_id(&mut self) -> i64 {
        World::reserve_marker_id(self)
    }

    fn log(&mut self, level: &str, text: &str) {
        World::lua_log(self, level, text);
    }

    fn log_clear(&mut self) {
        // Collected on the world, not on the engine's effect set: the runtime
        // hands out a fresh set after every hook, which would drop the flag.
        // `run_hook` emits the event once the hook returned, so lines logged
        // before the clear still reach the client first.
        self.log_clear_this_hook = true;
    }

    fn random_int(&mut self, min: i64, max: i64) -> i64 {
        let (mut low, mut high) = (min, max);
        if low > high {
            std::mem::swap(&mut low, &mut high);
        }

        if low == high {
            return low;
        }

        // Matches the reference resolution (~31 bit).
        let fraction = crate::sim::random::fraction();
        let span = (high - low) as f64;
        low + ((span + 1.0) * fraction).floor().min(span) as i64
    }

    fn marker_limit_reached(&self) -> bool {
        self.markers.len() + self.collected.markers.len() >= MAX_MARKERS
    }

    fn queue_position(&mut self, id: &str, x: f64, y: f64) {
        self.collected.positions.push(PositionEffect {
            id: id.to_owned(),
            x,
            y,
        });
    }

    fn current_score(&self, id: &str) -> Option<i64> {
        World::player(self, id).map(|player| player.score)
    }

    fn queue_score(&mut self, id: &str, score: i64, delta: bool) -> Option<i64> {
        // The reference implementation stored effect requests in PHP arrays
        // handed to the script by value; the first write after an HP request in
        // the same hook went to a stale copy and was lost. Scripts that
        // `set_hp` therefore score from the *next* hook on. Kept for parity.
        if self.hp_queued_this_hook {
            return None;
        }

        let current = if delta { self.current_score(id)? } else { 0 };
        let value = if delta {
            current.saturating_add(score)
        } else {
            score
        };

        // Merge with the newest request for the same player: absolute requests
        // win, deltas stack on them.
        if let Some(index) = self
            .collected
            .scores
            .iter()
            .rposition(|entry| entry.id == id)
        {
            let existing = self.collected.scores.remove(index);
            let merged = value + if existing.delta { existing.score } else { 0 };
            self.collected.scores.push(ScoreEffect {
                id: id.to_owned(),
                score: merged,
                delta: false,
            });
            return Some(merged);
        }

        self.collected.scores.push(ScoreEffect {
            id: id.to_owned(),
            score: value,
            delta: false,
        });
        Some(value)
    }

    fn queue_hp(&mut self, id: &str, hp: i64) {
        self.hp_queued_this_hook = true;
        self.collected.hp.push(HpEffect {
            id: id.to_owned(),
            hp,
        });
    }

    fn queue_marker(&mut self, id: i64, x: f64, y: f64, label: &str, kind: &str) {
        self.collected.markers.push(MarkerEffect {
            id,
            x,
            y,
            label: label.to_owned(),
            kind: kind.to_owned(),
        });
    }

    fn queue_remove_marker(&mut self, id: i64) -> bool {
        self.collected.removed_markers.push(id);
        true
    }

    fn queue_clear_markers(&mut self) {
        self.collected.clear_markers = true;
    }

    fn queue_finish(&mut self, winner: Option<&str>) {
        self.collected.winner = Some(
            winner
                .filter(|value| !value.is_empty())
                .unwrap_or("draw")
                .to_owned(),
        );
    }

    fn finish_requested(&self) -> bool {
        self.collected.winner.is_some()
    }
}

#[cfg(test)]
mod effects_tests {
    use super::*;
    use crate::config::GameplayConfig;
    use crate::lua::runtime::LuaScript;
    use crate::sim::script::{LuaEffects, PositionEffect};

    #[test]
    fn a_scripted_position_is_clamped_once_and_not_by_the_radius() {
        let config = SimulationConfig::from_parts(60, 20, 8, 30, GameplayConfig::default());
        let mut world: World<LuaScript> = World::new("m", config);
        world.add_player("p-1", "Alice", None);

        world.apply_lua_effects(&LuaEffects {
            positions: vec![PositionEffect {
                id: "p-1".into(),
                x: 99999.0,
                y: -5.0,
            }],
            ..LuaEffects::default()
        });

        assert_eq!(world.player("p-1").unwrap().x, world.world_width());
        assert_eq!(world.player("p-1").unwrap().y, 0.0);
    }
}
