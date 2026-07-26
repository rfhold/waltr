CREATE TABLE outbound_oauth_credentials (
    target_key TEXT PRIMARY KEY CHECK (length(target_key) > 0),
    state_json TEXT NOT NULL,
    refresh_generation INTEGER NOT NULL CHECK (refresh_generation >= 0),
    refresh_claim_id TEXT,
    refresh_claim_expires_at_ms INTEGER,
    reauthorization_required INTEGER NOT NULL DEFAULT 0
        CHECK (reauthorization_required IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE outbound_oauth_attempts (
    attempt_id TEXT PRIMARY KEY CHECK (length(attempt_id) > 0),
    state_digest BLOB NOT NULL UNIQUE CHECK (length(state_digest) = 32),
    target_key TEXT NOT NULL CHECK (length(target_key) > 0),
    attempt_json TEXT NOT NULL,
    registration_method TEXT NOT NULL,
    completion_mode TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'completing', 'succeeded', 'failed', 'canceled', 'expired')),
    completion_claim_id TEXT,
    completion_claim_expires_at_ms INTEGER,
    failure_reason TEXT,
    expires_at_ms INTEGER NOT NULL CHECK (typeof(expires_at_ms) = 'integer'),
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX outbound_oauth_attempts_target_idx
    ON outbound_oauth_attempts(target_key, status);
