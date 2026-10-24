-- Rebuild only the event table; preserve its sequence high-water mark.
CREATE TEMP TABLE routine_result_schema_guard (valid INTEGER CHECK (valid = 1));
INSERT INTO routine_result_schema_guard
SELECT CASE WHEN EXISTS (
    SELECT 1 FROM sqlite_schema
    WHERE tbl_name = 'routine_invocation_events'
        AND (type = 'trigger' OR (type = 'index' AND sql IS NOT NULL
            AND name != 'routine_invocation_events_invocation_sequence_idx'))
) THEN 0 ELSE 1 END;
DROP TABLE routine_result_schema_guard;

CREATE TEMP TABLE routine_result_events_backup AS SELECT * FROM routine_invocation_events;
CREATE TEMP TABLE routine_result_sequence_backup AS
    SELECT name, seq FROM sqlite_sequence WHERE name = 'routine_invocation_events';
DROP TABLE routine_invocation_events;

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
    values_json TEXT CHECK (values_json IS NULL OR (length(CAST(values_json AS BLOB)) <= 8388608 AND json_valid(values_json))),
    session_references_json TEXT CHECK (session_references_json IS NULL OR (length(CAST(session_references_json AS BLOB)) <= 65536 AND json_valid(session_references_json)))
);

INSERT INTO routine_invocation_events SELECT * FROM routine_result_events_backup;
DELETE FROM sqlite_sequence WHERE name = 'routine_invocation_events';
INSERT INTO sqlite_sequence (name, seq) SELECT name, seq FROM routine_result_sequence_backup;
CREATE INDEX routine_invocation_events_invocation_sequence_idx
    ON routine_invocation_events(invocation_id, sequence ASC);
DROP TABLE routine_result_events_backup;
DROP TABLE routine_result_sequence_backup;
