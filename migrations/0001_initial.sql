-- LAN Verse schema (Postgres).
--
-- Ported 1:1 from the former schema, keeping table and column
-- names so the data migration (`lanverse_game migrate`) is a straight copy and
-- clients keep their tokens. The framework's bookkeeping tables (sessions, cache,
-- jobs, password resets) are deliberately absent: nothing needs them.

CREATE TABLE users (
    id                BIGSERIAL PRIMARY KEY,
    name              TEXT NOT NULL,
    email             TEXT NOT NULL UNIQUE,
    email_verified_at TIMESTAMPTZ,
    -- Argon2id PHC string (bcrypt hashes from the old server are upgraded on
    -- the next successful login).
    password          TEXT NOT NULL,
    created_at        TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ
);

CREATE TABLE players (
    id                BIGSERIAL PRIMARY KEY,
    user_id           BIGINT NOT NULL UNIQUE REFERENCES users (id) ON DELETE CASCADE,
    display_name      VARCHAR(32) NOT NULL UNIQUE,
    avatar            VARCHAR(64),
    xp                INTEGER NOT NULL DEFAULT 0,
    level             INTEGER NOT NULL DEFAULT 1,
    matches_played    INTEGER NOT NULL DEFAULT 0,
    matches_won       INTEGER NOT NULL DEFAULT 0,
    total_score       INTEGER NOT NULL DEFAULT 0,
    kills             INTEGER NOT NULL DEFAULT 0,
    deaths            INTEGER NOT NULL DEFAULT 0,
    moderation_status VARCHAR(16) NOT NULL DEFAULT 'active',
    last_seen_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ,
    updated_at        TIMESTAMPTZ
);

CREATE INDEX players_moderation_status_index ON players (moderation_status);

-- Sanctum-compatible: `token` is the SHA-256 of the plaintext token.
CREATE TABLE personal_access_tokens (
    id             BIGSERIAL PRIMARY KEY,
    tokenable_type TEXT NOT NULL,
    tokenable_id   BIGINT NOT NULL,
    name           TEXT NOT NULL,
    token          VARCHAR(64) NOT NULL UNIQUE,
    abilities      TEXT,
    last_used_at   TIMESTAMPTZ,
    expires_at     TIMESTAMPTZ,
    created_at     TIMESTAMPTZ,
    updated_at     TIMESTAMPTZ
);

CREATE INDEX personal_access_tokens_tokenable_index ON personal_access_tokens (tokenable_type, tokenable_id);
CREATE INDEX personal_access_tokens_expires_at_index ON personal_access_tokens (expires_at);

CREATE TABLE cosmetics (
    id           BIGSERIAL PRIMARY KEY,
    slug         VARCHAR(64) NOT NULL UNIQUE,
    name         VARCHAR(64) NOT NULL,
    kind         VARCHAR(16) NOT NULL,
    unlock_level INTEGER NOT NULL DEFAULT 1,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ
);

CREATE TABLE player_cosmetic (
    id          BIGSERIAL PRIMARY KEY,
    player_id   BIGINT NOT NULL REFERENCES players (id) ON DELETE CASCADE,
    cosmetic_id BIGINT NOT NULL REFERENCES cosmetics (id) ON DELETE CASCADE,
    equipped_at TIMESTAMPTZ,
    created_at  TIMESTAMPTZ,
    updated_at  TIMESTAMPTZ,
    UNIQUE (player_id, cosmetic_id)
);

CREATE TABLE friendships (
    id         BIGSERIAL PRIMARY KEY,
    player_id  BIGINT NOT NULL REFERENCES players (id) ON DELETE CASCADE,
    friend_id  BIGINT NOT NULL REFERENCES players (id) ON DELETE CASCADE,
    status     VARCHAR(16) NOT NULL DEFAULT 'pending',
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ,
    UNIQUE (player_id, friend_id)
);

CREATE INDEX friendships_friend_id_status_index ON friendships (friend_id, status);

CREATE TABLE lobbies (
    id                     VARCHAR(26) PRIMARY KEY,
    name                   VARCHAR(64) NOT NULL,
    game_mode              VARCHAR(32) NOT NULL,
    max_players            SMALLINT NOT NULL DEFAULT 16,
    status                 VARCHAR(16) NOT NULL DEFAULT 'open',
    host_player_profile_id BIGINT REFERENCES players (id) ON DELETE SET NULL,
    created_at             TIMESTAMPTZ,
    updated_at             TIMESTAMPTZ
);

