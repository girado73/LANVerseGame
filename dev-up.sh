#!/usr/bin/env bash
# Start the local Postgres (and Redis) from docker-compose.yml and run the
# server against it. Extra arguments are passed to `cargo run`, e.g.
#   ./dev-up.sh --release -- serve        (default: -- serve)
#   ./dev-up.sh -- token --email=a@b.c
set -euo pipefail
cd "$(dirname "$0")"

if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    . ./.env
    set +a
fi

export LANBASH_DATABASE_URL="${LANBASH_DATABASE_URL:-postgres://lanverse:lanverse@127.0.0.1:5432/lanverse}"
export LANBASH_REDIS_URL="${LANBASH_REDIS_URL:-redis://127.0.0.1:6379/2}"

docker compose up -d
echo "waiting for Postgres…"
for _ in $(seq 1 30); do
    if docker compose exec -T postgres pg_isready -U "${POSTGRES_USER:-lanverse}" >/dev/null 2>&1; then
        break
    fi
    sleep 1
done

if [[ $# -eq 0 ]]; then
    exec cargo run -- serve
fi
exec cargo run -- "$@"
