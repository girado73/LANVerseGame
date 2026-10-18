//! `lb` table: the API a mode script sees.
//!
//! The functions only *queue* intents on the [`ScriptHost`]; the world clamps
//! and applies them after the hook returned. Names, arguments and return shapes
//! match the builder's editor API (`LANVerseBuilder/src/lua.rs`).
//!
//! Lua callbacks must be `'static` (mlua's non-`send` mode), so they capture an
//! owned handle: an `Rc<RefCell<Box<dyn ScriptHost>>>` that the runtime
//! installs for exactly one hook call.

use std::cell::RefCell;
use std::sync::Arc;

use mlua::{Lua, Table, Value};

use crate::sim::script::ScriptHost;

/// Shared world handle for one hook call.
///
/// `mlua`'s `send` feature requires callbacks to be `Send`, but a Lua state
/// runs on exactly one thread and the world handle must not escape the call.
/// The cell is therefore marked `Send` manually; it is created in
/// `set_host` and destroyed in `clear_host` within one synchronous hook.
pub type HostCell = Arc<HostCellInner>;

/// The `RefCell` behind [`HostCell`].
pub struct HostCellInner(pub RefCell<Box<dyn ScriptHost + 'static>>);

// SAFETY: see `HostCell`; the host is only ever touched from the thread that
// runs the hook and the containing `Lua` is not shared between threads.
unsafe impl Send for HostCellInner {}

// SAFETY: see `HostCell`; the cell is only touched on the hook's thread.
unsafe impl Sync for HostCellInner {}

impl HostCellInner {
    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, Box<dyn ScriptHost + 'static>> {
        self.0.borrow_mut()
    }
}

/// Borrows the world for the duration of one call.
///
/// Callbacks must be `'static`, so the borrow is taken *inside* the closure
/// and dropped when it returns; the cell ever holds at most one host.
fn host<'a>(cell: &'a HostCell) -> std::cell::RefMut<'a, Box<dyn ScriptHost + 'static>> {
    cell.borrow_mut()
}

