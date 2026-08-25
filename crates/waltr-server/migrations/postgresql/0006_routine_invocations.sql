CREATE TABLE waltr.routine_invocations (
    admission_sequence BIGINT GENERATED ALWAYS AS IDENTITY UNIQUE,
    invocation_id TEXT PRIMARY KEY CHECK (length(invocation_id) BETWEEN 1 AND 512),
    execution_id TEXT NOT NULL CHECK (length(execution_id) BETWEEN 1 AND 512),
    routine_name TEXT NOT NULL CHECK (length(routine_name) BETWEEN 1 AND 256),
    routine_revision TEXT NOT NULL CHECK (length(routine_revision) BETWEEN 1 AND 256),
    trigger_name TEXT NOT NULL CHECK (length(trigger_name) BETWEEN 1 AND 256),
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('manual', 'event')),
    correlation_id TEXT NOT NULL CHECK (length(correlation_id) BETWEEN 1 AND 512),
    source_cursor TEXT CHECK (source_cursor IS NULL OR octet_length(source_cursor) BETWEEN 1 AND 1024),
    declared_tasks_json TEXT NOT NULL
        CHECK (octet_length(declared_tasks_json) BETWEEN 1 AND 65536 AND declared_tasks_json IS JSON),
    status TEXT NOT NULL CHECK (status IN ('admitted', 'running', 'succeeded', 'failed', 'interrupted')),
    admitted_at_ms BIGINT NOT NULL,
    started_at_ms BIGINT,
    completed_at_ms BIGINT,
    updated_at_ms BIGINT NOT NULL,
    terminal_error_code TEXT CHECK (terminal_error_code IS NULL OR octet_length(terminal_error_code) <= 128),
    terminal_error_message TEXT CHECK (terminal_error_message IS NULL OR octet_length(terminal_error_message) <= 4096),
    latest_sequence BIGINT
);

CREATE TABLE waltr.routine_invocation_metadata (
    invocation_id TEXT NOT NULL REFERENCES waltr.routine_invocations(invocation_id) ON DELETE RESTRICT,
    metadata_key TEXT NOT NULL CHECK (octet_length(metadata_key) BETWEEN 1 AND 256),
    metadata_value TEXT NOT NULL CHECK (octet_length(metadata_value) <= 1024),
    PRIMARY KEY (invocation_id, metadata_key)
);

CREATE TABLE waltr.routine_invocation_events (
    sequence BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    invocation_id TEXT NOT NULL REFERENCES waltr.routine_invocations(invocation_id) ON DELETE RESTRICT,
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'invocation_admitted', 'invocation_started', 'task_started', 'task_succeeded',
        'task_skipped', 'task_blocked', 'task_failed', 'task_interrupted',
        'invocation_succeeded', 'invocation_failed', 'invocation_interrupted'
    )),
    task_name TEXT CHECK (task_name IS NULL OR length(task_name) BETWEEN 1 AND 256),
    occurred_at_ms BIGINT NOT NULL,
    error_code TEXT CHECK (error_code IS NULL OR octet_length(error_code) <= 128),
    error_message TEXT CHECK (error_message IS NULL OR octet_length(error_message) <= 4096),
    values_json TEXT CHECK (values_json IS NULL OR (octet_length(values_json) <= 65536 AND values_json IS JSON)),
    session_references_json TEXT CHECK (session_references_json IS NULL OR (octet_length(session_references_json) <= 65536 AND session_references_json IS JSON))
);

CREATE INDEX routine_invocations_status_admission_idx
    ON waltr.routine_invocations(status, admission_sequence DESC);
CREATE INDEX routine_invocations_routine_admission_idx
    ON waltr.routine_invocations(routine_name, admission_sequence DESC);
CREATE INDEX routine_invocations_correlation_admission_idx
    ON waltr.routine_invocations(correlation_id, admission_sequence DESC);
CREATE INDEX routine_invocation_metadata_exact_idx
    ON waltr.routine_invocation_metadata(metadata_key, metadata_value, invocation_id);
CREATE INDEX routine_invocation_events_invocation_sequence_idx
    ON waltr.routine_invocation_events(invocation_id, sequence ASC);
