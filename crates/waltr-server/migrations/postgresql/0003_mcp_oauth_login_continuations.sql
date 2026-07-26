CREATE TABLE waltr.mcp_oauth_login_continuations (
    transaction_id TEXT PRIMARY KEY CHECK (length(transaction_id) > 0),
    correlation_digest TEXT NOT NULL CHECK (correlation_digest ~ '^[0-9a-f]{64}$'),
    created_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > created_at_ms)
);

CREATE INDEX mcp_oauth_login_continuations_expiry_idx
    ON waltr.mcp_oauth_login_continuations (expires_at_ms);