/// Builds the `lb` table bound to `cell`.
pub fn install(lua: &Lua, cell: &HostCell) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    api.set("host", "lan-verse")?;

    {
        let cell = cell.clone();
        api.set(
            "log",
            lua.create_function(move |_, message: Option<Value>| {
                let text = to_text(message.as_ref());
                host(&cell).log("log", &text);
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "warn",
            lua.create_function(move |_, message: Option<Value>| {
                let text = to_text(message.as_ref());
                host(&cell).log("warn", &text);
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "tick",
            lua.create_function(move |_, ()| Ok(host(&cell).tick()))?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "tick_rate",
            lua.create_function(move |_, ()| Ok(host(&cell).tick_rate()))?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "time",
            lua.create_function(move |_, ()| {
                let host = host(&cell);
                let rate = host.tick_rate().max(1);
                Ok(host.tick() as f64 / rate as f64)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "world",
            lua.create_function(move |lua, ()| {
                let host = host(&cell);
                let (width, height) = host.world_size();
                let table = lua.create_table()?;
                table.set("width", width)?;
                table.set("height", height)?;
                Ok(table)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "players",
            lua.create_function(move |lua, ()| {
                let players = host(&cell).players();
                let list = lua.create_table_with_capacity(players.len(), 0)?;
                for (index, player) in players.iter().enumerate() {
                    list.set(index + 1, player_table(lua, player)?)?;
                }
                Ok(list)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "player",
            lua.create_function(move |lua, id: Option<Value>| {
                let Some(id) = text_arg(id.as_ref()) else {
                    return Ok(Value::Nil);
                };
                match host(&cell).find_player(&id) {
                    Some(player) => Ok(Value::Table(player_table(lua, &player)?)),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "set_position",
            lua.create_function(
                move |_, (id, x, y): (Option<Value>, Option<Value>, Option<Value>)| {
                    let (Some(id), Some(x), Some(y)) = (
                        text_arg(id.as_ref()),
                        number(x.as_ref()),
                        number(y.as_ref()),
                    ) else {
                        return Ok(());
                    };
                    let mut host = host(&cell);
                    let resolved = host.resolved_id(&id);
                    host.queue_position(&resolved, x, y);
                    Ok(())
                },
            )?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "add_score",
            lua.create_function(move |_, (id, amount): (Option<Value>, Option<Value>)| {
                let (Some(id), Some(amount)) = (text_arg(id.as_ref()), number(amount.as_ref()))
                else {
                    return Ok(());
                };
                let mut host = host(&cell);
                let resolved = host.resolved_id(&id);
                host.queue_score(&resolved, amount as i64, true);
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "set_score",
            lua.create_function(move |_, (id, score): (Option<Value>, Option<Value>)| {
                let (Some(id), Some(score)) = (text_arg(id.as_ref()), number(score.as_ref()))
                else {
                    return Ok(());
                };
                let mut host = host(&cell);
                let resolved = host.resolved_id(&id);
                host.queue_score(&resolved, score as i64, false);
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "set_hp",
            lua.create_function(move |_, (id, hp): (Option<Value>, Option<Value>)| {
                let (Some(id), Some(hp)) = (text_arg(id.as_ref()), number(hp.as_ref())) else {
                    return Ok(());
                };
                let mut host = host(&cell);
                let resolved = host.resolved_id(&id);
                host.queue_hp(&resolved, hp as i64);
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "remove_marker",
            lua.create_function(move |_, id: Option<Value>| {
                let Some(id) = number(id.as_ref()) else {
                    return Ok(false);
                };
                Ok(host(&cell).queue_remove_marker(id as i64))
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "clear_markers",
            lua.create_function(move |_, ()| {
                host(&cell).queue_clear_markers();
                Ok(true)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "markers",
            lua.create_function(move |lua, ()| {
                let markers = host(&cell).markers();
                let list = lua.create_table_with_capacity(markers.len(), 0)?;
                for (index, marker) in markers.into_iter().enumerate() {
                    let table = lua.create_table()?;
                    table.set("id", marker.id)?;
                    table.set("x", marker.x)?;
                    table.set("y", marker.y)?;
                    table.set("label", marker.label)?;
                    table.set("kind", marker.kind)?;
                    list.set(index + 1, table)?;
                }
                Ok(list)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "finish",
            lua.create_function(move |_, winner: Option<Value>| {
                let winner = text_arg(winner.as_ref());
                host(&cell).queue_finish(winner.as_deref());
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "is_finished",
            lua.create_function(move |_, ()| Ok(host(&cell).finish_requested()))?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "random",
            lua.create_function(move |_, (min, max): (Option<Value>, Option<Value>)| {
                let low = integer_arg(min.as_ref()).unwrap_or(0);
                let high = integer_arg(max.as_ref()).unwrap_or(1);
                Ok(host(&cell).random_int(low, high))
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "spawn_marker",
            lua.create_function(
                move |_,
                      (x, y, label, kind): (
                    Option<Value>,
                    Option<Value>,
                    Option<Value>,
                    Option<Value>,
                )| {
                    let (Some(x), Some(y)) = (number(x.as_ref()), number(y.as_ref())) else {
                        return Ok(Value::Nil);
                    };

                    let mut world = host(&cell);
                    if world.marker_limit_reached() {
                        world.log("warn", "marker limit reached, spawn_marker ignored");
                        return Ok(Value::Nil);
                    }

                    let label = text_arg(label.as_ref())
                        .map(|text| truncate(&text, crate::sim::world::MAX_MARKER_LABEL_LENGTH))
                        .unwrap_or_default();
                    let kind = text_arg(kind.as_ref())
                        .map(|text| truncate(&text, crate::sim::world::MAX_MARKER_KIND_LENGTH))
                        .unwrap_or_else(|| "marker".to_owned());

                    let id = world.reserve_marker_id();
                    world.queue_marker(id, x, y, &label, &kind);
                    Ok(Value::Integer(id))
                },
            )?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "remove_marker",
            lua.create_function(move |_, id: Option<Value>| {
                let Some(id) = number(id.as_ref()) else {
                    return Ok(false);
                };
                Ok(host(&cell).queue_remove_marker(id as i64))
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "clear_markers",
            lua.create_function(move |_, ()| {
                host(&cell).queue_clear_markers();
                Ok(true)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "markers",
            lua.create_function(move |lua, ()| {
                let markers = host(&cell).markers();
                let list = lua.create_table_with_capacity(markers.len(), 0)?;
                for (index, marker) in markers.into_iter().enumerate() {
                    let table = lua.create_table()?;
                    table.set("id", marker.id)?;
                    table.set("x", marker.x)?;
                    table.set("y", marker.y)?;
                    table.set("label", marker.label)?;
                    table.set("kind", marker.kind)?;
                    list.set(index + 1, table)?;
                }
                Ok(list)
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "finish",
            lua.create_function(move |_, winner: Option<Value>| {
                let winner = text_arg(winner.as_ref());
                host(&cell).queue_finish(winner.as_deref());
                Ok(())
            })?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "is_finished",
            lua.create_function(move |_, ()| Ok(host(&cell).finish_requested()))?,
        )?;
    }

    {
        let cell = cell.clone();
        api.set(
            "random",
            lua.create_function(move |_, (min, max): (Option<Value>, Option<Value>)| {
                let low = integer_arg(min.as_ref()).unwrap_or(0);
                let high = integer_arg(max.as_ref()).unwrap_or(1);
                Ok(host(&cell).random_int(low, high))
            })?,
        )?;
    }

    // Accepted for builder compatibility; randomness is seeded per process
    // (or per match in deterministic mode).
    api.set(
        "seed",
        lua.create_function(|_, _seed: Option<Value>| Ok(()))?,
    )?;

    Ok(api)
}

/// `print` replacement: one tab-separated log line.
pub fn install_print(lua: &Lua, globals: &Table, cell: &HostCell) -> mlua::Result<()> {
    let world = cell.clone();
    let print = lua.create_function(move |_, values: mlua::Variadic<Value>| {
        let text = values
            .iter()
            .map(|value| to_text(Some(value)))
            .collect::<Vec<_>>()
            .join("\t");
        host(&world).log("log", &text);
        Ok(())
    })?;

    globals.set("print", print)
}

fn player_table(lua: &Lua, player: &crate::sim::script::PlayerView) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("id", player.id.as_str())?;
    table.set("name", player.name.as_str())?;
    table.set("x", player.x)?;
    table.set("y", player.y)?;
    table.set("hp", player.hp)?;
    table.set("max_hp", player.max_hp)?;
    table.set("score", player.score)?;
    table.set("team", player.team.clone())?;
    table.set("is_bot", player.is_bot)?;
    Ok(table)
}

fn truncate(text: &str, len: usize) -> String {
    text.chars().take(len).collect()
}

fn text_arg(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_string)
        .and_then(|text| text.to_str().ok())
        .map(|text| text.to_string())
}

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(|value| match value {
        Value::Integer(number) => Some(*number as f64),
        Value::Number(number) => Some(*number),
        _ => None,
    })
}

fn integer_arg(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| match value {
        Value::Integer(number) => Some(*number),
        Value::Number(number) => Some(*number as i64),
        Value::String(text) => text.to_str().ok().and_then(|text| text.parse().ok()),
        _ => None,
    })
}

/// Log arguments: strings as-is, booleans as `true`/`false`, numbers without a
/// trailing `.0`, everything else by type.
pub fn to_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Nil) => "nil".to_owned(),
        Some(Value::Boolean(flag)) => flag.to_string(),
        Some(Value::Integer(number)) => number.to_string(),
        Some(Value::Number(number)) => {
            let value = *number;
            if value.is_finite() && value.fract() == 0.0 {
                format!("{value:.0}")
            } else {
                value.to_string()
            }
        }
        Some(Value::String(text)) => text.to_string_lossy().to_string(),
        Some(Value::Table(_)) => "table".to_owned(),
        Some(Value::Function(_)) => "function".to_owned(),
        Some(Value::Thread(_)) => "thread".to_owned(),
        Some(Value::LightUserData(_)) => "userdata".to_owned(),
        Some(Value::UserData(_)) => "userdata".to_owned(),
        Some(Value::Error(_)) => "error".to_owned(),
        Some(Value::Other(_)) => "userdata".to_owned(),
    }
}
