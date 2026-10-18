//! Mutable per-player state owned exclusively by the simulation.
//!
//! Nothing in here is ever taken from the client except the input fields, and
//! those are validated and normalized by the protocol decoder before they
//! reach this type.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    /// Protocol player id ("p-…").
    pub id: String,
    pub name: String,
    /// "red"/"blue", or `None` in free-for-all.
    pub team: Option<String>,
    /// Authenticated control plane user, if any.
    pub user_id: Option<i64>,
    pub joined_at_tick: i64,

    pub x: f64,
    pub y: f64,
    pub max_hp: i64,
    pub hp: i64,
    pub score: i64,
    pub deaths: i64,
    pub is_alive: bool,

    pub velocity_x: f64,
    pub velocity_y: f64,
    pub facing_x: f64,
    pub facing_y: f64,

    pub respawn_at_tick: i64,
    pub dash_until_tick: i64,
    pub dash_ready_at_tick: i64,
    pub jump_until_tick: i64,
    pub jump_ready_at_tick: i64,
    pub attack_ready_at_tick: i64,

    /// Movement intent for the current tick, already normalized.
    pub input_x: f64,
    pub input_y: f64,
    /// Whether the client claimed to be moving this tick (dashes need this).
    pub has_movement_intent: bool,
    /// Edge-triggered attack request, consumed by the next movement step.
    pub wants_attack: bool,
    /// Highest input sequence processed; echoed in snapshots for prediction.
    pub acked_input_sequence: i64,
}

impl PlayerState {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        x: f64,
        y: f64,
        max_hp: i64,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            team: None,
            user_id: None,
            joined_at_tick: 0,
            x,
            y,
            max_hp,
            hp: max_hp,
            score: 0,
            deaths: 0,
            is_alive: true,
            velocity_x: 0.0,
            velocity_y: 0.0,
            facing_x: 1.0,
            facing_y: 0.0,
            respawn_at_tick: 0,
            dash_until_tick: 0,
            dash_ready_at_tick: 0,
            jump_until_tick: 0,
            jump_ready_at_tick: 0,
            attack_ready_at_tick: 0,
            input_x: 0.0,
            input_y: 0.0,
            has_movement_intent: false,
            wants_attack: false,
            acked_input_sequence: 0,
        }
    }

    pub fn is_dashing(&self, tick: i64) -> bool {
        tick < self.dash_until_tick
    }

    pub fn is_jumping(&self, tick: i64) -> bool {
        tick < self.jump_until_tick
    }

    /// Clears per-tick movement intent once it has been consumed.
    pub fn reset_movement_intent(&mut self) {
        self.input_x = 0.0;
        self.input_y = 0.0;
        self.has_movement_intent = false;
        self.wants_attack = false;
    }

    /// Restores a player to a fresh state for a new round.
    pub fn reset_for_round(&mut self) {
        self.hp = self.max_hp;
        self.is_alive = true;
        self.score = 0;
        self.deaths = 0;
        self.velocity_x = 0.0;
        self.velocity_y = 0.0;
        self.dash_until_tick = 0;
        self.dash_ready_at_tick = 0;
        self.jump_until_tick = 0;
        self.jump_ready_at_tick = 0;
        self.attack_ready_at_tick = 0;
        self.respawn_at_tick = 0;
        self.reset_movement_intent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_for_round_clears_everything_but_identity() {
        let mut player = PlayerState::new("p-1", "Alice", 10.0, 20.0, 100);
        player.hp = 3;
        player.score = 7;
        player.deaths = 2;
        player.dash_until_tick = 50;
        player.input_x = 1.0;
        player.wants_attack = true;

        player.reset_for_round();

        assert_eq!(player.hp, 100);
        assert_eq!(player.score, 0);
        assert_eq!(player.deaths, 0);
        assert!(player.is_alive);
        assert_eq!(player.dash_until_tick, 0);
        assert_eq!(player.input_x, 0.0);
        assert!(!player.wants_attack);
        assert_eq!(player.id, "p-1");
        assert_eq!((player.x, player.y), (10.0, 20.0));
    }

    #[test]
    fn dash_and_jump_windows_are_half_open() {
        let mut player = PlayerState::new("p-1", "Alice", 0.0, 0.0, 100);
        player.dash_until_tick = 10;
        player.jump_until_tick = 5;
        assert!(player.is_dashing(9));
        assert!(!player.is_dashing(10));
        assert!(player.is_jumping(4));
        assert!(!player.is_jumping(5));
    }
}
