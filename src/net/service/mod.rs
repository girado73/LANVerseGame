//! The game loop: worlds, connections, handshake and snapshot fan-out.
//!
//! A frame travels through these stages:
//!
//! 1. the transport decodes the raw frame (stateless, validates everything);
//! 2. the service binds or routes it to a connection (handshake, input, ping);
//! 3. inputs reach the world as *intent* only;
//! 4. once per tick the service advances worlds, drains events and fans out
//!    snapshots.
//!
//! Everything here is synchronous. The binary drives it from a single task, so
//! the tick never waits for the database: results and world state are handed to
//! sinks (see [`sinks`]) that persist them in the background.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::Value;

use crate::config::SimulationConfig;
use crate::lua::runtime::LuaScript;
use crate::net::connection::{ConnectionState, Transport};
use crate::net::protocol::{self, ClientMessage, Hello, Input, Ping, Pull};
use crate::sim::events::SimulationEvent;
use crate::sim::round_phase::RoundPhase;
use crate::sim::script::ScriptEngine;
use crate::sim::snapshot::SnapshotFrame;
use crate::sim::world::World;

pub use self::identity::{AuthError, GameIdentity, GuestOnlyResolver, IdentityResolver};
pub use self::modes::{EmptyModeRepository, ModePackage, ModeRepository};
pub use self::persistence::{
    NoResults, NoStateStore, ResultPersistence, ResultSink, RoundPlayer, RoundResult,
    StatePersistence, WorldStateStore,
};

pub mod identity;
pub mod modes;
pub mod persistence;

/// One request the outside world has to perform for the game loop.
/// Decides which match a connecting player belongs to.
pub trait Matchmaker: Send + Sync {
    fn resolve(&self, requested: Option<&str>) -> String;
}

/// Anyone joining without a specific match id lands in this match; a requested
/// mode id becomes its own match.
#[derive(Debug, Clone)]
pub struct DefaultMatchmaker {
    prefix: String,
    fixed: Option<String>,
}

impl DefaultMatchmaker {
    pub fn new(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            fixed: None,
        }
    }

    pub fn fixed(match_id: &str) -> Self {
        Self {
            prefix: "lan".to_owned(),
            fixed: Some(match_id.to_owned()),
        }
    }

    /// `lan-YYYYMMDD` (the reference used the local date).
    pub fn default_match_id(&self) -> String {
        if let Some(fixed) = &self.fixed {
            return fixed.clone();
        }

        format!("{}-{}", self.prefix, chrono::Local::now().format("%Y%m%d"))
    }

    fn sanitize(&self, requested: &str) -> String {
        let trimmed = requested.trim();
        let safe = !trimmed.is_empty()
            && trimmed.len() <= 64
            && trimmed.chars().all(|character| {
                character.is_ascii_alphanumeric() || character == '_' || character == '-'
            });

        if safe {
            trimmed.to_owned()
        } else {
            self.default_match_id()
        }
    }
}

impl Matchmaker for DefaultMatchmaker {
    fn resolve(&self, requested: Option<&str>) -> String {
        match requested {
            Some(requested) => self.sanitize(requested),
            None => self.default_match_id(),
        }
    }
}

pub struct GameService {
    pub config: SimulationConfig,
    protocol_version: i64,
    idle_timeout_ms: i64,
    flush_interval_ms: i64,
    game_mode: String,

    worlds: HashMap<String, World<LuaScript>>,
    active_packages: HashMap<String, ModePackage>,
    connections: HashMap<String, ConnectionState>,
    /// Connection ids in join order per match.
    rosters: HashMap<String, Vec<String>>,
    /// Players already announced in this process (match => player ids).
    announced: HashMap<String, HashSet<String>>,
    last_round_signature: HashMap<String, String>,
    last_persisted_round: HashMap<String, i64>,
    last_persisted_tick: HashMap<String, i64>,
    package_cache: HashMap<String, Option<ModePackage>>,
    /// Inputs accepted per connection in the current tick (a client sending
    /// faster than the server ticks keeps only its newest input).
    input_watermark: HashMap<String, i64>,

    next_tick_at_ms: i64,
    tick_remainder_ms: f64,
    last_flush_at: i64,
    opened_at_ms: i64,

