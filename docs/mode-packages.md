# Lua game modes

A mode is a package created in `LANVerseBuilder`:

```text
LANVerseBuilder/modes/coin_rush/
  mode.json     # metadata + tuning (the builder's format)
  script.lua    # hooks + lb API
```

## `mode.json`

`schema_version: 1`; the builder's `src/model.rs` is authoritative.

| Field | Meaning |
|---|---|
| `id` | slug: match id (`hello.match_id`), client folder name (`$LAN_BASH_MODES_DIR/<id>`) |
| `name`, `description` | display |
| `world.width/height` | 320..8192 x 240..8192 |
| `player.max_hp`, `player.move_speed` | 1..10000, 0..2000 |
| `round.duration_ms/countdown_ms/min_players/max_players` | 1000..3 600 000 / 0..30 000 / 1..64 / 1..64 |
| `script` | `"script.lua"` |

`preview` (bot count) is a builder-only field; the server ignores it.

## Upload

```sh
cd LANVerseBuilder
LAN_BASH_TOKEN=<token> cargo run -- publish modes/coin_rush
```

or HTTP:

```http
POST /api/v1/modes
Authorization: Bearer <token>

{"manifest": { …mode.json… }, "script": "function on_start() … end", "publish": true}
```

- `201` — stored and (if `publish`) live.
- `422` — stored, **not published**; `validation.errors` lists why. A live mode
  stays live when a new upload fails validation.

Two separate things happen on every upload:

1. **Validation**: slug and reserved ids (`lan`, `deathmatch`, `default`),
   sizes, script size and NUL bytes, a real Lua 5.4 compile (`mlua`
   `into_function`, no external checker), and a dry run in a throwaway world
   (`on_start` + three ticks).
2. **Publishing**: a valid package is promoted by pointing `live_checksum` at
   the uploaded checksum. The Lua source is shared between versions, so
   publishing never rewrites a script a running match may still execute.

The checksum is `sha256(json(manifest) . "\n" . script)` exactly as the builder
computes it (integral floats lose their fraction, following the established
JSON encoding the builder uses).

## Who plays which mode?

A mode is a match:

```json
{"type": "hello", "protocol_version": 1, "player_name": "Alice", "match_id": "coin_rush"}
```

- published id → that mode's match (created on the first join),
- anything else (empty, unknown, draft, invalid) → the shared Deathmatch.

Client: `--mode coin_rush` (or `LAN_BASH_MODE`); `deathmatch` means automatic
matchmaking. There is no "start now" switch; the join behaviour decides.

## Distribution

1. `welcome.mode = {id, name, checksum}` — not the script.
2. The client sends `{"type":"pull","what":"mode"}` once per session.
3. The server answers `{"type":"mode","mode":{id, name, manifest, script, checksum}}`.
4. The client stores it under `$LAN_BASH_MODES_DIR/<id>/`; a differing local
   copy is never overwritten (the server copy lands in `<id>_server/`).
5. Play never needs the script: positions, scores and markers are in the
   snapshots.

Lookup order on the server: Redis (`lanbash:mode-package:<id>`, 60 s TTL) →
Postgres (live versions only) → built-in game.

## Lua in the match (server-authoritative)

- One sandboxed `mlua` state (Lua 5.4, vendored) per match. Available
  libraries: `table`, `string`, `math`, `utf8`, `coroutine`; `os`, `io`,
  `package`, `debug`, `load`, `loadfile`, `dofile`, `require` and
  `string.dump` are removed.
- Hooks as in the builder: `on_start()`, `on_tick(dt)`,
  `on_input(id, mx, my, action)`, `on_finish(winner)`.
- `lb.*` never writes the simulation: it queues requests, the world clamps and
  applies them (positions inside the world, HP 0..max, scores ≥ 0, ≤ 256
  markers). Collisions, input acceptance and movement stay with the built-in
  rules.
- `"p-local"`/`"p-me"` resolve to the first player of the world.
- Budget: 2 000 000 instructions per hook (an instruction hook with a real
  deadline), plus a 250 ms wall-clock guard and a 16 MiB memory cap. The first
  violation parks the script for the rest of the match: a `mode_error` event,
  the round continues with built-in rules.
- A compile error or a failing `on_start` parks the script before tick 0, so a
  broken upload never claims a match.

### `lb` API

| Function | Result |
|---|---|
| `lb.log(msg)`, `lb.warn(msg)`, `print(…)` | mode console (`mode_log` events) |
| `lb.tick()`, `lb.time()`, `lb.tick_rate()` | match clock |
| `lb.world()` | `{width, height}` |
| `lb.players()`, `lb.player(id)` | `{id, name, x, y, hp, max_hp, score, team, is_bot}` |
| `lb.set_position(id, x, y)` | clamped to the world |
| `lb.add_score(id, n)` / `lb.set_score(id, n)` | delta / absolute, ≥ 0 |
| `lb.set_hp(id, hp)` | clamped; 0 kills (killer `"mode"`) |
| `lb.spawn_marker(x, y, label, kind)` | returns the marker id |
| `lb.remove_marker(id)`, `lb.clear_markers()`, `lb.markers()` | marker list |
| `lb.finish(winner)` / `lb.is_finished()` | end the round / query |
| `lb.random(min, max)`, `lb.seed(n)` | deterministic enough for gameplay |

## Snapshot extensions

```json
{"type":"snapshot","tick":300,"players":[…],
 "markers":[{"id":7,"x":100.5,"y":200,"label":"+1","kind":"coin"}]}
```

`markers` is absent in Deathmatch. Events from a mode: `mode_log`
(`{mode_id, level, message}`) and `mode_error` (`{mode_id, message}`).

## Differences from the former server

The former server used a small Lua 5.1-ish interpreter with observable bugs.
This server runs real Lua 5.4 (vendored in `mlua`), so:

| Former behaviour | Now |
|---|---|
| `return` inside a `while` loop in `on_tick` was ignored (king_of_the_hill kept scoring past its target) | works as written |
| requests made after an HP request in the same hook were dropped | every request applies |
| malformed chunks sometimes loaded | every chunk is compiled before the match |
| a hook could run past its budget | instruction budget enforced mid-call |

The parity fixtures (`tests/parity/fixtures`) pin the old behaviour for the
built-in game and the score/effect clamping; script semantics follow Lua 5.4.

## Limits

- A running mode match keeps the package it was created with; a re-upload only
  affects new matches.
- Markers are display only; what they mean is up to the script.
- Renaming a mode does not change its slug: the slug is the identity.
