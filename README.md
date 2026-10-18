# LANVerseGame – LAN Verse game server

One Rust binary that serves everything a LAN party needs: the authoritative
game simulation, Lua mode scripts, the WebSocket protocol, the REST control
plane and an admin page.

```
LANVerseClient (raylib) ──ws://host:8080──▶ ┌─────────────────────┐
Browser / builder CLI ──http://host:8080─▶  │  LANVerseGame       │
                                            │  axum + tokio       │
                                            ├─────────────────────┤
                                            │ Postgres (sqlx)     │ accounts, modes, results
                                            │ Redis               │ world state, mode cache
                                            │ mlua (Lua 5.4)      │ one sandbox per match
                                            └─────────────────────┘
```

## Quick start

```sh
cd LANVerseGame
cargo run --release -- serve
```

- WebSocket + REST on `0.0.0.0:8080` (`LANBASH_BIND`)
- Admin page on `127.0.0.1:8081` (`LANBASH_ADMIN_BIND`), never on the LAN port

Without a database the server runs protocol-only (guests, Deathmatch). With
`LANBASH_DATABASE_URL` it enables accounts, modes, results and the admin page;
the schema is applied automatically on start (`migrate` does the same on
demand).

```sh
export LANBASH_DATABASE_URL=postgres://lanverse:secret@127.0.0.1/lanverse
export LANBASH_REDIS_URL=redis://127.0.0.1:6379/2
export LANBASH_RESULT_SECRET=$(openssl rand -hex 32)
export LANBASH_ADMIN_KEY=$(openssl rand -hex 16)
cargo run --release -- serve
```

### Local database via Docker

`docker-compose.yml` brings up Postgres 17 and Redis with local-only
credentials, and `dev-up.sh` starts it and runs the server with the matching
URLs:

```sh
cp .env.example .env        # optional, dev-up.sh falls back to the same defaults
./dev-up.sh                 # = docker compose up -d + cargo run -- serve
docker compose down         # stop; add -v to also drop the volumes
```

The Postgres URL it exports is
`postgres://lanverse:lanverse@127.0.0.1:5432/lanverse` (user/password/db
override via `POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`; port via
`POSTGRES_PORT`).

Then play with the client:

```sh
cd ../LANVerseClient
cargo run -- --server ws://127.0.0.1:8080 --player-name Alice --mode coin_rush
```

## Commands

| Command | Purpose |
|---|---|
| `serve` | WebSocket + REST + admin page (default) |
| `daemon` | simulation only (split deployment, opt-in) |
| `migrate [--check]` | copy data from the old MySQL database |
| `token --email=… [--name=…] [--revoke]` | builder tokens |
| `mode [--list] [--owner=email]` | inspect / transfer mode ownership |
| `export` | where the parity fixtures live |

## Configuration

Every setting is an environment variable; the names are the ones the former
server used.

| Variable | Default | Meaning |
|---|---|---|
| `LANBASH_BIND` | `0.0.0.0:8080` | game + REST listener |
| `LANBASH_ADMIN_BIND` | `127.0.0.1:8081` | admin + internal listener |
| `LANBASH_DATABASE_URL` | – | Postgres URL (enables accounts/modes/results) |
| `LANBASH_REDIS_URL` | `redis://127.0.0.1:6379/2` | cache and world state |
| `LANBASH_TICK_RATE` | `60` | simulation Hz |
| `LANBASH_SNAPSHOT_RATE` | `20` | snapshots per second |
| `LANBASH_MAX_PLAYERS` | `512` | per match |
| `LANBASH_DEFAULT_MATCH` | `lan` | id prefix of the shared match |
| `LANBASH_GAME_MODE` | `deathmatch` | mode name stored on match rows |
| `LANBASH_IDLE_TIMEOUT` | `10` | seconds without a frame before a socket closes |
| `LANBASH_ROUND_DURATION` | `180000` | built-in rounds (ms) |
| `LANBASH_ROUND_COUNTDOWN` | `3000` | built-in countdown (ms) |
| `LANBASH_MIN_PLAYERS` | `1` | players needed to start |
| `LANBASH_WORLD_WIDTH` / `_HEIGHT` | `3200` / `1800` | built-in world |
| `LANBASH_RESULT_SECRET` | – | HMAC key for round results |
| `LANBASH_ADMIN_KEY` | – | admin page and internal endpoints |
| `LANBASH_MYSQL_URL` | – | source database for `migrate` |
| `LANBASH_MODE_MAX_SCRIPT_BYTES` | `65536` | upload limit |
| `LANBASH_MODE_TICK_TIMEOUT` | `20` | per-hook budget (ms) |
| `LANBASH_MODE_ALLOW_PUBLISH` | `true` | `false` keeps every upload a draft |
| `LANBASH_PUBLIC_URL` | `http://127.0.0.1:8080` | URL printed by `token` |
| `RUST_LOG` | `info` | tracing filter |

