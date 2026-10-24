ALTER TABLE waltr.routine_invocation_events
    DROP CONSTRAINT routine_invocation_events_values_json_check;
ALTER TABLE waltr.routine_invocation_events
    ADD CONSTRAINT routine_invocation_events_values_json_check
    CHECK (values_json IS NULL OR (octet_length(values_json) <= 8388608 AND values_json IS JSON));
