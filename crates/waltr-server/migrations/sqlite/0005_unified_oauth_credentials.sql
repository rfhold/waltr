DROP TABLE outbound_oauth_credentials;

CREATE TABLE outbound_oauth_credentials (
    target_key TEXT PRIMARY KEY CHECK (length(target_key) > 0),
    credential_json TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    refresh_claim_id TEXT,
    refresh_claim_expires_at_ms INTEGER,
    reauthorization_required INTEGER NOT NULL DEFAULT 0
        CHECK (reauthorization_required IN (0, 1)),
    updated_at_ms INTEGER NOT NULL
);
