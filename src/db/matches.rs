//! Matches, rounds, participants and highscores.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};
use sqlx::Row;

use super::Db;
use crate::net::service::{RoundPlayer, RoundResult};

#[derive(Debug, Clone, PartialEq)]
pub struct MatchRow {
    pub id: String,
    pub game_mode: String,
    pub status: String,
    pub round_number: i64,
    pub tick_rate: i64,
    pub started_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
}

impl Db {
    /// Creates the durable match row on first use (idempotent).
    pub async fn ensure_match(
        &self,
        match_id: &str,
        game_mode: &str,
        tick_rate: i64,
        max_players: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO matches (id, game_mode, status, tick_rate, max_players, started_at, created_at, updated_at)
             VALUES ($1, $2, 'open', $3, $4, now(), now(), now())
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(match_id)
        .bind(game_mode)
        .bind(tick_rate as i16)
        .bind(max_players as i16)
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Records a participant; a reconnect refreshes name and team.
    pub async fn record_participant(
        &self,
        match_id: &str,
        protocol_player_id: &str,
        display_name: &str,
        player_profile_id: Option<i64>,
        team: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO match_players
                (match_id, protocol_player_id, player_id, display_name, team, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, now(), now())
             ON CONFLICT (match_id, protocol_player_id) DO UPDATE
                SET player_id = EXCLUDED.player_id,
                    display_name = EXCLUDED.display_name,
                    team = EXCLUDED.team,
                    updated_at = now()",
        )
        .bind(match_id)
        .bind(protocol_player_id)
        .bind(player_profile_id)
        .bind(display_name)
        .bind(team)
        .execute(self.pool())
        .await?;

        Ok(())
    }

