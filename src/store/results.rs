//! Round results: HMAC-signed canonical bodies, idempotent persistence.
//!
//! The game loop never writes to the database directly. It builds a result,
//! signs the canonical JSON with `LANBASH_RESULT_SECRET` and hands it to a
//! [`ResultRecorder`]. In a single process the recorder receives it directly; a
//! split deployment posts the same signed body to `POST /internal/rounds`.
//! Both paths verify the signature before writing, so a forged or truncated
//! result never reaches `match_rounds`/`highscores`.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db::Db;
use crate::net::service::{GameIdentity, ResultSink, RoundPlayer, RoundResult};

/// Header carrying the signature.
pub const SIGNATURE_HEADER: &str = "x-lanverse-signature";
const USER_MORPH: &str = "App\\Models\\User";

/// The signed payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedResult {
    pub match_id: String,
    pub mode_id: String,
    pub round_number: i64,
    pub round_id: String,
    pub winner: Option<String>,
    pub team_scores: serde_json::Map<String, Value>,
    pub players: Vec<SignedPlayer>,
    pub duration_ms: i64,
    pub finished_at_tick: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedPlayer {
    pub player_id: String,
    pub user_id: Option<i64>,
    pub profile_id: Option<i64>,
    pub display_name: String,
    pub team: Option<String>,
    pub score: i64,
    pub deaths: i64,
}

impl From<&RoundResult> for SignedResult {
    fn from(result: &RoundResult) -> Self {
        Self {
            match_id: result.match_id.clone(),
            mode_id: result.mode_id.clone(),
            round_number: result.round_number,
            round_id: result.round_id.clone(),
            winner: result.winner.clone(),
            team_scores: result.team_scores.clone(),
            players: result
                .players
                .iter()
                .map(|player| SignedPlayer {
                    player_id: player.player_id.clone(),
                    user_id: player.user_id,
                    profile_id: player.profile_id,
                    display_name: player.display_name.clone(),
                    team: player.team.clone(),
                    score: player.score,
                    deaths: player.deaths,
                })
                .collect(),
            duration_ms: result.duration_ms,
            finished_at_tick: result.finished_at_tick,
        }
    }
}

impl SignedResult {
    pub fn into_round_result(self) -> RoundResult {
        RoundResult {
            match_id: self.match_id,
            mode_id: self.mode_id,
            round_number: self.round_number,
            round_id: self.round_id,
            winner: self.winner,
            team_scores: self.team_scores,
            players: self
                .players
                .into_iter()
                .map(|player| RoundPlayer {
                    player_id: player.player_id,
                    user_id: player.user_id,
                    profile_id: player.profile_id,
                    display_name: player.display_name,
                    team: player.team,
                    score: player.score,
                    deaths: player.deaths,
                })
                .collect(),
            duration_ms: self.duration_ms,
            finished_at_tick: self.finished_at_tick,
        }
    }
}

/// Signs a result. See [`canonical_json`] for the exact bytes covered.
pub fn sign(secret: &str, result: &SignedResult) -> Result<String> {
    use hmac::{Hmac, KeyInit, Mac};

    use sha2::Sha256;

    let body = canonical_json(result)?;
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes())
        .map_err(|error| anyhow::anyhow!("bad HMAC secret: {error}"))?;
    mac.update(body.as_bytes());

    Ok(hex(&mac.finalize().into_bytes()))
}

/// Verifies a signature in constant time.
pub fn verify(secret: &str, result: &SignedResult, signature: &str) -> bool {
    use subtle::ConstantTimeEq;

    match sign(secret, result) {
        Ok(expected) => {
            let expected = expected.as_bytes();
            let given = signature.trim().as_bytes();
            expected.len() == given.len() && expected.ct_eq(given).into()
        }
        Err(_) => false,
    }
}

/// Deterministic JSON: object keys sorted, no whitespace. Both sides of the
/// signature compute exactly these bytes.
pub fn canonical_json(result: &SignedResult) -> Result<String> {
    let value = serde_json::to_value(result)?;
    Ok(canonical(&value).to_string())
}

fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = serde_json::Map::new();

            for key in keys {
                sorted.insert(key.clone(), canonical(&map[key]));
            }

            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Persists verified results and keeps the match bookkeeping up to date.
pub struct ResultRecorder {
    db: Db,
    secret: String,
    /// Mode and limits stored on a match row when it is created.
    game_mode: String,
    tick_rate: i64,
    max_players: i64,
}

impl ResultRecorder {
    pub fn new(db: Db, secret: impl Into<String>) -> Self {
        Self {
            db,
            secret: secret.into(),
            game_mode: "deathmatch".to_owned(),
            tick_rate: 60,
            max_players: 512,
        }
    }

