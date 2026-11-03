-- Spawn initial schema. Event ordering and command leasing are enforced here so
-- the guarantees do not depend on a single application process.

CREATE TABLE IF NOT EXISTS spawn_session_heads (
    session_id       TEXT PRIMARY KEY,
    next_seq         BIGINT NOT NULL CHECK (next_seq >= 0),
    last_occurred_at BIGINT NOT NULL
);

CREATE TABLE IF NOT EXISTS spawn_events (
    id              BIGSERIAL PRIMARY KEY,
    event_id        TEXT NOT NULL UNIQUE,
    agent_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    execution_id    TEXT,
    session_seq     BIGINT NOT NULL,
    source          TEXT NOT NULL,
    source_event_id TEXT,
    kind            JSONB NOT NULL,
    artifact        JSONB,
    occurred_at     BIGINT NOT NULL,
    recorded_at     BIGINT NOT NULL,
    UNIQUE (session_id, source, source_event_id)
);

CREATE INDEX IF NOT EXISTS idx_spawn_events_global
    ON spawn_events (id);
CREATE INDEX IF NOT EXISTS idx_spawn_events_session
    ON spawn_events (session_id, session_seq);

CREATE TABLE IF NOT EXISTS spawn_commands (
    command_id      TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL,
    session_id      TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    principal       JSONB NOT NULL,
    action          JSONB NOT NULL,
    status          JSONB NOT NULL,
    lease_attempt   BIGINT NOT NULL DEFAULT 0,
    submitted_at    BIGINT NOT NULL,
    UNIQUE (idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_spawn_commands_dispatch
    ON spawn_commands (session_id, submitted_at, command_id);
