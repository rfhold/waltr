ALTER TABLE mcp_oauth_clients
    ADD COLUMN registration_method TEXT NOT NULL DEFAULT 'preconfigured'
    CHECK (registration_method IN ('preconfigured', 'dynamic', 'client_metadata_document'));

CREATE TABLE mcp_oauth_client_metadata_cache (
    snapshot_id TEXT PRIMARY KEY NOT NULL,
    client_id TEXT NOT NULL,
    normalized_document TEXT NOT NULL,
    document_digest BLOB NOT NULL CHECK (length(document_digest) = 32),
    retrieved_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    retain_until_ms INTEGER NOT NULL,
    CHECK (retrieved_at_ms <= expires_at_ms AND expires_at_ms <= retain_until_ms)
);
CREATE INDEX mcp_oauth_client_metadata_expiry_idx
    ON mcp_oauth_client_metadata_cache (expires_at_ms, snapshot_id);
CREATE INDEX mcp_oauth_client_metadata_retention_idx
    ON mcp_oauth_client_metadata_cache (retain_until_ms, snapshot_id);

ALTER TABLE mcp_oauth_authorization_transactions
    ADD COLUMN client_metadata_snapshot_id TEXT
    REFERENCES mcp_oauth_client_metadata_cache(snapshot_id);

CREATE TABLE mcp_oauth_dynamic_clients (
    client_id TEXT PRIMARY KEY NOT NULL REFERENCES mcp_oauth_clients(client_id) ON DELETE CASCADE,
    provisional_expires_at_ms INTEGER NOT NULL,
    inactivity_lifetime_ms INTEGER NOT NULL CHECK (inactivity_lifetime_ms > 0),
    promoted_principal_id TEXT,
    promoted_at_ms INTEGER,
    last_used_at_ms INTEGER NOT NULL,
    CHECK (
        (promoted_principal_id IS NULL AND promoted_at_ms IS NULL)
        OR (promoted_principal_id IS NOT NULL AND promoted_at_ms IS NOT NULL)
    )
);
CREATE INDEX mcp_oauth_dynamic_clients_capacity_idx
    ON mcp_oauth_dynamic_clients (promoted_principal_id, provisional_expires_at_ms);
CREATE INDEX mcp_oauth_dynamic_clients_inactivity_idx
    ON mcp_oauth_dynamic_clients (last_used_at_ms, client_id);

CREATE TABLE mcp_oauth_registration_rates (
    rate_key TEXT PRIMARY KEY NOT NULL,
    window_started_at_ms INTEGER NOT NULL,
    window_expires_at_ms INTEGER NOT NULL,
    registration_count INTEGER NOT NULL CHECK (registration_count >= 0),
    CHECK (window_expires_at_ms > window_started_at_ms)
);
CREATE INDEX mcp_oauth_registration_rates_expiry_idx
    ON mcp_oauth_registration_rates (window_expires_at_ms, rate_key);