## REST API (`/api/v1`)

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/health` | – | status of database, Redis, worlds, players |
| GET | `/modes` | – | published modes (`?include_drafts=1` lists all) |
| GET | `/modes/{id}` | – | full package (drafts only for their author) |
| POST | `/modes` | bearer | upload `{manifest, script, publish}` |
| POST | `/modes/{id}/publish` / `unpublish` | owner | promote / withdraw |
| DELETE | `/modes/{id}` | owner | delete a draft |
| POST | `/auth/register` | – | `{name, email, password, password_confirmation}` → `{user, player, token}` |
| POST | `/auth/login` | – | `{email, password}` → `{user, player, token}` |
| GET | `/auth/me` | bearer | account + profile |
| POST | `/auth/logout` | bearer | revoke the presenting token |
| GET/PATCH | `/profile` | bearer | own profile |
| GET | `/profile/cosmetics` | bearer | owned + locked cosmetics |
| PUT | `/profile/cosmetics/{slug}` | bearer | equip an owned cosmetic |
| GET/POST | `/friends` | bearer | list / request |
| DELETE | `/friends/{id}` | bearer | remove |
| GET/POST | `/lobbies` | bearer | list / create |
| GET | `/matches/{id}` | bearer | match + participants |
| GET | `/matches/{id}/results` | bearer | rounds |
| GET | `/highscores?game_mode=&limit=` | bearer | best score per player |

Validation errors use the established shape (`{"message", "errors": {field: [msg]}}`,
HTTP 422) so the existing builder keeps working.

## Results and signatures

A finished round is serialized as canonical JSON (sorted keys), signed with
HMAC-SHA256 using `LANBASH_RESULT_SECRET` and written only if the signature
verifies. The same path is used in-process and by `POST /internal/rounds`
(admin listener, header `X-LanVerse-Signature`). Writes are idempotent per
`(match, round)`, so a replay adds nothing.

## Layout

```
src/
  config.rs, settings.rs   configuration (env, defaults from the old server)
  sim/                     simulation: world, combat, items, rounds, snapshots
  lua/                     mlua runtime, lb API, validation, dry run
  net/                     axum: protocol, WebSocket, router, game loop, REST
  auth/                    tokens (Sanctum format), passwords, resolver
  db/                      sqlx queries (accounts, modes, matches)
  store/                   Redis, mode library, signed results
  api/                     REST handlers, admin page
migrations/                Postgres schema (applied on start)
tests/                     parity fixtures, unit, HTTP, DB, admin, load
docs/                      protocol, mode packages, deployment
```

## Quality gates

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` starts an embedded Postgres (`pg-embed`, cached after the first
run) for the database, HTTP and admin tests; without network access those tests
skip themselves with a message. The performance gates run on demand:

```sh
cargo test --release --test perf_test -- --ignored --nocapture
cargo test --release --test load_test -- --ignored --nocapture
```
