//! Runs one uploaded mode script against a live world (mlua, Lua 5.4).
//!
//! The script is advisory: it may request position, HP, score and marker
//! changes and end the round, but it can never write simulation state
//! directly. Requests are collected through [`ScriptHost`] and applied by the
//! world after the hook returned, with the same clamping as any other change.
//!
//! Failure policy: the first error (or a budget overrun) disables the script
//! for the rest of the match, the world falls back to built-in Deathmatch
//! rules and the error is reported once. A bad upload must never stall a match.
//!
//! Hooks, identical to the builder:
//!
//! ```lua
//! function on_start()
//! function on_tick(dt)
//! function on_input(id, move_x, move_y, action)
//! function on_finish(winner)
//! ```

use std::cell::{RefCell, UnsafeCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mlua::{Error as LuaError, HookTriggers, Lua, LuaOptions, MultiValue, StdLib, Value, VmState};

use crate::lua::api::{self, HostCell, HostCellInner};
use crate::sim::script::{LuaEffects, MarkerEffect, ScriptEngine, ScriptHost};

/// Instruction budget per hook call.
pub const INSTRUCTION_BUDGET: usize = 2_000_000;
/// Instruction hook granularity.
const HOOK_STRIDE: usize = 10_000;
/// Hard wall-clock budget per hook call (the instruction hook is the primary
/// guard; this catches host functions that block).
pub const WALL_CLOCK_BUDGET: Duration = Duration::from_millis(250);
/// Memory a single script may allocate.
pub const MEMORY_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Everything a script may use. No `io`, `os`, `package`, `debug`; `load`,
/// `dofile`, `require` and `string.dump` are removed after loading.
fn std_libs() -> StdLib {
    StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE
}

/// Instruction counter shared with the hook closure.
///
/// `mlua`'s `send` feature wants the closure to be `Send + Sync`; the counter
/// is only ever touched on the thread that runs the Lua state.
struct CounterCell(UnsafeCell<usize>);

// SAFETY: see `CounterCell`; the Lua state (and with it the hook) never leaves
// its thread.
unsafe impl Send for CounterCell {}
unsafe impl Sync for CounterCell {}

/// Arguments of one hook call, turned into Lua values inside the call.
#[derive(Clone, Debug, Default)]
struct HookArgs {
    tick_delta: Option<f64>,
    input: Option<(String, f64, f64, String)>,
    winner: Option<String>,
}

pub struct LuaScript {
    package_id: String,
    source: String,
    lua: Option<Lua>,
    failure: Option<String>,
    effects: Option<LuaEffects>,
    /// Marker requests produced by the last hook (ids reserved in call order).
    marker_effects: Vec<MarkerEffect>,
    /// World handle for the current call (installed by `set_host`).
    host: Option<HostCell>,
    finish_hook_called: bool,
    calls: u64,
}

impl std::fmt::Debug for LuaScript {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaScript")
            .field("package_id", &self.package_id)
            .field("failure", &self.failure)
            .finish()
    }
}

