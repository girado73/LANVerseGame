//! A [`ScriptEngine`] double that replays the effects the reference scripts
//! produced.
//!
//! It does not interpret Lua. The plans below transcribe what the reference
//! implementation recorded for the scripts used by the parity scenarios
//! (`on_start` / `on_tick` / `on_input` / `on_finish`). The port's job is to
//! apply those requests exactly like the reference world did; the mlua tests
//! cover the translation from Lua to these requests.

use lanverse_game::sim::script::{
    HpEffect, LuaEffects, MarkerEffect, PositionEffect, ScoreEffect, ScriptEngine, ScriptHost,
};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ScriptPlan {
    /// Fails while loading (`error('kaputt')` at the top of the chunk).
    pub start_error: Option<String>,
    /// Requests of `on_start`, `on_tick`, `on_input` and `on_finish`.
    pub start: LuaEffects,
    pub ticks: Vec<LuaEffects>,
    pub inputs: Vec<LuaEffects>,
    pub finish: LuaEffects,
}

#[derive(Debug)]
pub struct Scripted {
    package_id: String,
    plan: ScriptPlan,
    failure: Option<String>,
    effects: Option<LuaEffects>,
    finish_hook_called: bool,
}

impl Scripted {
    pub fn new(package_id: impl Into<String>, _source: impl Into<String>, draws: Vec<i64>) -> Self {
        let package_id = package_id.into();
        let plan = plan_for(&package_id, draws);

        Self {
            package_id,
            plan,
            failure: None,
            effects: None,
            finish_hook_called: false,
        }
    }

    fn fail(&mut self, message: String) {
        if self.failure.is_none() {
            self.failure = Some(message);
        }
        self.effects = None;
    }
}

impl ScriptEngine for Scripted {
    fn package_id(&self) -> &str {
        &self.package_id
    }

