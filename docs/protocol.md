# LAN Verse – client ↔ server protocol (v1)

This document is the contract between the raylib client (`LANVerseClient`) and
the game server (`LANVerseGame`). Anything not described here is **not
authoritative** for the client.

- **Transport:** WebSocket, one JSON object per text frame.
- **Endpoint:** the root path of the game listener (`ws://host:8080/`). The old
  `/app/<key>` path is accepted as an alias and has no effect.
- **Version:** `PROTOCOL_VERSION = 1`. The client refuses a mismatched
  `welcome.protocol_version`.
- **Forward compatibility:** unknown `type`s and unknown `event`s are ignored by
  both sides, so new fields/messages can be added.

## Connection sequence

```text
client                                          server
   │  WS upgrade /                                 │
   │ ────────────────────────────────────────────►  │
   │  {"event":"pusher:connection_established",…}   │  compatibility frame
   │ ◄────────────────────────────────────────────  │  (old clients ignore it)
   │  {"type":"hello", "protocol_version":1, …}     │
   │ ────────────────────────────────────────────►  │
   │  {"type":"welcome", "player_id":…, …}          │
   │ ◄────────────────────────────────────────────  │
   │  {"type":"round_state", "phase":"countdown"}   │
   │ ◄────────────────────────────────────────────  │
   │  {"type":"input", "sequence":1, …}  (60 Hz)    │
   │ ────────────────────────────────────────────►  │
   │  {"type":"snapshot", "tick":…, "players":[…]}  │  ≤ 20 Hz
   │ ◄────────────────────────────────────────────  │
```

The `pusher:connection_established` frame is sent only for compatibility with
clients written for the former transport; it carries no game state.

### Timeouts

| Parameter           | Value |
|---------------------|-------|
| Server idle timeout | 10 s without an inbound frame ⇒ connection closed |
| Client heartbeat    | ping every 2 s, reconnect after 15 s of silence |
| Handshake           | `hello` within milliseconds of opening the socket |

The server only accepts **one input per connection per tick**; additional
inputs are dropped (the newest intent of a tick wins for the next one).

## Client → server

### `hello`

```json
{
  "type": "hello",
  "protocol_version": 1,
  "player_name": "Alice",
  "token": "1|Xyz…",            // optional: account token
  "match_id": "coin_rush",      // optional: mode id, otherwise the shared match
  "client_version": "0.1.0"
}
```

- `player_name`: 1–32 characters after removing control characters.
- `token`: issued by `POST /api/v1/auth/register|login` or `lanverse_game token`.
  Without a token the player is a guest. An invalid/expired token is a **fatal**
  error.
- `match_id`: the id of a published mode becomes that mode's match; anything
  else (unknown, draft, invalid) falls back to the built-in Deathmatch.

### `input`

```json
{
  "type": "input",
  "client_tick": 18420,
  "sequence": 815,
  "movement": { "x": 1, "y": 0 },
  "actions": ["jump"]
}
```

| field | meaning |
|---|---|
| `client_tick` | the client's estimate of the server tick (informational) |
| `sequence` | monotonic per connection; the highest processed value is echoed in `snapshot.acked_input_sequence` |
| `movement` | intent, length ≤ 1; values outside `[-1,1]` are clamped, non-numbers become 0 |
| `actions` | edge-triggered: `jump`, `dash`, `use_item`, `interact`, `emote` |

Actions are applied when the input is accepted (dash needs a movement
intent; cooldowns are enforced).

### `ping`

```json
{ "type": "ping", "client_time_ms": 1739184020123 }
```

### `pull`

```json
{ "type": "pull", "what": "mode" }
```

Asks for the Lua package of the current match (`mode` answer below).

## Server → client

### `welcome`

```json
{
  "type": "welcome",
  "protocol_version": 1,
  "player_id": "p-17",
  "session_id": "s-…",
  "match_id": "coin_rush",
  "tick_rate": 60,
  "server_tick": 0,
  "mode": { "id": "coin_rush", "name": "Coin Rush", "checksum": "…" }
}
```

`mode` is present only for Lua modes. Authenticated players get the stable id
`p-<profile id>`; guests get a random `p-<10 chars>`.

### `snapshot`

```json
{
  "type": "snapshot",
  "tick": 1204,
  "acked_input_sequence": 812,
  "round": { "round_id": "r-3", "time_left_ms": 42000 },
  "players": [
    {
      "id": "p-17",
      "name": "Alice",
      "position": { "x": 120.5, "y": 90 },
      "velocity": { "x": 0, "y": 0 },
      "facing": { "x": 1, "y": 0 },
      "hp": 100,
      "max_hp": 100,
      "is_alive": true,
      "score": 3,
      "team": "red"
    }
  ],
  "markers": [ { "id": 7, "x": 100.5, "y": 200, "label": "+1", "kind": "coin" } ]
}
```

- Positions and velocities are rounded to 2 decimals, facing to 3.
- Floats always keep a fractional part (`1.0`, never `1`).
- `players` is the complete roster (players missing are hidden).
- `markers` is omitted for built-in matches; it is the complete marker list of a
  Lua mode and replaces the previous one.

### `event`

```json
{ "type": "event", "event": "player_joined", "player_id": "p-18", "name": "Bob" }
{ "type": "event", "event": "player_left",   "player_id": "p-18" }
{ "type": "event", "event": "player_hit",    "attacker": "p-17", "victim": "p-18",
  "damage": 12, "remaining_hp": 88, "position": { "x": 140, "y": 92 } }
{ "type": "event", "event": "player_died",   "player_id": "p-18", "killer": "p-17" }
{ "type": "event", "event": "item_picked",   "player_id": "p-17", "item_id": "i-9" }
{ "type": "event", "event": "score_changed", "player_id": "p-17", "score": 4, "team": "red" }
{ "type": "event", "event": "mode_log",      "mode_id": "coin_rush", "level": "log", "message": "…" }
{ "type": "event", "event": "mode_error",    "mode_id": "coin_rush", "message": "…" }
```

`killer` is `"mode"` when a script removed the HP.

### `round_state`

```json
{
  "type": "round_state",
  "phase": "countdown",
  "round": "r-3",
  "round_number": 3,
  "countdown_ms": 3000,
  "winner": "red"
}
```

`phase` is one of `connecting`, `lobby`, `countdown`, `running`, `finished`.
Only `running` accepts gameplay input.

### `mode`

Answer to `pull`: `{ "type": "mode", "mode": { id, name, manifest, script, checksum } }`
or `{ "type": "mode", "mode": null }` for built-in matches.

### `pong`

```json
{ "type": "pong", "client_time_ms": 1739184020123, "server_time_ms": 1739184020125 }
```

### `error`

```json
{ "type": "error", "code": "auth_failed", "message": "Invalid token.", "fatal": true }
```

Codes: `protocol_mismatch`, `auth_failed`, `match_full`, `match_not_found`,
`invalid_hello`, `invalid_message`, `bad_request`, `server_error`.
`fatal: true` stops reconnecting (protocol mismatch, invalid token, server
error); everything else is recoverable.

## REST control plane

All under `/api/v1` on the same port. See `README.md` for the full table;
`GET /health` and `GET /modes` are public, everything else needs
`Authorization: Bearer <token>`.

The admin page (`/admin`) and the internal result endpoint (`/internal/rounds`)
are served on the separate admin listener (`LANBASH_ADMIN_BIND`, default
`127.0.0.1:8081`) and are **not** reachable from the LAN port.
