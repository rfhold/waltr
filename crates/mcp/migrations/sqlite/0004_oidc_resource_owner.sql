CREATE TABLE mcp_oauth_oidc_resource_owner_attempts (
    transaction_id TEXT PRIMARY KEY NOT NULL CHECK (length(transaction_id) BETWEEN 1 AND 128) REFERENCES mcp_oauth_authorization_transactions(transaction_id) ON DELETE CASCADE,
    state_digest BLOB NOT NULL UNIQUE CHECK (length(state_digest) = 32),
    browser_correlation_digest BLOB NOT NULL CHECK (length(browser_correlation_digest) = 32),
    nonce TEXT NOT NULL CHECK (length(nonce) BETWEEN 1 AND 256),
    pkce_verifier TEXT NOT NULL CHECK (length(pkce_verifier) = 43),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > created_at_ms),
    callback_claimed_at_ms INTEGER,
    completed_at_ms INTEGER,
    principal_id TEXT,
    consumed_at_ms INTEGER,
    CHECK ((completed_at_ms IS NULL AND principal_id IS NULL) OR (completed_at_ms IS NOT NULL AND length(principal_id) BETWEEN 1 AND 256)),
    CHECK (consumed_at_ms IS NULL OR completed_at_ms IS NOT NULL)
);
CREATE INDEX mcp_oauth_oidc_attempts_expiry_idx ON mcp_oauth_oidc_resource_owner_attempts (expires_at_ms, transaction_id);
