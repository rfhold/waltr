CREATE TABLE waltr_mcp_oauth_login_continuations (
    transaction_id TEXT PRIMARY KEY NOT NULL CHECK (length(transaction_id) > 0),
    correlation_digest TEXT NOT NULL
        CHECK (length(correlation_digest) = 64 AND correlation_digest NOT GLOB '*[^0-9a-f]*'),
    created_at_ms INTEGER NOT NULL CHECK (typeof(created_at_ms) = 'integer'),
    expires_at_ms INTEGER NOT NULL
        CHECK (typeof(expires_at_ms) = 'integer' AND expires_at_ms > created_at_ms)
);

CREATE INDEX waltr_mcp_oauth_login_continuations_expiry_idx
    ON waltr_mcp_oauth_login_continuations (expires_at_ms);
