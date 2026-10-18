//! What the loop hands to the outside world (database, Redis, control plane).
//!
//! The simulation never waits for storage: the process drains these records
//! after each tick.

use serde_json::Value;

use crate::net::service::GameIdentity;

/// Outcome of a finished round.
#[derive(Debug, Clone, PartialEq)]
pub struct RoundResult {
    pub match_id: String,
    pub mode_id: String,
    pub round_number: i64,
    pub round_id: String,
    pub winner: Option<String>,
    pub team_scores: serde_json::Map<String, Value>,
    pub players: Vec<RoundPlayer>,
    pub duration_ms: i64,
    pub finished_at_tick: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RoundPlayer {
    pub player_id: String,
    pub user_id: Option<i64>,
    pub profile_id: Option<i64>,
    pub display_name: String,
    pub team: Option<String>,
    pub score: i64,
    pub deaths: i64,
}

pub trait ResultSink: Send + Sync {
    fn record(&self, result: &RoundResult);
    fn match_started(&self, match_id: &str);
    fn participant(
        &self,
        match_id: &str,
        player_id: &str,
        identity: &GameIdentity,
        team: Option<&str>,
    );
}

/// Discards everything (tests, simulation-only deployments).
pub struct NoResults;

impl ResultSink for NoResults {
    fn record(&self, _result: &RoundResult) {}
    fn match_started(&self, _match_id: &str) {}
    fn participant(
        &self,
        _match_id: &str,
        _player_id: &str,
        _identity: &GameIdentity,
        _team: Option<&str>,
    ) {
    }
}

pub trait WorldStateStore: Send + Sync {
    fn save_states(&self, states: &[(String, Value)]);
}

pub struct NoStateStore;

impl WorldStateStore for NoStateStore {
    fn save_states(&self, _states: &[(String, Value)]) {}
}

/// Many sinks / stores behind one trait object.
pub struct ResultPersistence(pub Vec<std::sync::Arc<dyn ResultSink>>);

impl ResultSink for ResultPersistence {
    fn record(&self, result: &RoundResult) {
        for sink in &self.0 {
            sink.record(result);
        }
    }

    fn match_started(&self, match_id: &str) {
        for sink in &self.0 {
            sink.match_started(match_id);
        }
    }

    fn participant(
        &self,
        match_id: &str,
        player_id: &str,
        identity: &GameIdentity,
        team: Option<&str>,
    ) {
        for sink in &self.0 {
            sink.participant(match_id, player_id, identity, team);
        }
    }
}

pub struct StatePersistence(pub Vec<std::sync::Arc<dyn WorldStateStore>>);

impl WorldStateStore for StatePersistence {
    fn save_states(&self, states: &[(String, Value)]) {
        for store in &self.0 {
            store.save_states(states);
        }
    }
}
