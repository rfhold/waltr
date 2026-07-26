CREATE TABLE mcp_streamable_http_sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
    owner_key TEXT NOT NULL,
    protocol_version TEXT NOT NULL,
    lifecycle TEXT NOT NULL CHECK (
        lifecycle IN ('initialize_responded', 'initializing', 'initialized')
    ),
    initialization_operation_id TEXT NOT NULL,
    initialization_claim_id TEXT,
    initialization_claim_expires_at_ms INTEGER,
    completed_initialization_claim_id TEXT,
    event_stream_id TEXT NOT NULL,
    next_event_sequence INTEGER NOT NULL DEFAULT 1,
    delivered_event_sequence INTEGER NOT NULL DEFAULT 0,
    acknowledged_event_sequence INTEGER NOT NULL DEFAULT 0,
    pending_event_count INTEGER NOT NULL DEFAULT 0,
    pending_event_bytes INTEGER NOT NULL DEFAULT 0,
    delivery_id TEXT,
    delivery_from_sequence INTEGER,
    delivery_through_sequence INTEGER,
    delivery_lease_expires_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    CHECK (created_at_ms <= updated_at_ms AND updated_at_ms <= expires_at_ms),
    CHECK (
        acknowledged_event_sequence >= 0
        AND acknowledged_event_sequence <= delivered_event_sequence
        AND delivered_event_sequence < next_event_sequence
    ),
    CHECK (pending_event_count >= 0 AND pending_event_bytes >= 0),
    CHECK (
        (initialization_claim_id IS NULL) =
        (initialization_claim_expires_at_ms IS NULL)
    ),
    CHECK (
        lifecycle <> 'initializing'
        OR (initialization_claim_id IS NOT NULL AND initialization_claim_expires_at_ms IS NOT NULL)
    ),
    CHECK (
        lifecycle = 'initializing'
        OR (initialization_claim_id IS NULL AND initialization_claim_expires_at_ms IS NULL)
    ),
    CHECK (
        (delivery_id IS NULL AND delivery_from_sequence IS NULL
            AND delivery_through_sequence IS NULL AND delivery_lease_expires_at_ms IS NULL)
        OR
        (delivery_id IS NOT NULL AND delivery_from_sequence IS NOT NULL
            AND delivery_through_sequence IS NOT NULL AND delivery_lease_expires_at_ms IS NOT NULL
            AND delivery_from_sequence > 0
            AND delivery_from_sequence <= delivery_through_sequence
            AND delivery_through_sequence <= delivered_event_sequence)
    )
);

CREATE INDEX mcp_streamable_http_sessions_expiry_idx
ON mcp_streamable_http_sessions (expires_at_ms, session_id);

CREATE INDEX mcp_streamable_http_sessions_owner_expiry_idx
ON mcp_streamable_http_sessions (owner_key, expires_at_ms);

CREATE TABLE mcp_streamable_http_events (
    session_id TEXT NOT NULL,
    event_sequence INTEGER NOT NULL CHECK (event_sequence > 0),
    message TEXT NOT NULL,
    message_bytes INTEGER NOT NULL CHECK (
        message_bytes > 0 AND message_bytes = length(CAST(message AS BLOB))
    ),
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (session_id, event_sequence),
    FOREIGN KEY (session_id)
        REFERENCES mcp_streamable_http_sessions(session_id)
        ON DELETE CASCADE
);