CREATE INDEX lobbies_status_created_at_index ON lobbies (status, created_at);

CREATE TABLE lobby_members (
    id         BIGSERIAL PRIMARY KEY,
    lobby_id   VARCHAR(26) NOT NULL REFERENCES lobbies (id) ON DELETE CASCADE,
    player_id  BIGINT NOT NULL REFERENCES players (id) ON DELETE CASCADE,
    team       VARCHAR(16),
    joined_at  TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ,
    UNIQUE (lobby_id, player_id)
);

CREATE TABLE matches (
    id           VARCHAR(64) PRIMARY KEY,
    game_mode    VARCHAR(32) NOT NULL,
    status       VARCHAR(16) NOT NULL DEFAULT 'open',
    round_number INTEGER NOT NULL DEFAULT 0,
    tick_rate    SMALLINT NOT NULL DEFAULT 60,
    max_players  SMALLINT NOT NULL DEFAULT 512,
    lobby_id     VARCHAR(26) REFERENCES lobbies (id) ON DELETE SET NULL,
    started_at   TIMESTAMPTZ,
    closed_at    TIMESTAMPTZ,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ
);

CREATE INDEX matches_status_created_at_index ON matches (status, created_at);

CREATE TABLE match_players (
    id                 BIGSERIAL PRIMARY KEY,
    match_id           VARCHAR(64) NOT NULL REFERENCES matches (id) ON DELETE CASCADE,
    protocol_player_id VARCHAR(32) NOT NULL,
    player_id          BIGINT REFERENCES players (id) ON DELETE SET NULL,
    display_name       VARCHAR(32) NOT NULL,
    team               VARCHAR(16),
    score              INTEGER NOT NULL DEFAULT 0,
    deaths             SMALLINT NOT NULL DEFAULT 0,
    kills              SMALLINT NOT NULL DEFAULT 0,
    is_winner          BOOLEAN NOT NULL DEFAULT FALSE,
    created_at         TIMESTAMPTZ,
    updated_at         TIMESTAMPTZ,
    UNIQUE (match_id, protocol_player_id)
);

CREATE INDEX match_players_player_id_index ON match_players (player_id);

CREATE TABLE match_rounds (
    id           BIGSERIAL PRIMARY KEY,
    match_id     VARCHAR(64) NOT NULL REFERENCES matches (id) ON DELETE CASCADE,
    round_number INTEGER NOT NULL,
    winner       VARCHAR(32),
    scores       JSONB NOT NULL,
    duration_ms  INTEGER NOT NULL,
    created_at   TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ,
    UNIQUE (match_id, round_number)
);

CREATE TABLE highscores (
    id         BIGSERIAL PRIMARY KEY,
    player_id  BIGINT NOT NULL REFERENCES players (id) ON DELETE CASCADE,
    game_mode  VARCHAR(32) NOT NULL,
    score      INTEGER NOT NULL,
    match_id   VARCHAR(64),
    created_at TIMESTAMPTZ,
    updated_at TIMESTAMPTZ
);

CREATE INDEX highscores_game_mode_score_index ON highscores (game_mode, score);
CREATE INDEX highscores_player_id_game_mode_index ON highscores (player_id, game_mode);

CREATE TABLE game_modes (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug          VARCHAR(64) NOT NULL UNIQUE,
    name          VARCHAR(64) NOT NULL,
    description   VARCHAR(255) NOT NULL DEFAULT '',
    script        TEXT NOT NULL,
    manifest      JSONB NOT NULL,
    checksum      VARCHAR(64) NOT NULL,
    status        VARCHAR(16) NOT NULL DEFAULT 'draft',
    live_checksum VARCHAR(64),
    created_by    BIGINT REFERENCES users (id) ON DELETE SET NULL,
    validated_at  TIMESTAMPTZ,
    created_at    TIMESTAMPTZ,
    updated_at    TIMESTAMPTZ
);

CREATE INDEX game_modes_status_updated_at_index ON game_modes (status, updated_at);

-- Data written by the migration tool: one row per source table and run, so a
-- repeated run is idempotent and auditable.
CREATE TABLE data_migrations (
    id         BIGSERIAL PRIMARY KEY,
    source     TEXT NOT NULL,
    run_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    report     JSONB NOT NULL
);
