# HANDOVER – LANVerseGame

State: **complete**. The server serves the simulation, Lua modes, the WebSocket
protocol, the REST control plane and the admin page from one binary.

## Verify it

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test                                   # 158 tests
cargo test --release --test perf_test -- --ignored --nocapture
cargo test --release --test load_test -- --ignored --nocapture
```

Live smoke test (needs Postgres for accounts and modes; none needed for a
Deathmatch-only event):

```sh
LANBASH_DATABASE_URL=postgres://… LANBASH_RESULT_SECRET=… LANBASH_ADMIN_KEY=… \
  cargo run --release -- serve
#   http://127.0.0.1:8080/api/v1/health
#   http://127.0.0.1:8081/admin
cd ../LANVerseClient && cargo run -- --headless --max-ticks 600
```

## What is covered by which test

| Area | Tests |
|---|---|
| Simulation against the original server, tick by tick | `tests/parity_test.rs`, `tests/parity/fixtures/*.json` |
| World rules in isolation | `tests/world_test.rs` |
| Lua runtime (`lb`, sandbox, budget, failure policy) | `tests/lua_test.rs` |
| Protocol/decoder and game service | `src/net/protocol.rs`, `tests/protocol_test.rs` |
| Real sockets (two clients, ping, alias path, HTTP) | `tests/socket_test.rs` |
| Database: migrations, tokens, modes, results, profiles | `tests/db_test.rs` (embedded Postgres) |
| REST API over HTTP | `tests/http_test.rs` |
| Admin page: key, CSRF, upload, tokens, signed results | `tests/admin_test.rs` |
| Migration tool helpers | `src/migrate.rs` tests |
| Performance and load | `tests/perf_test.rs`, `tests/load_test.rs` (ignored) |

## Decisions worth knowing

- **The game loop owns the simulation on its own thread**, without a runtime;
  HTTP handlers only read published statistics. This is why the loop never
  stalls on a database call (the old server could).
- **Lua states are not `Send`.** `GameService` is created and used entirely on
  the loop thread (`ServiceFactory`), so a script can never migrate between
  threads. The two `unsafe impl Send/Sync` in `src/lua/api.rs` are scoped to
  that invariant; do not share a `LuaScript` across threads.
- **Snapshots are serialized once per tick** and shared between connections;
  only the acknowledged sequence is spliced per connection.
- **One input per connection and tick** is accepted; faster clients keep only
  their newest input (their older ones never reach the world, so movement
  intent is never cancelled by the client's own frame).
- **Results are signed** with HMAC-SHA256 over canonical JSON and written
  idempotently per `(match, round)`. The same code path backs the internal HTTP
  endpoint.
- **The admin page and `/internal` live on their own listener** (loopback by
  default) and are not routed from the LAN port.

## Known limits

- The snapshot fan-out costs time proportional to players × connections
  (measured ~19 ms per tick with 100 connections × 100 players in one process).
  For very large events, spread players over several matches (different
  `hello.match_id`) or raise the snapshot interval.
- Running mode matches keep the package they started with; re-uploads affect
  new matches only.
- Redis is optional, but without it a restart loses running worlds.
- `lanverse_game daemon` (split deployment) exists but is untested under load;
  the single-process mode is the supported deployment.
