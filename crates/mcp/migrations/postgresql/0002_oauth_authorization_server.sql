CREATE TABLE mcp.oauth_clients (
    client_id TEXT PRIMARY KEY,
    display_metadata TEXT,
    client_secret_digest BYTEA CHECK (client_secret_digest IS NULL OR octet_length(client_secret_digest) >= 16),
    created_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT,
    disabled_at_ms BIGINT,
    redirect_limit BIGINT NOT NULL CHECK (redirect_limit > 0)
);
CREATE INDEX oauth_clients_expiry_idx ON mcp.oauth_clients (expires_at_ms, client_id);
CREATE TABLE mcp.oauth_client_redirect_uris (client_id TEXT NOT NULL REFERENCES mcp.oauth_clients(client_id) ON DELETE CASCADE, redirect_uri TEXT NOT NULL, PRIMARY KEY (client_id, redirect_uri));
CREATE TABLE mcp.oauth_authorization_transactions (transaction_id TEXT PRIMARY KEY, client_id TEXT NOT NULL, redirect_uri TEXT NOT NULL, resource TEXT NOT NULL, scopes TEXT NOT NULL, pkce_challenge TEXT NOT NULL, browser_correlation_digest BYTEA NOT NULL CHECK (octet_length(browser_correlation_digest) >= 16), browser_correlation_consumed_at_ms BIGINT, state_reference TEXT NOT NULL, principal_id TEXT, decision TEXT CHECK (decision IS NULL OR decision IN ('approved', 'denied')), created_at_ms BIGINT NOT NULL, expires_at_ms BIGINT NOT NULL CHECK (expires_at_ms > created_at_ms));
CREATE INDEX oauth_transactions_expiry_idx ON mcp.oauth_authorization_transactions (expires_at_ms, transaction_id);
CREATE INDEX oauth_transactions_client_idx ON mcp.oauth_authorization_transactions (client_id, expires_at_ms);
CREATE TABLE mcp.oauth_consents (principal_id TEXT NOT NULL, client_id TEXT NOT NULL, resource TEXT NOT NULL, scopes TEXT NOT NULL, created_at_ms BIGINT NOT NULL, updated_at_ms BIGINT NOT NULL, revoked_at_ms BIGINT, PRIMARY KEY (principal_id, client_id, resource, scopes));
CREATE TABLE mcp.oauth_authorization_codes (code_digest BYTEA PRIMARY KEY CHECK (octet_length(code_digest) >= 16), transaction_id TEXT NOT NULL REFERENCES mcp.oauth_authorization_transactions(transaction_id), created_at_ms BIGINT NOT NULL, expires_at_ms BIGINT NOT NULL, consumed_at_ms BIGINT);
CREATE INDEX oauth_codes_expiry_idx ON mcp.oauth_authorization_codes (expires_at_ms, code_digest);
CREATE TABLE mcp.oauth_grants (grant_id TEXT PRIMARY KEY, principal_id TEXT NOT NULL, client_id TEXT NOT NULL, resource TEXT NOT NULL, scopes TEXT NOT NULL, created_at_ms BIGINT NOT NULL, revoked_at_ms BIGINT);
CREATE INDEX oauth_grants_client_idx ON mcp.oauth_grants (client_id, revoked_at_ms);
CREATE TABLE mcp.oauth_refresh_families (family_id TEXT PRIMARY KEY, grant_id TEXT NOT NULL REFERENCES mcp.oauth_grants(grant_id), created_at_ms BIGINT NOT NULL, expires_at_ms BIGINT NOT NULL, revoked_at_ms BIGINT, revocation_reason TEXT);
CREATE INDEX oauth_refresh_families_expiry_idx ON mcp.oauth_refresh_families (expires_at_ms, family_id);
CREATE TABLE mcp.oauth_refresh_tokens (token_digest BYTEA PRIMARY KEY CHECK (octet_length(token_digest) >= 16), family_id TEXT NOT NULL REFERENCES mcp.oauth_refresh_families(family_id) ON DELETE CASCADE, generation BIGINT NOT NULL CHECK (generation >= 0), issued_at_ms BIGINT NOT NULL, expires_at_ms BIGINT NOT NULL, consumed_at_ms BIGINT, replacement_generation BIGINT, UNIQUE (family_id, generation));
CREATE INDEX oauth_refresh_tokens_expiry_idx ON mcp.oauth_refresh_tokens (expires_at_ms, token_digest);
CREATE TABLE mcp.oauth_signing_keys (issuer TEXT NOT NULL, key_id TEXT NOT NULL, algorithm TEXT NOT NULL CHECK (algorithm = 'ES256'), public_jwk JSONB NOT NULL, encrypted_private_material BYTEA, wrapping_key_id TEXT, encryption_nonce BYTEA, state TEXT NOT NULL CHECK (state IN ('pending', 'active', 'retired')), created_at_ms BIGINT NOT NULL, activated_at_ms BIGINT, retired_at_ms BIGINT, verify_until_ms BIGINT, private_until_ms BIGINT, PRIMARY KEY (issuer, key_id), CHECK ((encrypted_private_material IS NULL AND wrapping_key_id IS NULL AND encryption_nonce IS NULL) OR (octet_length(encrypted_private_material) >= 16 AND length(wrapping_key_id) > 0 AND octet_length(encryption_nonce) = 12)));
CREATE UNIQUE INDEX oauth_one_active_signing_key_idx ON mcp.oauth_signing_keys (issuer) WHERE state = 'active';
CREATE INDEX oauth_signing_keys_cleanup_idx ON mcp.oauth_signing_keys (state, verify_until_ms, issuer, key_id);
CREATE TABLE mcp.oauth_access_tokens (jti TEXT PRIMARY KEY, signing_key_issuer TEXT NOT NULL, signing_key_id TEXT NOT NULL, refresh_family_id TEXT REFERENCES mcp.oauth_refresh_families(family_id), grant_id TEXT NOT NULL REFERENCES mcp.oauth_grants(grant_id), client_id TEXT NOT NULL, principal_id TEXT NOT NULL, resource TEXT NOT NULL, scopes TEXT NOT NULL, issued_at_ms BIGINT NOT NULL, expires_at_ms BIGINT NOT NULL, revoked_at_ms BIGINT);
CREATE INDEX oauth_access_tokens_expiry_idx ON mcp.oauth_access_tokens (expires_at_ms, jti);
CREATE INDEX oauth_access_tokens_family_idx ON mcp.oauth_access_tokens (refresh_family_id, revoked_at_ms);
CREATE INDEX oauth_access_tokens_grant_idx ON mcp.oauth_access_tokens (grant_id, revoked_at_ms);
CREATE TABLE mcp.oauth_revoked_clients (client_id TEXT PRIMARY KEY, revoked_at_ms BIGINT NOT NULL);
