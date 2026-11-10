LOCK TABLE waltr.script_program_revisions, waltr.script_invocations, waltr.script_text_results IN ACCESS EXCLUSIVE MODE;
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM waltr.script_program_revisions) OR EXISTS (SELECT 1 FROM waltr.script_invocations) OR EXISTS (SELECT 1 FROM waltr.script_text_results) THEN
        RAISE EXCEPTION 'incompatible populated Routine SQL payload state; separately authorize exact Routine archive/reset; no data was deleted';
    END IF;
END $$;
ALTER TABLE waltr.script_program_revisions DROP COLUMN envelope;
ALTER TABLE waltr.script_program_revisions ADD COLUMN object_sha256 TEXT NOT NULL CHECK(length(object_sha256) = 64);
ALTER TABLE waltr.script_program_revisions ADD COLUMN object_bytes BIGINT NOT NULL CHECK(object_bytes BETWEEN 0 AND 131072);
ALTER TABLE waltr.script_text_results DROP COLUMN content;
ALTER TABLE waltr.script_text_results ADD COLUMN object_sha256 TEXT NOT NULL CHECK(length(object_sha256) = 64);
