CREATE TABLE waltr_oidc_login_attempts (
    state_digest TEXT PRIMARY KEY NOT NULL
        CHECK (length(state_digest) = 64 AND state_digest NOT GLOB '*[^0-9a-f]*'),
    correlation_digest TEXT NOT NULL
        CHECK (length(correlation_digest) = 64 AND correlation_digest NOT GLOB '*[^0-9a-f]*'),
    nonce TEXT NOT NULL CHECK (length(nonce) > 0),
    pkce_verifier TEXT NOT NULL CHECK (length(pkce_verifier) > 0),
    return_path TEXT NOT NULL
        CHECK (
            substr(return_path, 1, 1) = '/'
            AND substr(return_path, 1, 2) <> '//'
            AND instr(return_path, char(92)) = 0
        ),
    created_at_ms INTEGER NOT NULL CHECK (typeof(created_at_ms) = 'integer'),
    expires_at_ms INTEGER NOT NULL
        CHECK (typeof(expires_at_ms) = 'integer' AND expires_at_ms > created_at_ms),
    consumed_at_ms INTEGER
        CHECK (
            consumed_at_ms IS NULL
            OR (typeof(consumed_at_ms) = 'integer' AND consumed_at_ms >= created_at_ms)
        )
);

CREATE INDEX waltr_oidc_login_attempts_expiry_idx
    ON waltr_oidc_login_attempts (expires_at_ms);

CREATE TABLE waltr_browser_sessions (
    token_digest TEXT PRIMARY KEY NOT NULL
        CHECK (length(token_digest) = 64 AND token_digest NOT GLOB '*[^0-9a-f]*'),
    issuer TEXT NOT NULL CHECK (length(issuer) > 0),
    client_id TEXT NOT NULL CHECK (length(client_id) > 0),
    subject TEXT NOT NULL CHECK (length(subject) > 0),
    created_at_ms INTEGER NOT NULL CHECK (typeof(created_at_ms) = 'integer'),
    expires_at_ms INTEGER NOT NULL
        CHECK (typeof(expires_at_ms) = 'integer' AND expires_at_ms > created_at_ms),
    last_seen_at_ms INTEGER NOT NULL
        CHECK (
            typeof(last_seen_at_ms) = 'integer'
            AND last_seen_at_ms >= created_at_ms
            AND last_seen_at_ms <= expires_at_ms
        ),
    revoked_at_ms INTEGER
        CHECK (
            revoked_at_ms IS NULL
            OR (typeof(revoked_at_ms) = 'integer' AND revoked_at_ms >= created_at_ms)
        )
);

CREATE INDEX waltr_browser_sessions_expiry_idx
    ON waltr_browser_sessions (expires_at_ms);