impl LuaScript {
    pub fn new(package_id: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            package_id: package_id.into(),
            source: source.into(),
            lua: None,
            failure: None,
            effects: None,
            marker_effects: Vec::new(),
            host: None,
            finish_hook_called: false,
            calls: 0,
        }
    }

    /// Compiles the chunk without running it (upload validation).
    pub fn compile(
        package_id: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<Self, String> {
        let mut script = Self::new(package_id, source);
        script.load()?;
        Ok(script)
    }

    /// Hook calls executed so far (diagnostics).
    pub fn calls(&self) -> u64 {
        self.calls
    }

    fn fail(&mut self, message: String) {
        if self.failure.is_none() {
            self.failure = Some(message);
        }
        self.effects = None;
    }

    /// Builds the sandboxed interpreter and compiles the top-level chunk.
    fn load(&mut self) -> Result<(), String> {
        if self.lua.is_some() {
            return Ok(());
        }

        let lua = Lua::new_with(std_libs(), LuaOptions::default())
            .map_err(|error| format!("Lua runtime unavailable: {error}"))?;

        lua.set_memory_limit(MEMORY_LIMIT_BYTES)
            .map_err(|error| format!("Lua memory limit: {error}"))?;

        // Sandbox: drop what the safe libraries still expose but a game mode
        // has no business using.
        let globals = lua.globals();
        for name in [
            "load",
            "loadstring",
            "dofile",
            "loadfile",
            "require",
            "collectgarbage",
            "os",
            "io",
            "package",
            "debug",
        ] {
            globals
                .set(name, Value::Nil)
                .map_err(|error| format!("Lua sandbox ({name}): {error}"))?;
        }
        if let Ok(string_table) = globals.get::<mlua::Table>("string") {
            let _ = string_table.set("dump", Value::Nil);
        }

        // Compile once: a syntax error must be visible before the first tick.
        lua.load(&self.source)
            .set_name(self.package_id.clone())
            .into_function()
            .map_err(|error| format!("Lua load error: {}", shorten(&error.to_string())))?;

        self.lua = Some(lua);
        Ok(())
    }

    /// Called once after loading: runs the chunk (it may define hooks).
    fn run_chunk(&mut self) -> Result<(), String> {
        let lua = self.lua.as_ref().expect("loaded");
        let result = lua
            .load(&self.source)
            .set_name(self.package_id.clone())
            .exec();

        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(format!("Lua load error: {}", shorten(&error.to_string()))),
        }
    }

    /// Runs one hook; missing hooks are not an error.
    fn run_hook(&mut self, name: &str, args: HookArgs) {
        if self.is_faulted() {
            return;
        }

        if let Err(error) = self.load().and_then(|()| self.run_chunk()) {
            self.fail(error);
            return;
        }

        let Some(cell) = self.host.clone() else {
            self.fail(format!("Lua {name} error: no host installed"));
            return;
        };

        let lua = self.lua.as_ref().expect("loaded");
        let globals = lua.globals();
        let function = match globals.get::<Value>(name) {
            Ok(Value::Function(function)) => function,
            Ok(_) => {
                self.effects = Some(LuaEffects::default());
                self.marker_effects.clear();
                return;
            }
            Err(error) => {
                self.fail(format!("Lua {name} error: {}", shorten(&error.to_string())));
                return;
            }
        };

        // `lb` sees the world through the installed handle.
        if let Err(error) = self.install_api(&cell) {
            self.fail(format!("Lua {name} error: {}", shorten(&error.to_string())));
            return;
        }

        let instructions = Arc::new(CounterCell(UnsafeCell::new(0_usize)));
        let counter = instructions.clone();
        let hook = lua.set_hook(
            HookTriggers::new().every_nth_instruction(HOOK_STRIDE as u32),
            move |_lua, _debug| {
                // SAFETY: the hook runs on the Lua state's thread while it
                // executes this hook; nothing else touches the counter then.
                let count = unsafe { &mut *counter.0.get() };
                *count += HOOK_STRIDE;

                if *count > INSTRUCTION_BUDGET {
                    return Err(LuaError::runtime("instruction budget exceeded"));
                }

                Ok(VmState::Continue)
            },
        );
        if let Err(error) = hook {
            self.fail(format!("Lua {name} error: {error}"));
            return;
        }

        self.calls += 1;
        let started = Instant::now();
        let result = call_hook(&function, &args);
        let elapsed = started.elapsed();
        lua.remove_hook();

        match result.map(|_| ()) {
            Err(error) => {
                let message = error.to_string();

                if message.contains("instruction budget") {
                    self.fail(format!(
                        "Lua {name} exceeded its {INSTRUCTION_BUDGET} instruction budget."
                    ));
                } else {
                    self.fail(format!("Lua {name} error: {}", shorten(&message)));
                }
                return;
            }
            Ok(()) if elapsed > WALL_CLOCK_BUDGET => {
                self.fail(format!(
                    "Lua {name} exceeded its {} ms budget ({:.1} ms).",
                    WALL_CLOCK_BUDGET.as_millis(),
                    elapsed.as_secs_f64() * 1000.0
                ));
                return;
            }
            Ok(()) => {}
        }

        // Marker ids were reserved by the host while the hook ran; the queued
        // effects carry positions and labels only, so the ids travel with the
        // effects in call order.
        self.marker_effects = Vec::new();
        self.effects = Some(LuaEffects::default());
    }

    fn install_api(&self, cell: &HostCell) -> mlua::Result<()> {
        let lua = self.lua.as_ref().expect("loaded");
        let globals = lua.globals();
        globals.set("lb", api::install(lua, cell)?)?;
        api::install_print(lua, &globals, cell)
    }
}

