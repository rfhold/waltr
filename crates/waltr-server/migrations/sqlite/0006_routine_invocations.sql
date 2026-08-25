CREATE TABLE routine_invocations (
    admission_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    invocation_id TEXT NOT NULL UNIQUE CHECK (length(invocation_id) BETWEEN 1 AND 512),
    execution_id TEXT NOT NULL CHECK (length(execution_id) BETWEEN 1 AND 512),
    routine_name TEXT NOT NULL CHECK (length(routine_name) BETWEEN 1 AND 256),
    routine_revision TEXT NOT NULL CHECK (length(routine_revision) BETWEEN 1 AND 256),
    trigger_name TEXT NOT NULL CHECK (length(trigger_name) BETWEEN 1 AND 256),
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('manual', 'event')),
    correlation_id TEXT NOT NULL CHECK (length(correlation_id) BETWEEN 1 AND 512),
    source_cursor TEXT CHECK (source_cursor IS NULL OR length(source_cursor) BETWEEN 1 AND 1024),
    declared_tasks_json TEXT NOT NULL
        CHECK (length(CAST(declared_tasks_json AS BLOB)) BETWEEN 1 AND 65536 AND json_valid(declared_tasks_json)),
    status TEXT NOT NULL CHECK (status IN ('admitted', 'running', 'succeeded', 'failed', 'interrupted')),
    admitted_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    completed_at_ms INTEGER,
    updated_at_ms INTEGER NOT NULL,
    terminal_error_code TEXT CHECK (terminal_error_code IS NULL OR length(CAST(terminal_error_code AS BLOB)) <= 128),
    terminal_error_message TEXT CHECK (terminal_error_message IS NULL OR length(CAST(terminal_error_message AS BLOB)) <= 4096),
    latest_sequence INTEGER
);

CREATE TABLE routine_invocation_metadata (
    invocation_id TEXT NOT NULL REFERENCES routine_invocations(invocation_id) ON DELETE RESTRICT,
    metadata_key TEXT NOT NULL CHECK (length(CAST(metadata_key AS BLOB)) BETWEEN 1 AND 256),
    metadata_value TEXT NOT NULL CHECK (length(CAST(metadata_value AS BLOB)) <= 1024),
    PRIMARY KEY (invocation_id, metadata_key)
);

CREATE TABLE routine_invocation_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    invocation_id TEXT NOT NULL REFERENCES routine_invocations(invocation_id) ON DELETE RESTRICT,
    event_kind TEXT NOT NULL CHECK (event_kind IN (
        'invocation_admitted', 'invocation_started', 'task_started', 'task_succeeded',
        'task_skipped', 'task_blocked', 'task_failed', 'task_interrupted',
        'invocation_succeeded', 'invocation_failed', 'invocation_interrupted'
    )),
    task_name TEXT CHECK (task_name IS NULL OR length(task_name) BETWEEN 1 AND 256),
    occurred_at_ms INTEGER NOT NULL,
    error_code TEXT CHECK (error_code IS NULL OR length(CAST(error_code AS BLOB)) <= 128),
    error_message TEXT CHECK (error_message IS NULL OR length(CAST(error_message AS BLOB)) <= 4096),
    values_json TEXT CHECK (values_json IS NULL OR (length(CAST(values_json AS BLOB)) <= 65536 AND json_valid(values_json))),
    session_references_json TEXT CHECK (session_references_json IS NULL OR (length(CAST(session_references_json AS BLOB)) <= 65536 AND json_valid(session_references_json)))
);

CREATE INDEX routine_invocations_status_admission_idx
    ON routine_invocations(status, admission_sequence DESC);
CREATE INDEX routine_invocations_routine_admission_idx
    ON routine_invocations(routine_name, admission_sequence DESC);
CREATE INDEX routine_invocations_correlation_admission_idx
    ON routine_invocations(correlation_id, admission_sequence DESC);
CREATE INDEX routine_invocation_metadata_exact_idx
    ON routine_invocation_metadata(metadata_key, metadata_value, invocation_id);
CREATE INDEX routine_invocation_events_invocation_sequence_idx
    ON routine_invocation_events(invocation_id, sequence ASC);
