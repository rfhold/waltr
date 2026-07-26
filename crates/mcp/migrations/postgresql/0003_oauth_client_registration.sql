ALTER TABLE mcp.oauth_clients
    ADD COLUMN registration_method TEXT NOT NULL DEFAULT 'preconfigured'
    CHECK (registration_method IN ('preconfigured', 'dynamic', 'client_metadata_document'));

CREATE TABLE mcp.oauth_client_metadata_cache (
    snapshot_id TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    normalized_document JSONB NOT NULL,
    document_digest BYTEA NOT NULL CHECK (octet_length(document_digest) = 32),
    retrieved_at_ms BIGINT NOT NULL,
    expires_at_ms BIGINT NOT NULL,
    retain_until_ms BIGINT NOT NULL,
    CHECK (retrieved_at_ms <= expires_at_ms AND expires_at_ms <= retain_until_ms)
);
CREATE INDEX oauth_client_metadata_expiry_idx
    ON mcp.oauth_client_metadata_cache (expires_at_ms, snapshot_id);
CREATE INDEX oauth_client_metadata_retention_idx
    ON mcp.oauth_client_metadata_cache (retain_until_ms, snapshot_id);

ALTER TABLE mcp.oauth_authorization_transactions
    ADD COLUMN client_metadata_snapshot_id TEXT
    REFERENCES mcp.oauth_client_metadata_cache(snapshot_id);

CREATE TABLE mcp.oauth_dynamic_clients (
    client_id TEXT PRIMARY KEY REFERENCES mcp.oauth_clients(client_id) ON DELETE CASCADE,
    provisional_expires_at_ms BIGINT NOT NULL,
    inactivity_lifetime_ms BIGINT NOT NULL CHECK (inactivity_lifetime_ms > 0),
    promoted_principal_id TEXT,
    promoted_at_ms BIGINT,
    last_used_at_ms BIGINT NOT NULL,
    CHECK (
        (promoted_principal_id IS NULL AND promoted_at_ms IS NULL)
        OR (promoted_principal_id IS NOT NULL AND promoted_at_ms IS NOT NULL)
    )
);
CREATE INDEX oauth_dynamic_clients_capacity_idx
    ON mcp.oauth_dynamic_clients (promoted_principal_id, provisional_expires_at_ms);
CREATE INDEX oauth_dynamic_clients_inactivity_idx
    ON mcp.oauth_dynamic_clients (last_used_at_ms, client_id);

CREATE TABLE mcp.oauth_registration_rates (
    rate_key TEXT PRIMARY KEY,
    window_started_at_ms BIGINT NOT NULL,
    window_expires_at_ms BIGINT NOT NULL,
    registration_count BIGINT NOT NULL CHECK (registration_count >= 0),
    CHECK (window_expires_at_ms > window_started_at_ms)
);
CREATE INDEX oauth_registration_rates_expiry_idx
    ON mcp.oauth_registration_rates (window_expires_at_ms, rate_key);
