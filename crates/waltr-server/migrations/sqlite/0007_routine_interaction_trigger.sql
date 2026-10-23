-- SQLx runs this rebuild in one transaction with foreign keys still enabled.
-- Snapshot children first; dropping them removes RESTRICT edges before the parent.
-- Reject unversioned indexes/triggers rather than silently discard them in a rebuild.
CREATE TEMP TABLE routine_interaction_schema_guard (valid INTEGER CHECK (valid = 1));
INSERT INTO routine_interaction_schema_guard
SELECT CASE WHEN EXISTS (
    SELECT 1 FROM sqlite_schema
    WHERE tbl_name IN ('routine_invocations', 'routine_invocation_metadata', 'routine_invocation_events')
        AND (type = 'trigger' OR (type = 'index' AND sql IS NOT NULL AND name NOT IN (
            'routine_invocations_status_admission_idx',
            'routine_invocations_routine_admission_idx',
            'routine_invocations_correlation_admission_idx',
            'routine_invocation_metadata_exact_idx',
            'routine_invocation_events_invocation_sequence_idx'
        )))
) THEN 0 ELSE 1 END;
DROP TABLE routine_interaction_schema_guard;

CREATE TEMP TABLE routine_interaction_invocations_backup AS SELECT * FROM routine_invocations;
CREATE TEMP TABLE routine_interaction_metadata_backup AS SELECT * FROM routine_invocation_metadata;
CREATE TEMP TABLE routine_interaction_events_backup AS SELECT * FROM routine_invocation_events;
CREATE TEMP TABLE routine_interaction_sequences_backup AS
    SELECT name, seq FROM sqlite_sequence
    WHERE name IN ('routine_invocations', 'routine_invocation_events');

DROP TABLE routine_invocation_events;
DROP TABLE routine_invocation_metadata;
DROP TABLE routine_invocations;

CREATE TABLE routine_invocations (
    admission_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    invocation_id TEXT NOT NULL UNIQUE CHECK (length(invocation_id) BETWEEN 1 AND 512),
    execution_id TEXT NOT NULL CHECK (length(execution_id) BETWEEN 1 AND 512),
    routine_name TEXT NOT NULL CHECK (length(routine_name) BETWEEN 1 AND 256),
    routine_revision TEXT NOT NULL CHECK (length(routine_revision) BETWEEN 1 AND 256),
    trigger_name TEXT NOT NULL CHECK (length(trigger_name) BETWEEN 1 AND 256),
    trigger_kind TEXT NOT NULL CHECK (trigger_kind IN ('manual', 'event', 'interaction')),
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

INSERT INTO routine_invocations SELECT * FROM routine_interaction_invocations_backup;
INSERT INTO routine_invocation_metadata SELECT * FROM routine_interaction_metadata_backup;
INSERT INTO routine_invocation_events SELECT * FROM routine_interaction_events_backup;

-- Explicit row restoration alone would lose high-water marks above retained rows.
DELETE FROM sqlite_sequence WHERE name IN ('routine_invocations', 'routine_invocation_events');
INSERT INTO sqlite_sequence (name, seq) SELECT name, seq FROM routine_interaction_sequences_backup;

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

DROP TABLE routine_interaction_invocations_backup;
DROP TABLE routine_interaction_metadata_backup;
DROP TABLE routine_interaction_events_backup;
DROP TABLE routine_interaction_sequences_backup;
