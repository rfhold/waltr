CREATE TABLE mcp.oauth_oidc_resource_owner_attempts (
    transaction_id TEXT PRIMARY KEY CHECK (length(transaction_id) BETWEEN 1 AND 128) REFERENCES mcp.oauth_authorization_transactions(transaction_id) ON DELETE CASCADE,
    state_digest BYTEA NOT NULL UNIQUE CHECK (octet_length(state_digest) = 32),
    browser_correlation_digest BYTEA NOT NULL CHECK (octet_length(browser_correlation_digest) = 32),
    nonce TEXT NOT NULL CHECK (length(nonce) BETWEEN 1 AND 256),
    pkce_verifier TEXT NOT NULL CHECK (length(pkce_verifier) = 43),
    created_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > created_at_ms),
    callback_claimed_at_ms BIGINT,
    completed_at_ms BIGINT,
    principal_id TEXT,
    consumed_at_ms BIGINT,
    CHECK ((completed_at_ms IS NULL AND principal_id IS NULL) OR (completed_at_ms IS NOT NULL AND length(principal_id) BETWEEN 1 AND 256)),
    CHECK (consumed_at_ms IS NULL OR completed_at_ms IS NOT NULL)
);
CREATE INDEX oauth_oidc_attempts_expiry_idx ON mcp.oauth_oidc_resource_owner_attempts (expires_at_ms, transaction_id);
