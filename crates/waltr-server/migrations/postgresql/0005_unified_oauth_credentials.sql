DROP TABLE waltr.outbound_oauth_credentials;

CREATE TABLE waltr.outbound_oauth_credentials (
    target_key TEXT PRIMARY KEY CHECK (length(target_key) > 0),
    credential_json TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision >= 0),
    refresh_claim_id TEXT,
    refresh_claim_expires_at_ms BIGINT,
    reauthorization_required BOOLEAN NOT NULL DEFAULT FALSE,
    updated_at_ms BIGINT NOT NULL
);
