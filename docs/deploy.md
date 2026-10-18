# Deployment

## One host, one process

The default is a single `lanverse_game serve` process:

1. copies on a LAN connect to `ws://<host>:8080/` (the client's default),
2. the REST control plane is on the same port under `/api/v1`,
3. the admin page listens on `127.0.0.1:8081` only.

Requirements:

- Postgres 14+ (data, accounts, modes, results). The schema is applied on start.
- Redis 6+ (world state for restarts, mode-package cache, shared default match).

Both are optional: without a database the server still plays Deathmatch with
guests, and without Redis a restart loses running worlds.

```sh
LANBASH_DATABASE_URL=postgres://lanverse:secret@127.0.0.1/lanverse \
LANBASH_REDIS_URL=redis://127.0.0.1:6379/2 \
LANBASH_RESULT_SECRET=$(openssl rand -hex 32) \
LANBASH_ADMIN_KEY=$(openssl rand -hex 16) \
./lanverse_game serve
```

## LAN deployment

- Open TCP 8080 on the host firewall. Nothing else is needed.
- The admin port stays on loopback; to reach it remotely use an SSH tunnel
  (`ssh -L 8081:127.0.0.1:8081 host`) instead of binding it to the LAN.
- Clients: `--server ws://<host>:8080` (the `host:port` shorthand also works; a
  stale `/app/<key>` suffix is accepted and ignored).

## Reverse proxy (TLS)

If the party is reachable outside the LAN, terminate TLS in front of the game
port. WebSockets need upgrade headers and must not be buffered:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_read_timeout 65s;
}
```

The admin listener should stay unreachable from outside; if a proxy must expose
it, put it behind an additional auth layer — the admin key is transmitted once
at login and protects only the session cookie.

## Split deployment (opt-in)

`lanverse_game daemon` runs the simulation without sockets. Frames are
exchanged through Redis (`lanbash:frames`, `lanbash:framerelays`). The daemon
exists for experiments; the single-process mode has no added latency and needs
no extra Redis round trips. Keep `daemon` on the same host as `serve` unless the
Redis link is solid.

## Backups and migration

- Postgres is the only durable store: `pg_dump` it before upgrades.
- Importing the former database: `LANBASH_MYSQL_URL=mysql://user:pass@host/lanbash
  lanverse_game migrate --check` first (report only), then without `--check`.
  The import is idempotent and verifies that every source row is present.
- Old client tokens keep working (only their SHA-256 is stored and copied).
- bcrypt password hashes from the old server are upgraded to Argon2id on the
  next successful login.

## Operations

- Logs: stdout, `RUST_LOG=info` by default (`debug` for frames, `trace` for
  ticks). Slow ticks (> 5 ms) are logged with the match id and player count.
- Health: `GET /api/v1/health` (public) reports database, Redis, worlds,
  players and the protocol version.
- Graceful shutdown (`SIGTERM`/`Ctrl-C`): the loop stops and every world is
  written to Redis before exit.
- Restart in the middle of a round: worlds are restored on the next join from
  `lanbash:match:<id>:state` (see `tests/protocol_test.rs`).

## Capacity

Measured on a developer laptop (release build):

| Scenario | Result |
|---|---|
| simulation tick, 100 players | p99 ≈ 2.5 µs |
| service tick incl. 20 Hz snapshots into 100 queues | p99 ≈ 19 ms |
| 100 socket clients, 60 Hz inputs, 20 Hz snapshots | 20 snapshots/client/s, 10 s run |

The simulation is far from a bottleneck; the snapshot fan-out is dominated by
copying ~19 KB per client at 20 Hz. For events beyond a few hundred clients,
raise `LANBASH_SNAPSHOT_RATE` only with care and consider splitting clients
over several matches (`hello.match_id`).