    identity: Arc<dyn IdentityResolver>,
    matchmaker: Arc<dyn Matchmaker>,
    modes: Arc<dyn ModeRepository>,
    router: Arc<dyn Transport>,
    results: Arc<dyn ResultSink>,
    state_store: Arc<dyn WorldStateStore>,
}

impl GameService {
    pub fn new(
        config: SimulationConfig,
        protocol_version: i64,
        idle_timeout_ms: i64,
        flush_interval_ms: i64,
        game_mode: &str,
    ) -> Self {
        Self {
            config,
            protocol_version,
            idle_timeout_ms,
            flush_interval_ms,
            game_mode: game_mode.to_owned(),
            worlds: HashMap::new(),
            active_packages: HashMap::new(),
            connections: HashMap::new(),
            rosters: HashMap::new(),
            announced: HashMap::new(),
            last_round_signature: HashMap::new(),
            last_persisted_round: HashMap::new(),
            last_persisted_tick: HashMap::new(),
            package_cache: HashMap::new(),
            input_watermark: HashMap::new(),
            next_tick_at_ms: 0,
            tick_remainder_ms: 0.0,
            last_flush_at: 0,
            opened_at_ms: now_ms(),
            identity: Arc::new(GuestOnlyResolver),
            matchmaker: Arc::new(DefaultMatchmaker::new("lan")),
            modes: Arc::new(EmptyModeRepository),
            router: Arc::new(crate::net::connection::QueueTransport::default()),
            results: Arc::new(NoResults),
            state_store: Arc::new(NoStateStore),
        }
    }

    pub fn with_identity(mut self, identity: Arc<dyn IdentityResolver>) -> Self {
        self.identity = identity;
        self
    }

    pub fn with_matchmaker(mut self, matchmaker: Arc<dyn Matchmaker>) -> Self {
        self.matchmaker = matchmaker;
        self
    }

    pub fn with_modes(mut self, modes: Arc<dyn ModeRepository>) -> Self {
        self.modes = modes;
        self
    }

    pub fn with_results(mut self, results: Arc<dyn ResultSink>) -> Self {
        self.results = results;
        self
    }

    pub fn with_state_store(mut self, state_store: Arc<dyn WorldStateStore>) -> Self {
        self.state_store = state_store;
        self
    }

    /// Installs the transport the loop sends through.
    pub fn with_router(mut self, router: Arc<dyn Transport>) -> Self {
        self.router = router;
        self
    }

    // -----------------------------------------------------------------
    // Introspection (health endpoint, admin page, tests)
    // -----------------------------------------------------------------

    pub fn world(&self, match_id: &str) -> Option<&World<LuaScript>> {
        self.worlds.get(match_id)
    }

    pub fn worlds(&self) -> &HashMap<String, World<LuaScript>> {
        &self.worlds
    }

    pub fn connection(&self, connection_id: &str) -> Option<&ConnectionState> {
        self.connections.get(connection_id)
    }

    pub fn connected_players(&self) -> usize {
        self.worlds.values().map(World::player_count).sum()
    }

    pub fn active_package(&self, match_id: &str) -> Option<&ModePackage> {
        self.active_packages.get(match_id)
    }

