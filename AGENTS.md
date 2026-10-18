# AGENTS.md – LANVerseGame (Rust server)

The authoritative backend for LAN Verse: accounts, modes, lobbies, matches,
highscores **and** the live game simulation. One binary serves everything.

## Layout

```
LANVerseGame/
  src/
    config.rs      ENV + CLI, defaults ported 1:1 from the old server
    sim/           simulation (no I/O, no clock, no DB)
    lua/           mlua runtime for mode scripts
    net/           axum WebSocket + REST (same listener)
    auth/          tokens, identities, admin key
    store/         Redis + Postgres adapters
    api/           REST routes, upload, admin page
    db/            migrations + queries
  tests/
    parity/        reference fixtures + replay harness (byte-exact)
  docs/            protocol, mode packages, deployment
```

## Rules

1. **The server is authoritative.** Clients send intent only. Every rule
   (movement, hits, score, round end) is decided in `src/sim`.
2. **`src/sim` performs no I/O** – no clock, no Redis, no database. Timing is
   ticks. This is what makes parity and replays possible.
3. **Parity is a gate.** `tests/parity/fixtures` are exports of the former
   server. Any simulation change must keep `cargo test --test parity_test`
   green or update a fixture deliberately (and say why in the commit).
   Known reference bugs are pinned as quirks in the fixtures, never silently
   "fixed" inside the port.
4. **Wire format v1 is byte-stable.** Snapshot floats keep their fraction
   (`1.0`, not `1`), positions are rounded to 2 decimals, velocities to 2,
   facing to 3, field order is fixed. `snapshot_bytes_match_the_reference`
   guards it.
5. **Lua scripts are advisory.** They queue `LuaEffects`; the world clamps and
   applies them. Hooks: `on_start`, `on_tick(dt)`, `on_input(id,mx,my,action)`,
   `on_finish(winner)`.
6. **Hostile input is neutralized, never trusted.** Decoders clamp and drop;
   the simulation re-checks.
7. **No secrets in the repository.** DB URL, admin key, HMAC secret come from
   the environment.

## Quality gates (before every commit)

```bash
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Reference implementation

The former server is gone. Its behaviour lives on in
`tests/parity/fixtures/*.json` (input script + every observable value per tick)
and in the comments at each quirk. The fixtures and the complete former
project are archived outside this repository (see `HISTORY.md`).
