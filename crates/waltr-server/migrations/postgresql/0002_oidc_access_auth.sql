CREATE TABLE waltr.oidc_login_attempts (
    state_digest TEXT PRIMARY KEY CHECK (state_digest ~ '^[0-9a-f]{64}$'),
    correlation_digest TEXT NOT NULL CHECK (correlation_digest ~ '^[0-9a-f]{64}$'),
    nonce TEXT NOT NULL CHECK (length(nonce) > 0),
    pkce_verifier TEXT NOT NULL CHECK (length(pkce_verifier) > 0),
    return_path TEXT NOT NULL
        CHECK (
            left(return_path, 1) = '/'
            AND left(return_path, 2) <> '//'
            AND position(chr(92) IN return_path) = 0
        ),
    created_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > created_at_ms),
    consumed_at_ms BIGINT CHECK (consumed_at_ms IS NULL OR consumed_at_ms >= created_at_ms)
);

CREATE INDEX oidc_login_attempts_expiry_idx
    ON waltr.oidc_login_attempts (expires_at_ms);

CREATE TABLE waltr.browser_sessions (
    token_digest TEXT PRIMARY KEY CHECK (token_digest ~ '^[0-9a-f]{64}$'),
    issuer TEXT NOT NULL CHECK (length(issuer) > 0),
    client_id TEXT NOT NULL CHECK (length(client_id) > 0),
    subject TEXT NOT NULL CHECK (length(subject) > 0),
    created_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > created_at_ms),
    last_seen_at_ms BIGINT NOT NULL
        CHECK (last_seen_at_ms >= created_at_ms AND last_seen_at_ms <= expires_at_ms),
    revoked_at_ms BIGINT CHECK (revoked_at_ms IS NULL OR revoked_at_ms >= created_at_ms)
);

CREATE INDEX browser_sessions_expiry_idx
    ON waltr.browser_sessions (expires_at_ms);
