CREATE TABLE waltr.external_capability_dispatch (
    operation_id TEXT PRIMARY KEY NOT NULL,
    association TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('uncertain', 'complete')),
    outcome TEXT,
    CHECK ((state = 'uncertain' AND outcome IS NULL) OR (state = 'complete' AND outcome IS NOT NULL))
);