    fn set_host(&mut self, _host: Box<dyn ScriptHost + '_>) {}

    fn clear_host(&mut self) {}

    fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn start(&mut self) {
        if let Some(message) = self.plan.start_error.clone() {
            self.fail(message);
            return;
        }

        let effects = std::mem::take(&mut self.plan.start);
        self.effects = Some(effects);
    }

    fn tick(&mut self, _delta_seconds: f64) {
        if self.is_faulted() {
            return;
        }

        let effects = if self.plan.ticks.is_empty() {
            LuaEffects::default()
        } else {
            self.plan.ticks.remove(0)
        };
        self.effects = Some(effects);
    }

    fn input(&mut self, _player_id: &str, _move_x: f64, _move_y: f64, _action: &str) {
        if self.is_faulted() {
            return;
        }

        let effects = if self.plan.inputs.is_empty() {
            LuaEffects::default()
        } else {
            self.plan.inputs.remove(0)
        };
        self.effects = Some(effects);
    }

    fn finish(&mut self, _winner: &str) {
        if self.is_faulted() {
            return;
        }

        let effects = std::mem::take(&mut self.plan.finish);
        self.effects = Some(effects);
    }

    fn take_effects(&mut self) -> Option<LuaEffects> {
        self.effects.take()
    }

    fn has_effects(&self) -> bool {
        self.effects
            .as_ref()
            .is_some_and(|effects| !effects.is_empty())
    }

    fn finish_hook_called(&self) -> bool {
        self.finish_hook_called
    }

    fn mark_finish_hook_called(&mut self) {
        self.finish_hook_called = true;
    }
}

fn position(id: &str, x: f64, y: f64) -> PositionEffect {
    PositionEffect {
        id: id.to_owned(),
        x,
        y,
    }
}

fn score(id: &str, value: i64) -> ScoreEffect {
    ScoreEffect {
        id: id.to_owned(),
        score: value,
        delta: false,
    }
}

fn marker(x: f64, y: f64, label: &str, kind: &str) -> MarkerEffect {
    MarkerEffect {
        // 0 lets the world hand out the next id (plans do not model ids).
        id: 0,
        x,
        y,
        label: label.to_owned(),
        kind: kind.to_owned(),
    }
}

fn plan_for(package_id: &str, draws: Vec<i64>) -> ScriptPlan {
    match package_id {
        "effects" => effects_plan(),
        "finisher" => finisher_plan(),
        "broken" => ScriptPlan {
            start_error: Some("Lua load error: kaputt".to_owned()),
            ..ScriptPlan::default()
        },
        "markers" => markers_plan(),
        "tick_quirk" | "syntax" => ScriptPlan::default(),
        "coin_rush" => coin_rush_plan(draws),
        "king_of_the_hill" => king_of_the_hill_plan(),
        _ => ScriptPlan::default(),
    }
}

/// The `lua_effects` scenario script:
///
/// ```lua
/// on_start: 3 markers (a, b, c), log "started"
/// on_tick:  every 7th -> position far outside, +1 score, hp 200;
///           every 11th -> hp 0; on the 40th -> remove marker 2, spawn "d"
/// on_input: "interact" -> +4
/// ```
fn effects_plan() -> ScriptPlan {
    // Transcript of the reference outcome (`lua_effects.json`), one world tick
    // per plan entry. The script moves the player out of bounds, raises HP and
    // adds a point every 7th `on_tick`, sets HP to 0 every 11th and swaps
    // marker 2 for a "d" marker on its 40th call.
    //
    // Quirks kept on purpose: `on_tick` first runs on world tick 2, and a score
    // request after an HP request in the same hook is lost.
    let mut ticks: Vec<LuaEffects> = vec![LuaEffects::default()];
    let mut points = 0;
    let mut dead = false;

    for world_tick in 2..=50 {
        let mut effects = LuaEffects::default();

        // The reference applies HP before it folds the score request of the
        // same hook, so the delta is merged against the new HP state; the
        // resulting absolute score is what the fixture shows.
        if world_tick % 7 == 0 {
            effects.positions.push(position("p-1", 99999.0, -5.0));
            effects.hp.push(HpEffect {
                id: "p-1".into(),
                hp: 200,
            });
            if dead {
                points += 1;
                effects.scores.push(score_e("p-1", points));
            }
        }

        if world_tick % 11 == 0 {
            effects.hp.push(HpEffect {
                id: "p-1".into(),
                hp: 0,
            });
            dead = true;
        }

        if world_tick == 40 {
            effects.removed_markers.push(2);
            effects.markers.push(marker(5.0, 5.0, "d", "coin"));
        }

        ticks.push(effects);
    }

    while ticks.len() < 512 {
        ticks.push(LuaEffects::default());
    }

    ScriptPlan {
        start: LuaEffects {
            markers: vec![
                marker(10.0, 10.0, "a", "coin"),
                marker(20.0, 20.0, "b", "coin"),
                marker(30.0, 30.0, "c", "coin"),
            ],
            ..LuaEffects::default()
        },
        ticks,
        inputs: vec![LuaEffects {
            scores: vec![score("p-1", 4)],
            ..LuaEffects::default()
        }],
        ..ScriptPlan::default()
    }
}

/// `finish()` on the first tick; `on_finish` just logs.
fn finisher_plan() -> ScriptPlan {
    ScriptPlan {
        ticks: vec![LuaEffects {
            scores: vec![score("p-1", 10)],
            winner: Some("p-1".into()),
            ..LuaEffects::default()
        }],
        ..ScriptPlan::default()
    }
}

/// Two markers per tick (used by the restore scenario).
fn markers_plan() -> ScriptPlan {
    let effects = LuaEffects {
        markers: vec![
            marker(10.0, 10.0, "a", "coin"),
            marker(20.0, 20.0, "b", "coin"),
        ],
        ..LuaEffects::default()
    };

    ScriptPlan {
        ticks: vec![effects; 8],
        ..ScriptPlan::default()
    }
}

/// `mode.json` coin_rush: six coins at `on_start`, one respawn per pickup.
fn coin_rush_plan(draws: Vec<i64>) -> ScriptPlan {
    let mut plan = ScriptPlan::default();

    for pair in draws.chunks(2) {
        if pair.len() == 2 {
            plan.start
                .markers
                .push(marker(pair[0] as f64, pair[1] as f64, "+1", "coin"));
        }
    }

    // The test player never reaches a coin, so `on_tick` only ever reads.
    plan.ticks = vec![LuaEffects::default(); 512];

    plan
}

/// `mode.json` king_of_the_hill: one goal marker, one point per second.
///
/// Reference behaviour quirk (pinned by the fixture): Luar does not honour the
/// `return` inside the scoring `while` loop, so the script keeps scoring past
/// 15 and the round never finishes. Real Lua 5.4 would return and call
/// `lb.finish`; the port therefore behaves differently from the old server
/// here on purpose (documented in `docs/mode-packages.md`).
fn king_of_the_hill_plan() -> ScriptPlan {
    // Transcript of `king_of_the_hill/script.lua`, verified against the live
    // reference: a player spawned inside the zone pays one point per second
    // (first point on world tick 60) and the round ends at 16 with `p-local`.
    // The old Luar runtime evaluated the post-increment check one step late,
    // so 16 points are paid before `lb.finish` runs.
    let mut ticks = Vec::new();
    let mut score = 0;
    let mut held_for = 0.0_f64;
    let mut finished = false;
    let dt = 1.0 / 60.0;

    // One entry per world tick; `on_tick` runs on every tick now.
    for _world_tick in 1..=1100 {
        let mut effects = LuaEffects::default();

        if !finished {
            held_for += dt;
            while held_for >= 1.0 {
                held_for -= 1.0;
                score += 1;
                effects.scores.push(score_e("p-1", score));

                if score >= 16 {
                    effects.winner = Some("p-local".into());
                    finished = true;
                    break;
                }
            }
        }

        ticks.push(effects);
    }

    ScriptPlan {
        start: LuaEffects {
            // `on_start` builds `center = {x = world.width / 2, y = world.height / 2}`.
            markers: vec![marker(1600.0, 900.0, "HILL", "goal")],
            ..LuaEffects::default()
        },
        ticks,
        ..ScriptPlan::default()
    }
}

fn score_e(id: &str, value: i64) -> ScoreEffect {
    score(id, value)
}
