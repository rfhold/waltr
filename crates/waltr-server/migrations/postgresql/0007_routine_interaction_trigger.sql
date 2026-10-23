ALTER TABLE waltr.routine_invocations
    DROP CONSTRAINT routine_invocations_trigger_kind_check,
    ADD CONSTRAINT routine_invocations_trigger_kind_check
        CHECK (trigger_kind IN ('manual', 'event', 'interaction'));
