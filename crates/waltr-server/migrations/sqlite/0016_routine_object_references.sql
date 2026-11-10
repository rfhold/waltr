-- Refuse incompatible retained payloads even if another writer races startup preflight.
CREATE TABLE routine_object_cutover_guard (empty INTEGER NOT NULL CHECK(empty = 0));
INSERT INTO routine_object_cutover_guard SELECT COUNT(*) FROM script_program_revisions;
INSERT INTO routine_object_cutover_guard SELECT COUNT(*) FROM script_invocations;
INSERT INTO routine_object_cutover_guard SELECT COUNT(*) FROM script_text_results;
DROP TABLE routine_object_cutover_guard;
ALTER TABLE script_program_revisions DROP COLUMN envelope;
ALTER TABLE script_program_revisions ADD COLUMN object_sha256 TEXT CHECK(object_sha256 IS NOT NULL AND length(object_sha256) = 64);
ALTER TABLE script_program_revisions ADD COLUMN object_bytes BIGINT CHECK(object_bytes IS NOT NULL AND object_bytes BETWEEN 0 AND 131072);
ALTER TABLE script_text_results DROP COLUMN content;
ALTER TABLE script_text_results ADD COLUMN object_sha256 TEXT CHECK(object_sha256 IS NOT NULL AND length(object_sha256) = 64);