    pub fn roster(&self, match_id: &str) -> &[String] {
        self.rosters.get(match_id).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn opened_at_ms(&self) -> i64 {
        self.opened_at_ms
    }

    /// The store worlds are persisted to (also used on shutdown).
    pub fn state_store(&self) -> Arc<dyn WorldStateStore> {
        self.state_store.clone()
    }

    pub fn protocol_version(&self) -> i64 {
        self.protocol_version
    }

    // -----------------------------------------------------------------
    // Connections
    // -----------------------------------------------------------------

    pub fn handle_open(&mut self, connection_id: &str) {
        let now = now_ms();
        let session_id = format!("s-{}", crate::sim::random::alphanumeric(20));
        self.connections.insert(
            connection_id.to_owned(),
            ConnectionState::new(
                connection_id,
                now,
                &session_id,
                self.config.tick_rate,
                self.protocol_version,
            ),
        );

        // Compatibility first frame: old clients ignore it, new ones use it
        // as a connection marker.
        let socket_id = format!(
            "{}.{}",
            crate::sim::random::global_int(1, 9_999_999),
            crate::sim::random::global_int(1, 9_999_999)
        );
        let frame =
            protocol::connection_established(&socket_id, (self.idle_timeout_ms / 1_000).max(1));
        self.send(connection_id, frame);
    }

    pub fn handle_close(&mut self, connection_id: &str) {
        self.disconnect(connection_id, true);
    }

    /// The underlying socket is gone: drop the connection's outbox too.
    fn close_socket(&self, connection_id: &str) {
        self.router.close(connection_id);
    }

    pub fn handle_frame(&mut self, connection_id: &str, payload: &str) {
        let Some(connection) = self.connections.get_mut(connection_id) else {
            return;
        };

        if connection.closed {
            return;
        }

        connection.touch(now_ms());

        match protocol::decode(payload) {
            ClientMessage::Hello(hello) => self.handle_hello(connection_id, hello),
            ClientMessage::Input(input) => self.handle_input(connection_id, input),
            ClientMessage::Ping(ping) => self.handle_ping(connection_id, ping),
            ClientMessage::Pull(pull) => self.handle_pull(connection_id, pull),
            ClientMessage::Invalid(invalid) => self.handle_invalid(connection_id, invalid.message),
            ClientMessage::Unknown(_) => {}
        }
    }

    fn handle_hello(&mut self, connection_id: &str, hello: Hello) {
        let Some(connection) = self.connections.get(connection_id) else {
            return;
        };
        if connection.handshake_completed {
            return;
        }

        if hello.protocol_version != self.protocol_version {
            self.reject(
                connection_id,
                protocol::error(
                    "protocol_mismatch",
                    &format!(
                        "Protocol version {} is not supported, server speaks {}.",
                        hello.protocol_version, self.protocol_version
                    ),
                    true,
                ),
            );
            return;
        }

        let identity = match self
            .identity
            .resolve(hello.token.as_deref(), &hello.player_name)
        {
            Ok(identity) => identity,
            Err(error) => {
                self.reject(
                    connection_id,
                    protocol::error("auth_failed", &error.message, true),
                );
                return;
            }
        };

        let match_id = self.matchmaker.resolve(hello.match_id.as_deref());
        if !self.worlds.contains_key(&match_id) {
            self.create_world(&match_id, hello.match_id.as_deref());
        }

        let (player_id, was_reconnect) = self.player_id_for(&identity, &match_id);

        // The control plane owns the canonical display name.
        let display_name = identity.display_name();
        let user_id = identity.user_id;

        let Some(world) = self.worlds.get_mut(&match_id) else {
            self.reject(
                connection_id,
                protocol::error("match_not_found", "Match not found.", false),
            );
            return;
        };

        if !world.add_player(&player_id, &display_name, user_id) {
            self.reject(
                connection_id,
                protocol::error("match_full", "Match is full.", false),
            );
            return;
        }

        let team = world
            .player(&player_id)
            .and_then(|player| player.team.clone());
        let server_tick = world.tick;

        if let Some(connection) = self.connections.get_mut(connection_id) {
            connection.bind(&match_id, &player_id, identity.clone(), was_reconnect);
            connection.world_state_sent = true;
        }
        self.rosters
            .entry(match_id.clone())
            .or_default()
            .push(connection_id.to_owned());

        self.results
            .participant(&match_id, &player_id, &identity, team.as_deref());

        let package = self.active_packages.get(&match_id);
        let mode = package.map(|package| {
            (
                package.id.as_str(),
                package.name.as_str(),
                package.checksum.as_str(),
            )
        });
        let session_id = self
            .connections
            .get(connection_id)
            .map(|connection| connection.session_id.clone());

        let frame = protocol::welcome(
            self.protocol_version,
            &player_id,
            self.config.tick_rate,
            server_tick,
            session_id.as_deref(),
            Some(&match_id),
            mode,
        );
        self.send(connection_id, frame);
        self.send_round_state_to(connection_id, &match_id);

        // Everybody else learns about the newcomer; a reconnect does not
        // announce itself twice.
        let already_announced = self
            .announced
            .get(&match_id)
            .is_some_and(|players| players.contains(&player_id));

        if !was_reconnect && !already_announced {
            self.announced
                .entry(match_id.clone())
                .or_default()
                .insert(player_id.clone());

            let frame = protocol::event(&SimulationEvent::PlayerJoined {
                player_id,
                name: display_name,
            });
            self.broadcast_except(&match_id, frame, connection_id);
        }
    }

    fn handle_input(&mut self, connection_id: &str, input: Input) {
        // Make the tick current first: a pending tick must run before this
        // input so it lands in the next one (inputs never race the clock).
        let now = now_ms();
        if self.next_tick_at_ms != 0 && self.next_tick_at_ms <= now {
            self.tick(now);
        }

        let Some(connection) = self.connections.get(connection_id) else {
            return;
        };

        let (Some(match_id), Some(player_id)) =
            (connection.match_id.clone(), connection.player_id.clone())
        else {
            if let Some(connection) = self.connections.get_mut(connection_id) {
                connection.invalid_frames += 1;
            }
            return;
        };

        let Some(world) = self.worlds.get_mut(&match_id) else {
            return;
        };

        if !world.has_player(&player_id) {
            return;
        }

        // One input per connection and tick: a client that sends faster than
        // the server ticks must not replace its own intent before the tick ran
        // (the world applies requests at input time, like the reference).
        let watermark = self
            .input_watermark
            .get(connection_id)
            .copied()
            .unwrap_or(i64::MIN);
        if watermark >= world.tick {
            if let Some(connection) = self.connections.get_mut(connection_id) {
                connection.coalesced_inputs += 1;
            }
            return;
        }
        self.input_watermark
            .insert(connection_id.to_owned(), world.tick);

        let _ = input.client_tick;
        world.apply_input(
            &player_id,
            input.sequence,
            input.movement_x,
            input.movement_y,
            &input.actions,
        );

        if let Some(world) = self.worlds.get_mut(&match_id) {
            let events = world.drain_events();
            for event in &events {
                self.broadcast(&match_id, protocol::event(event));
            }
        }

        self.dispatch_script_diagnostics(&match_id);
    }

    fn handle_ping(&mut self, connection_id: &str, ping: Ping) {
        self.send(connection_id, protocol::pong(ping.client_time_ms, now_ms()));
    }

    fn handle_pull(&mut self, connection_id: &str, pull: Pull) {
        let Some(connection) = self.connections.get(connection_id) else {
            return;
        };

        if pull.what != "mode" || !connection.handshake_completed {
            return;
        }

        let match_id = connection.match_id.clone().unwrap_or_default();
        let package = self
            .active_packages
            .get(&match_id)
            .map(ModePackage::to_value);
        self.send(connection_id, protocol::mode_package(package.as_ref()));
    }

    fn handle_invalid(&mut self, connection_id: &str, message: String) {
        let Some(connection) = self.connections.get_mut(connection_id) else {
            return;
        };

        connection.invalid_frames += 1;
        let count = connection.invalid_frames;
        let completed = connection.handshake_completed;

        // One malformed frame happens on a lossy LAN; a stream is abuse.
        if count == 1 {
            self.send(
                connection_id,
                protocol::error("invalid_message", &message, false),
            );
            return;
        }

        if count <= 32 {
            return;
        }

        if completed {
            self.disconnect(connection_id, true);
            self.close_socket(connection_id);
            return;
        }

        self.reject(
            connection_id,
            protocol::error("invalid_message", "Too many invalid frames.", false),
        );
    }

    // -----------------------------------------------------------------
    // Tick loop
    // -----------------------------------------------------------------

    /// Advances every world whose tick budget is due. Returns ticks simulated.
    pub fn tick(&mut self, now: i64) -> i64 {
        if !self.worlds.values().any(|world| !world.is_empty()) {
            self.sweep_idle_connections(now);
            return 0;
        }

        // Start (or re-base after a clock jump) at "now": the loop must never
        // burn its catch-up budget on a timestamp far in the past.
        if self.next_tick_at_ms == 0 || self.next_tick_at_ms < now - 1_000 {
            self.next_tick_at_ms = now;
        }

        let max_catchup = self.config.max_catchup_ticks;
        let mut ticks = 0;

        while self.next_tick_at_ms <= now && ticks < max_catchup {
            self.advance_all();
            ticks += 1;

            self.tick_remainder_ms += self.config.tick_duration_ms;
            let whole = self.tick_remainder_ms.floor();
            self.tick_remainder_ms -= whole;
            self.next_tick_at_ms += whole as i64;
        }

        if self.next_tick_at_ms <= now {
            // Too far behind: drop the backlog instead of spiralling.
            self.next_tick_at_ms = now + self.config.tick_duration_ms.ceil() as i64;
            self.tick_remainder_ms = 0.0;
        }

        self.sweep_idle_connections(now);
        self.maybe_flush(now);

        ticks
    }

    pub fn time_until_next_tick(&self, now: i64) -> i64 {
        if self.next_tick_at_ms == 0 {
            0
        } else {
            (self.next_tick_at_ms - now).max(0)
        }
    }

    fn advance_all(&mut self) {
        // A new tick accepts a new input per connection.
        self.input_watermark.clear();

        let match_ids: Vec<String> = self.worlds.keys().cloned().collect();

        for match_id in match_ids {
            let Some(world) = self.worlds.get_mut(&match_id) else {
                continue;
            };

            let started = std::time::Instant::now();
            world.advance();

            let events = world.drain_events();
            let clears_log = events
                .iter()
                .any(|event| matches!(event, SimulationEvent::ModeLogClear));
            for event in &events {
                self.broadcast(&match_id, protocol::event(event));
            }

            self.dispatch_script_diagnostics(&match_id);

            // The mode console is cleared after the lines of this tick, so the
            // client sees them appear and then drops them.
            if clears_log {
                let package_id = self
                    .worlds
                    .get(&match_id)
                    .and_then(|world| world.script.as_ref())
                    .map_or_else(
                        || "mode".to_owned(),
                        |script| script.package_id().to_owned(),
                    );
                let frame = protocol::mode_log_clear(&package_id);
                self.broadcast(&match_id, frame);
            }

            if let Some(world) = self.worlds.get(&match_id)
                && world.tick % self.config.ticks_per_snapshot == 0
            {
                self.send_snapshot(&match_id);
            }

            self.announce_round_state_if_changed(&match_id);

            // One span per match tick: the acceptance gate (p99 < 5 ms in
            // release, see `tests/perf_test.rs`) is visible in production.
            if started.elapsed() > std::time::Duration::from_millis(5) {
                tracing::warn!(
                    match_id,
                    elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
                    players = self.worlds.get(&match_id).map_or(0, World::player_count),
                    "slow tick"
                );
            }
        }

        self.reap_empty_worlds();
    }

    /// Forwards the script console and reports a failed script once.
    fn dispatch_script_diagnostics(&mut self, match_id: &str) {
        let Some(world) = self.worlds.get_mut(match_id) else {
            return;
        };

        let package_id = world.script.as_ref().map_or_else(
            || "mode".to_owned(),
            |script| script.package_id().to_owned(),
        );
        let logs = world.drain_lua_logs();

        for entry in logs {
            let frame = if entry.level == "error" {
                tracing::warn!(
                    match_id,
                    mode = %package_id,
                    error = %entry.text,
                    "mode script failed, falling back to built-in rules"
                );
                protocol::mode_error(&package_id, &entry.text)
            } else {
                protocol::mode_log(&package_id, &entry.level, &entry.text)
            };

            self.broadcast(match_id, frame);
        }
    }

    fn send_snapshot(&mut self, match_id: &str) {
        let Some(world) = self.worlds.get(match_id) else {
            return;
        };

        let connections: Vec<(String, String)> = self
            .rosters
            .get(match_id)
            .into_iter()
            .flatten()
            .filter_map(|connection_id| {
                let connection = self.connections.get(connection_id)?;
                Some((
                    connection_id.clone(),
                    connection.player_id.clone().unwrap_or_default(),
                ))
            })
            .collect();

        if connections.is_empty() {
            return;
        }

        // Built and serialized once per tick; only the acknowledged sequence is
        // per connection, so its digits are swapped into the shared bytes.
        let round = crate::sim::snapshot::round_block(world);
        let players = crate::sim::snapshot::players(world);
        let markers = if world.is_script_driven() {
            crate::sim::snapshot::markers(world.markers())
        } else {
            Vec::new()
        };
        let tick = world.tick;

        let shared = SnapshotFrame {
            tick,
            acked_input_sequence: 0,
            round,
            players,
            markers,
        }
        .to_json();

        // The shared JSON is whitespace-free and starts with
        // `{"type":"snapshot","tick":<tick>,"acked_input_sequence":<seq>,…`;
        // replacing the eight characters of the leading `0` keeps every other
        // byte shared (`Arc<str>`), so a 100-player frame is serialized once.
        let marker = r#""acked_input_sequence":"#;
        let sequence_start = shared
            .find(marker)
            .expect("snapshot frames always carry the ack")
            + marker.len();
        let (head, tail) = shared.split_at(sequence_start);

        for (connection_id, player_id) in connections {
            let acked = world
                .player(&player_id)
                .map_or(0, |player| player.acked_input_sequence);

            let json = if acked == 0 {
                // The placeholder is one digit; a zero needs no rebuild.
                shared.clone()
            } else {
                format!("{head}{acked}{}", &tail[1..])
            };

            self.router.send_json(&connection_id, &json);
        }
    }

    fn announce_round_state_if_changed(&mut self, match_id: &str) {
        let Some(world) = self.worlds.get(match_id) else {
            return;
        };

        let signature = format!("{}|{}", world.phase.as_str(), world.round_number);

        if self.last_round_signature.get(match_id) == Some(&signature) {
            return;
        }

        self.last_round_signature
            .insert(match_id.to_owned(), signature);

        let connections = self.rosters.get(match_id).cloned().unwrap_or_default();
        for connection_id in connections {
            self.send_round_state_to(&connection_id, match_id);
        }

        self.on_round_transition(match_id);
    }

    fn on_round_transition(&mut self, match_id: &str) {
        let Some(world) = self.worlds.get(match_id) else {
            return;
        };

        if world.phase == RoundPhase::Running {
            self.results.match_started(match_id);
            return;
        }

        if world.phase != RoundPhase::Finished || world.winner.is_none() {
            return;
        }

        let round_number = world.round_number;
        if self
            .last_persisted_round
            .get(match_id)
            .copied()
            .unwrap_or(0)
            >= round_number
        {
            return;
        }
        self.last_persisted_round
            .insert(match_id.to_owned(), round_number);

        let players = world
            .players()
            .iter()
            .map(|player| RoundPlayer {
                player_id: player.id.clone(),
                user_id: player.user_id,
                profile_id: profile_id_of(player),
                display_name: player.name.clone(),
                team: player.team.clone(),
                score: player.score,
                deaths: player.deaths,
            })
            .collect();

        let result = RoundResult {
            match_id: match_id.to_owned(),
            mode_id: self
                .active_packages
                .get(match_id)
                .map_or_else(|| self.game_mode.clone(), |package| package.id.clone()),
            round_number,
            round_id: world.round_id().unwrap_or("r-0").to_owned(),
            winner: world.winner.clone(),
            team_scores: world.team_scores_json(),
            players,
            duration_ms: self.config.gameplay.round.duration_ms,
            finished_at_tick: world.tick,
        };

        self.results.record(&result);
    }

    fn send_round_state_to(&mut self, connection_id: &str, match_id: &str) {
        let Some(world) = self.worlds.get(match_id) else {
            return;
        };

        let round = world.round_id();
        let frame = match world.phase {
            RoundPhase::Connecting => protocol::round_state("connecting", None, 0, 0, None),
            RoundPhase::Lobby => protocol::round_state("lobby", round, world.round_number, 0, None),
            RoundPhase::Countdown => protocol::round_state(
                "countdown",
                round,
                world.round_number,
                world.countdown_ms(),
                None,
            ),
            RoundPhase::Running => {
                protocol::round_state("running", round, world.round_number, 0, None)
            }
            RoundPhase::Finished => protocol::round_state(
                "finished",
                round,
                world.round_number,
                0,
                world.winner.as_deref(),
            ),
        };

        self.send(connection_id, frame);
    }

    // -----------------------------------------------------------------
    // Worlds and connections
    // -----------------------------------------------------------------

    /// Creates the world for a match. A requested match id naming a published
    /// mode makes the match script-driven; anything else falls back to the
    /// built-in Deathmatch so a join never fails because of an upload.
    fn create_world(&mut self, match_id: &str, requested_match_id: Option<&str>) {
        let package = match requested_match_id {
            Some(requested) => {
                if let Some(cached) = self.package_cache.get(requested) {
                    cached.clone()
                } else {
                    let found = self
                        .modes
                        .find(requested)
                        .filter(|package| !package.script.is_empty());
                    self.package_cache
                        .insert(requested.to_owned(), found.clone());
                    found
                }
            }
            None => None,
        };

        let mut world: World<LuaScript> = World::new(match_id, self.config.clone());

        if let Some(candidate) = package {
            let script = LuaScript::new(&candidate.id, &candidate.script);
            let probe = world.attach_script(script, true);

            // `on_start` ran once as a probe; a broken script never claims a
            // match, so the world falls back to Deathmatch from tick 0.
            if probe.ok && world.is_script_driven() {
                world.with_round_timings(
                    manifest_int(&candidate.manifest, "/round/countdown_ms", 3_000)
                        .clamp(0, 30_000),
                    manifest_int(&candidate.manifest, "/round/duration_ms", 120_000).max(1_000),
                );
                tracing::info!(
                    match_id,
                    mode = %candidate.id,
                    script_bytes = candidate.script.len(),
                    "lua mode match created"
                );
                self.active_packages.insert(match_id.to_owned(), candidate);
            } else {
                tracing::warn!(
                    match_id,
                    mode = %candidate.id,
                    error = ?probe.failure,
                    "mode script failed its probe, falling back to Deathmatch"
                );
                world = World::new(match_id, self.config.clone());
            }
        } else {
            tracing::debug!(match_id, "deathmatch match created");
        }

        self.worlds.insert(match_id.to_owned(), world);
    }

    /// The same authenticated profile always maps to the same player id so a
    /// reconnect replaces its ghost. Guests get a random id.
    fn player_id_for(&mut self, identity: &GameIdentity, match_id: &str) -> (String, bool) {
        if let Some(profile_id) = identity.player_profile_id {
            let player_id = format!("p-{profile_id}");
            let is_reconnect = self
                .worlds
                .get(match_id)
                .is_some_and(|world| world.has_player(&player_id));

            if is_reconnect && let Some(world) = self.worlds.get_mut(match_id) {
                world.remove_player(&player_id);
            }

            return (player_id, is_reconnect);
        }

        loop {
            let player_id = format!("p-{}", crate::sim::random::alphanumeric(10));
            let taken = self
                .worlds
                .get(match_id)
                .is_some_and(|world| world.has_player(&player_id));

            if !taken {
                return (player_id, false);
            }
        }
    }

    fn disconnect(&mut self, connection_id: &str, notify: bool) {
        let Some(connection) = self.connections.get_mut(connection_id) else {
            return;
        };

        if connection.closed {
            return;
        }

        connection.closed = true;
        let match_id = connection.match_id.clone();
        let player_id = connection.player_id.clone();

        if let Some(match_id) = match_id {
            if let Some(roster) = self.rosters.get_mut(&match_id) {
                roster.retain(|id| id != connection_id);
            }

            // A reconnect can briefly overlap its old socket; the player only
            // leaves when the last session of that identity is gone.
            let still_connected = self
                .rosters
                .get(&match_id)
                .map(|roster| {
                    roster.iter().any(|other| {
                        self.connections
                            .get(other)
                            .is_some_and(|other| !other.closed && other.player_id == player_id)
                    })
                })
                .unwrap_or(false);

            if let (Some(world), Some(player_id)) =
                (self.worlds.get_mut(&match_id), player_id.clone())
                && !still_connected
            {
                let was_present = world.has_player(&player_id);
                world.remove_player(&player_id);

                if notify && was_present {
                    if let Some(announced) = self.announced.get_mut(&match_id) {
                        announced.remove(&player_id);
                    }

                    self.broadcast(
                        &match_id,
                        protocol::event(&SimulationEvent::PlayerLeft { player_id }),
                    );
                }
            }
        }

        self.connections.remove(connection_id);
    }

    fn reject(&mut self, connection_id: &str, frame: Value) {
        self.send(connection_id, frame);
        self.disconnect(connection_id, false);
        self.close_socket(connection_id);
    }

    fn sweep_idle_connections(&mut self, now: i64) {
        let idle: Vec<String> = self
            .connections
            .iter()
            .filter(|(_, connection)| connection.is_idle(now, self.idle_timeout_ms))
            .map(|(id, _)| id.clone())
            .collect();

        for connection_id in idle {
            tracing::debug!(%connection_id, "dropping idle connection");
            self.disconnect(&connection_id, true);
            self.close_socket(&connection_id);
        }
    }

    fn reap_empty_worlds(&mut self) {
        let empty: Vec<String> = self
            .worlds
            .iter()
            .filter(|(match_id, world)| {
                world.is_empty() && self.rosters.get(*match_id).is_none_or(Vec::is_empty)
            })
            .map(|(match_id, _)| match_id.clone())
            .collect();

        for match_id in empty {
            self.worlds.remove(&match_id);
            self.last_round_signature.remove(&match_id);
            self.last_persisted_tick.remove(&match_id);
        }
    }

    fn maybe_flush(&mut self, now: i64) {
        if now - self.last_flush_at < self.flush_interval_ms {
            return;
        }
        self.last_flush_at = now;

        let dirty: Vec<(String, Value)> = self
            .worlds
            .iter()
            .filter(|(match_id, world)| {
                self.last_persisted_tick.get(*match_id) != Some(&world.tick)
            })
            .map(|(match_id, world)| (match_id.clone(), world.snapshot_state()))
            .collect();

        for (match_id, _) in &dirty {
            let tick = self
                .worlds
                .get(match_id)
                .map(|world| world.tick)
                .unwrap_or(0);
            self.last_persisted_tick.insert(match_id.clone(), tick);
        }

        if !dirty.is_empty() {
            self.state_store.save_states(&dirty);
        }
    }

    /// Restores a persisted world (server restart in the middle of a round).
    pub fn restore_world(&mut self, match_id: &str, state: &Value) {
        let world = World::<LuaScript>::restore(match_id, self.config.clone(), state);
        self.worlds.insert(match_id.to_owned(), world);
    }

    // -----------------------------------------------------------------
    // Sending
    // -----------------------------------------------------------------

    fn send(&self, connection_id: &str, frame: Value) {
        self.router.send(connection_id, &frame);
    }

    fn broadcast(&self, match_id: &str, frame: Value) {
        for connection_id in self.rosters.get(match_id).into_iter().flatten() {
            self.send(connection_id, frame.clone());
        }
    }

    fn broadcast_except(&self, match_id: &str, frame: Value, except: &str) {
        for connection_id in self.rosters.get(match_id).into_iter().flatten() {
            if connection_id != except {
                self.send(connection_id, frame.clone());
            }
        }
    }

    /// The Lua package a match is playing (for `pull` and diagnostics).
    pub fn mode_package_for_match(&self, match_id: &str) -> Option<Value> {
        self.active_packages
            .get(match_id)
            .map(ModePackage::to_value)
    }
}

fn profile_id_of(player: &crate::sim::player::PlayerState) -> Option<i64> {
    player.user_id?;

    player
        .id
        .strip_prefix("p-")
        .and_then(|id| id.parse::<i64>().ok())
}

fn manifest_int(manifest: &Value, pointer: &str, default: i64) -> i64 {
    manifest
        .pointer(pointer)
        .and_then(Value::as_i64)
        .unwrap_or(default)
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_match_is_dated() {
        let matchmaker = DefaultMatchmaker::new("lan");
        let id = matchmaker.resolve(None);
        assert!(id.starts_with("lan-"), "{id}");
        assert_eq!(id.len(), "lan-YYYYMMDD".len());
    }

    #[test]
    fn mode_ids_become_their_own_match() {
        let matchmaker = DefaultMatchmaker::new("lan");
        assert_eq!(matchmaker.resolve(Some("coin_rush")), "coin_rush");
        assert_eq!(matchmaker.resolve(Some("m-42")), "m-42");
    }

    #[test]
    fn unsafe_match_ids_fall_back_to_the_default() {
        let matchmaker = DefaultMatchmaker::new("lan");
        assert_eq!(matchmaker.resolve(Some("../etc")), matchmaker.resolve(None));
        assert_eq!(matchmaker.resolve(Some("")), matchmaker.resolve(None));
        assert_eq!(
            matchmaker.resolve(Some(&"a".repeat(65))),
            matchmaker.resolve(None)
        );
    }
}