    pub async fn mark_running(&self, match_id: &str) -> Result<()> {
        sqlx::query("UPDATE matches SET status = 'running', updated_at = now() WHERE id = $1 AND status = 'open'")
            .bind(match_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn match_by_id(&self, match_id: &str) -> Result<Option<MatchRow>> {
        let row = sqlx::query(
            "SELECT id, game_mode, status, round_number, tick_rate, started_at, closed_at
             FROM matches WHERE id = $1",
        )
        .bind(match_id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|row| MatchRow {
            id: row.get("id"),
            game_mode: row.get("game_mode"),
            status: row.get("status"),
            round_number: i64::from(row.get::<i32, _>("round_number")),
            tick_rate: i64::from(row.get::<i16, _>("tick_rate")),
            started_at: row.get("started_at"),
            closed_at: row.get("closed_at"),
        }))
    }

    pub async fn match_participants(&self, match_id: &str) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT protocol_player_id, display_name, team, score, deaths, kills
             FROM match_players WHERE match_id = $1 ORDER BY id",
        )
        .bind(match_id)
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "player_id": row.get::<String, _>("protocol_player_id"),
                    "name": row.get::<String, _>("display_name"),
                    "team": row.get::<Option<String>, _>("team"),
                    "score": row.get::<i32, _>("score"),
                    "deaths": row.get::<i16, _>("deaths"),
                    "kills": row.get::<i16, _>("kills"),
                })
            })
            .collect())
    }

    /// All rounds of a match, oldest first.
    pub async fn match_rounds(&self, match_id: &str) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT round_number, winner, scores, duration_ms, created_at
             FROM match_rounds WHERE match_id = $1 ORDER BY round_number",
        )
        .bind(match_id)
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "round_number": row.get::<i32, _>("round_number"),
                    "winner": row.get::<Option<String>, _>("winner"),
                    "scores": row.get::<Value, _>("scores"),
                    "duration_ms": row.get::<i32, _>("duration_ms"),
                    "finished_at": row.get::<Option<DateTime<Utc>>, _>("created_at"),
                })
            })
            .collect())
    }

    /// Persists one finished round. Idempotent per (match, round): a replay of
    /// the same signed result writes nothing new.
    ///
    /// Returns `false` when the round was already recorded.
    pub async fn record_round(&self, result: &RoundResult) -> Result<bool> {
        let mut transaction = self.pool().begin().await?;

        let inserted = sqlx::query(
            "INSERT INTO match_rounds (match_id, round_number, winner, scores, duration_ms, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, now(), now())
             ON CONFLICT (match_id, round_number) DO NOTHING",
        )
        .bind(&result.match_id)
        .bind(result.round_number as i32)
        .bind(&result.winner)
        .bind(Value::Object(result.team_scores.clone()))
        .bind(result.duration_ms as i32)
        .execute(&mut *transaction)
        .await?;

        if inserted.rows_affected() == 0 {
            return Ok(false);
        }

        sqlx::query(
            "UPDATE matches
             SET round_number = GREATEST(round_number, $2), status = 'running', updated_at = now()
             WHERE id = $1",
        )
        .bind(&result.match_id)
        .bind(result.round_number as i32)
        .execute(&mut *transaction)
        .await?;

        let winner = result.winner.clone();

        for player in &result.players {
            sqlx::query(
                "UPDATE match_players SET score = $3, deaths = $4, is_winner = ($5 = $6), updated_at = now()
                 WHERE match_id = $1 AND protocol_player_id = $2",
            )
            .bind(&result.match_id)
            .bind(&player.player_id)
            .bind(player.score as i32)
            .bind(player.deaths as i16)
            .bind(&player.team)
            .bind(&winner)
            .execute(&mut *transaction)
            .await?;

            let Some(profile_id) = player.profile_id else {
                continue;
            };

            sqlx::query(
                "INSERT INTO highscores (player_id, game_mode, score, match_id, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, now(), now())",
            )
            .bind(profile_id)
            .bind(&result.mode_id)
            .bind(player.score as i32)
            .bind(&result.match_id)
            .execute(&mut *transaction)
            .await?;

            sqlx::query("UPDATE players SET total_score = total_score + $2, updated_at = now() WHERE id = $1")
                .bind(profile_id)
                .bind(player.score.max(0) as i32)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;
        Ok(true)
    }

    /// Best score per player for one mode.
    pub async fn highscores(&self, game_mode: &str, limit: i64) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT h.player_id, p.display_name, MAX(h.score) AS best_score, COUNT(*) AS entries
             FROM highscores h LEFT JOIN players p ON p.id = h.player_id
             WHERE h.game_mode = $1
             GROUP BY h.player_id, p.display_name
             ORDER BY best_score DESC
             LIMIT $2",
        )
        .bind(game_mode)
        .bind(limit.clamp(1, 100))
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                serde_json::json!({
                    "rank": index + 1,
                    "player_id": row.get::<i64, _>("player_id"),
                    "display_name": row.get::<Option<String>, _>("display_name"),
                    "score": row.get::<i32, _>("best_score"),
                    "entries": row.get::<i64, _>("entries"),
                })
            })
            .collect())
    }

    pub async fn lobbies(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT l.id, l.name, l.game_mode, l.max_players, l.created_at,
                    (SELECT COUNT(*) FROM lobby_members m WHERE m.lobby_id = l.id) AS members
             FROM lobbies l WHERE l.status = 'open' ORDER BY l.created_at DESC LIMIT 50",
        )
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                let members: i64 = row.get("members");
                let max_players: i16 = row.get("max_players");
                serde_json::json!({
                    "id": row.get::<String, _>("id"),
                    "name": row.get::<String, _>("name"),
                    "game_mode": row.get::<String, _>("game_mode"),
                    "max_players": max_players,
                    "members": members,
                    "is_full": members >= i64::from(max_players),
                    "created_at": row.get::<Option<DateTime<Utc>>, _>("created_at"),
                })
            })
            .collect())
    }

    pub async fn create_lobby(
        &self,
        name: &str,
        game_mode: &str,
        max_players: i64,
        host_profile_id: i64,
    ) -> Result<Value> {
        let id = ulid::Ulid::new().to_string().to_lowercase();
        let mut transaction = self.pool().begin().await?;

        sqlx::query(
            "INSERT INTO lobbies (id, name, game_mode, max_players, status, host_player_profile_id, created_at, updated_at)
             VALUES ($1, $2, $3, $4, 'open', $5, now(), now())",
        )
        .bind(&id)
        .bind(name)
        .bind(game_mode)
        .bind(max_players as i16)
        .bind(host_profile_id)
        .execute(&mut *transaction)
        .await?;

        sqlx::query(
            "INSERT INTO lobby_members (lobby_id, player_id, joined_at, created_at, updated_at)
             VALUES ($1, $2, now(), now(), now())",
        )
        .bind(&id)
        .bind(host_profile_id)
        .execute(&mut *transaction)
        .await?;

        transaction.commit().await?;

        Ok(serde_json::json!({
            "id": id,
            "name": name,
            "game_mode": game_mode,
            "max_players": max_players,
        }))
    }
}

/// Rounds grouped by team, as stored in `match_rounds.scores`.
pub fn team_scores(players: &[RoundPlayer]) -> Map<String, Value> {
    let mut scores = Map::new();

    for player in players {
        if let Some(team) = &player.team {
            let entry = scores.entry(team.clone()).or_insert_with(|| Value::from(0));
            *entry = Value::from(entry.as_i64().unwrap_or(0) + player.score);
        }
    }

    scores
}
