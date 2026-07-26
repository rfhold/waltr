CREATE TABLE mcp_oauth_clients (
    client_id TEXT PRIMARY KEY NOT NULL,
    display_metadata TEXT,
    client_secret_digest BLOB CHECK (client_secret_digest IS NULL OR length(client_secret_digest) >= 16),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    disabled_at_ms INTEGER,
    redirect_limit INTEGER NOT NULL CHECK (redirect_limit > 0)
);
CREATE INDEX mcp_oauth_clients_expiry_idx ON mcp_oauth_clients (expires_at_ms, client_id);

CREATE TABLE mcp_oauth_client_redirect_uris (
    client_id TEXT NOT NULL REFERENCES mcp_oauth_clients(client_id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL,
    PRIMARY KEY (client_id, redirect_uri)
);

CREATE TABLE mcp_oauth_authorization_transactions (
    transaction_id TEXT PRIMARY KEY NOT NULL,
    client_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    resource TEXT NOT NULL,
    scopes TEXT NOT NULL,
    pkce_challenge TEXT NOT NULL,
    browser_correlation_digest BLOB NOT NULL CHECK (length(browser_correlation_digest) >= 16),
    browser_correlation_consumed_at_ms INTEGER,
    state_reference TEXT NOT NULL,
    principal_id TEXT,
    decision TEXT CHECK (decision IS NULL OR decision IN ('approved', 'denied')),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL CHECK (expires_at_ms > created_at_ms)
);
CREATE INDEX mcp_oauth_transactions_expiry_idx ON mcp_oauth_authorization_transactions (expires_at_ms, transaction_id);
CREATE INDEX mcp_oauth_transactions_client_idx ON mcp_oauth_authorization_transactions (client_id, expires_at_ms);

CREATE TABLE mcp_oauth_consents (
    principal_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    resource TEXT NOT NULL,
    scopes TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    revoked_at_ms INTEGER,
    PRIMARY KEY (principal_id, client_id, resource, scopes)
);

CREATE TABLE mcp_oauth_authorization_codes (
    code_digest BLOB PRIMARY KEY NOT NULL CHECK (length(code_digest) >= 16),
    transaction_id TEXT NOT NULL REFERENCES mcp_oauth_authorization_transactions(transaction_id),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    consumed_at_ms INTEGER
);
CREATE INDEX mcp_oauth_codes_expiry_idx ON mcp_oauth_authorization_codes (expires_at_ms, code_digest);

CREATE TABLE mcp_oauth_grants (
    grant_id TEXT PRIMARY KEY NOT NULL,
    principal_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    resource TEXT NOT NULL,
    scopes TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    revoked_at_ms INTEGER
);
CREATE INDEX mcp_oauth_grants_client_idx ON mcp_oauth_grants (client_id, revoked_at_ms);

CREATE TABLE mcp_oauth_refresh_families (
    family_id TEXT PRIMARY KEY NOT NULL,
    grant_id TEXT NOT NULL REFERENCES mcp_oauth_grants(grant_id),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    revoked_at_ms INTEGER,
    revocation_reason TEXT
);
CREATE INDEX mcp_oauth_refresh_families_expiry_idx ON mcp_oauth_refresh_families (expires_at_ms, family_id);

CREATE TABLE mcp_oauth_refresh_tokens (
    token_digest BLOB PRIMARY KEY NOT NULL CHECK (length(token_digest) >= 16),
    family_id TEXT NOT NULL REFERENCES mcp_oauth_refresh_families(family_id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    issued_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    consumed_at_ms INTEGER,
    replacement_generation INTEGER,
    UNIQUE (family_id, generation)
);
CREATE INDEX mcp_oauth_refresh_tokens_expiry_idx ON mcp_oauth_refresh_tokens (expires_at_ms, token_digest);

CREATE TABLE mcp_oauth_signing_keys (
    issuer TEXT NOT NULL,
    key_id TEXT NOT NULL,
    algorithm TEXT NOT NULL CHECK (algorithm = 'ES256'),
    public_jwk TEXT NOT NULL,
    encrypted_private_material BLOB,
    wrapping_key_id TEXT,
    encryption_nonce BLOB,
    state TEXT NOT NULL CHECK (state IN ('pending', 'active', 'retired')),
    created_at_ms INTEGER NOT NULL,
    activated_at_ms INTEGER,
    retired_at_ms INTEGER,
    verify_until_ms INTEGER,
    private_until_ms INTEGER,
    PRIMARY KEY (issuer, key_id),
    CHECK ((encrypted_private_material IS NULL AND wrapping_key_id IS NULL AND encryption_nonce IS NULL) OR (length(encrypted_private_material) >= 16 AND length(wrapping_key_id) > 0 AND length(encryption_nonce) = 12))
);
CREATE UNIQUE INDEX mcp_oauth_one_active_signing_key_idx ON mcp_oauth_signing_keys (issuer) WHERE state = 'active';
CREATE INDEX mcp_oauth_signing_keys_cleanup_idx ON mcp_oauth_signing_keys (state, verify_until_ms, issuer, key_id);

CREATE TABLE mcp_oauth_access_tokens (
    jti TEXT PRIMARY KEY NOT NULL,
    signing_key_issuer TEXT NOT NULL,
    signing_key_id TEXT NOT NULL,
    refresh_family_id TEXT REFERENCES mcp_oauth_refresh_families(family_id),
    grant_id TEXT NOT NULL REFERENCES mcp_oauth_grants(grant_id),
    client_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    resource TEXT NOT NULL,
    scopes TEXT NOT NULL,
    issued_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    revoked_at_ms INTEGER
);
CREATE INDEX mcp_oauth_access_tokens_expiry_idx ON mcp_oauth_access_tokens (expires_at_ms, jti);
CREATE INDEX mcp_oauth_access_tokens_family_idx ON mcp_oauth_access_tokens (refresh_family_id, revoked_at_ms);
CREATE INDEX mcp_oauth_access_tokens_grant_idx ON mcp_oauth_access_tokens (grant_id, revoked_at_ms);

CREATE TABLE mcp_oauth_revoked_clients (
    client_id TEXT PRIMARY KEY NOT NULL,
    revoked_at_ms INTEGER NOT NULL
);