fn call_hook(function: &mlua::Function, args: &HookArgs) -> mlua::Result<MultiValue> {
    match args {
        HookArgs {
            tick_delta: None,
            input: None,
            winner: None,
        } => function.call::<MultiValue>(()),
        HookArgs {
            tick_delta: Some(delta),
            ..
        } => function.call::<MultiValue>(*delta),
        HookArgs {
            input: Some((id, x, y, action)),
            ..
        } => function.call::<MultiValue>((id.as_str(), *x, *y, action.as_str())),
        HookArgs {
            winner: Some(winner),
            ..
        } => function.call::<MultiValue>(winner.as_str()),
    }
}

impl ScriptEngine for LuaScript {
    fn package_id(&self) -> &str {
        &self.package_id
    }

    fn set_host(&mut self, host: Box<dyn ScriptHost + '_>) {
        // SAFETY: mlua callbacks are `'static`, so the boxed host is erased
        // here. The world clears it (`clear_host`) before the borrow it lent
        // for this call ends, and `Audio`-style sharing across threads is
        // impossible: the Lua state itself is not `Send` and lives with the
        // world on the game thread.
        let erased: Box<dyn ScriptHost + 'static> = unsafe {
            std::mem::transmute::<Box<dyn ScriptHost + '_>, Box<dyn ScriptHost + 'static>>(host)
        };
        self.host = Some(std::sync::Arc::new(HostCellInner(RefCell::new(erased))));
    }

    fn clear_host(&mut self) {
        self.host = None;
    }

    fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn start(&mut self) {
        self.run_hook("on_start", HookArgs::default());
    }

    fn tick(&mut self, delta_seconds: f64) {
        self.run_hook(
            "on_tick",
            HookArgs {
                tick_delta: Some(delta_seconds),
                ..HookArgs::default()
            },
        );
    }

    fn input(&mut self, player_id: &str, move_x: f64, move_y: f64, action: &str) {
        self.run_hook(
            "on_input",
            HookArgs {
                input: Some((player_id.to_owned(), move_x, move_y, action.to_owned())),
                ..HookArgs::default()
            },
        );
    }

    fn finish(&mut self, winner: &str) {
        self.run_hook(
            "on_finish",
            HookArgs {
                winner: Some(winner.to_owned()),
                ..HookArgs::default()
            },
        );
    }

    fn take_effects(&mut self) -> Option<LuaEffects> {
        let mut effects = self.effects.take()?;
        effects.markers = std::mem::take(&mut self.marker_effects);
        Some(effects)
    }

    fn has_effects(&self) -> bool {
        // `lb` never queues into the host directly (it goes through the world
        // during the call), so any completed, non-faulted hook counts.
        self.effects.is_some()
    }

    fn finish_hook_called(&self) -> bool {
        self.finish_hook_called
    }

    fn mark_finish_hook_called(&mut self) {
        self.finish_hook_called = true;
    }
}

/// First line of a Lua error, at most 400 characters (what the old server
/// reported).
pub fn shorten(message: &str) -> String {
    let first = message.lines().next().unwrap_or(message);
    first.chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorten_keeps_the_first_line() {
        assert_eq!(shorten("boom\nstack traceback:"), "boom");
        assert_eq!(shorten(""), "");
    }

    #[test]
    fn a_syntax_error_fails_to_compile() {
        assert!(LuaScript::compile("broken", "local x =").is_err());
        assert!(LuaScript::compile("fine", "local x = 1").is_ok());
    }
}