    /// Defaults written to `matches` when a join creates the row.
    pub fn with_match_defaults(
        mut self,
        game_mode: &str,
        tick_rate: i64,
        max_players: i64,
    ) -> Self {
        self.game_mode = game_mode.to_owned();
        self.tick_rate = tick_rate;
        self.max_players = max_players;
        self
    }

    pub fn signed(&self, result: &RoundResult) -> Result<(Value, String)> {
        let signed = SignedResult::from(result);
        let signature = sign(&self.secret, &signed)?;
        Ok((serde_json::to_value(&signed)?, signature))
    }

    /// Verifies and writes. Returns `Ok(false)` for a duplicate round.
    pub fn accept(&self, body: &Value, signature: &str) -> Result<bool> {
        let signed: SignedResult = serde_json::from_value(body.clone())
            .map_err(|error| anyhow::anyhow!("malformed result: {error}"))?;

        if !verify(&self.secret, &signed, signature) {
            bail!("invalid signature");
        }

        let result = signed.into_round_result();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        runtime.block_on(async { self.db.record_round(&result).await })
    }

    pub fn record_blocking(&self, result: &RoundResult) -> Result<bool> {
        let (body, signature) = self.signed(result)?;
        self.accept(&body, &signature)
    }

    /// Blocking database helpers for the synchronous result sink.
    fn run<F, T>(&self, future: F) -> Option<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(future).ok()
    }
}

impl ResultSink for ResultRecorder {
    fn record(&self, result: &RoundResult) {
        match self.record_blocking(result) {
            Ok(true) => tracing::info!(
                match_id = %result.match_id,
                round = result.round_number,
                "round result recorded"
            ),
            Ok(false) => tracing::debug!(round = result.round_number, "round already recorded"),
            Err(error) => tracing::error!(%error, "recording the round result failed"),
        }
    }

    fn match_started(&self, match_id: &str) {
        let _ = self.run(self.db.mark_running(match_id));
    }

    fn participant(
        &self,
        match_id: &str,
        player_id: &str,
        identity: &GameIdentity,
        team: Option<&str>,
    ) {
        let _ = self.run(self.db.ensure_match(
            match_id,
            &self.game_mode,
            self.tick_rate,
            self.max_players,
        ));
        let _ = self.run(self.db.record_participant(
            match_id,
            player_id,
            &identity.display_name(),
            identity.player_profile_id,
            team,
        ));
    }
}

/// Creates the match row lazily (the loop knows the mode, the tick rate does
/// not matter for the row beyond the snapshot rates).
pub fn ensure_match(db: &Db, match_id: &str, game_mode: &str, tick_rate: i64, max_players: i64) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();

    if let Ok(runtime) = runtime {
        let _ = runtime.block_on(db.ensure_match(match_id, game_mode, tick_rate, max_players));
    }
}

/// True when the given morph type belongs to a user (migration check).
pub fn is_user_morph(morph: &str) -> bool {
    morph == USER_MORPH
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result() -> RoundResult {
        RoundResult {
            match_id: "m-1".into(),
            mode_id: "deathmatch".into(),
            round_number: 2,
            round_id: "r-2".into(),
            winner: Some("red".into()),
            team_scores: json!({"red": 3, "blue": 1}).as_object().unwrap().clone(),
            players: vec![RoundPlayer {
                player_id: "p-1".into(),
                user_id: Some(1),
                profile_id: Some(1),
                display_name: "Alice".into(),
                team: Some("red".into()),
                score: 3,
                deaths: 0,
            }],
            duration_ms: 60_000,
            finished_at_tick: 3600,
        }
    }

    #[test]
    fn signatures_round_trip_and_reject_tampering() {
        let signed = SignedResult::from(&result());
        let signature = sign("secret", &signed).unwrap();

        assert!(verify("secret", &signed, &signature));
        assert!(!verify("other-secret", &signed, &signature));
        assert!(!verify("secret", &signed, &format!("{signature}0")));

        let mut tampered = signed.clone();
        tampered.team_scores.insert("red".into(), Value::from(999));
        assert!(!verify("secret", &tampered, &signature));
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let signed = SignedResult::from(&result());
        let json = canonical_json(&signed).unwrap();

        assert!(json.starts_with(r#"{"duration_ms":60000,"finished_at_tick":3600,"match_id":"m-1","mode_id":"deathmatch","players":[{"death"#));
        assert!(!json.contains(' '));
    }

    #[test]
    fn signature_is_stable_for_equal_values() {
        let first = sign("k", &SignedResult::from(&result())).unwrap();
        let second = sign("k", &SignedResult::from(&result())).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn the_user_morph_matches_the_migration() {
        assert!(is_user_morph("App\\Models\\User"));
    }
}
