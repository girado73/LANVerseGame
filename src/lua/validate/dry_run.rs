//! Runs a submitted script once in a throwaway world.

use crate::config::GameplayConfig;
use crate::config::SimulationConfig;
use crate::lua::runtime::LuaScript;
use crate::sim::round_phase::RoundPhase;
use crate::sim::script::ScriptEngine;
use crate::sim::world::World;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DryRunReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Compiles the script, starts it in an inert world with one player and
/// advances three ticks. Nothing outside that world is touched.
pub fn run(script: &str) -> DryRunReport {
    let mut config = SimulationConfig::from_parts(60, 20, 8, 30, GameplayConfig::inert());
    config.gameplay.round.countdown_ms = 0;

    let runner = match LuaScript::compile("dry-run", script) {
        Ok(runner) => runner,
        Err(error) => {
            return DryRunReport {
                errors: vec![error],
                warnings: Vec::new(),
            };
        }
    };

    let mut world: World<LuaScript> = World::new("m-dry-run", config);
    world.add_player("p-dry", "Dry Run", None);
    world.attach_script(runner, false);

    for _ in 0..3 {
        world.advance();
        if world.phase == RoundPhase::Running {
            break;
        }
    }

    if world.script_faulted() {
        let failure = world
            .script
            .take()
            .and_then(|script| script.failure().map(str::to_owned))
            .unwrap_or_else(|| "the script failed".to_owned());

        return DryRunReport {
            errors: vec![failure],
            warnings: Vec::new(),
        };
    }

    let logs = world.drain_lua_logs();
    let mut warnings = Vec::new();

    if logs.is_empty() {
        // Informational: a script without hooks still loads fine.
        warnings.push("script produced no output in the dry run".to_owned());
    }

    DryRunReport {
        errors: Vec::new(),
        warnings,
    }
}
