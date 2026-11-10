CREATE TABLE waltr.script_program_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    api_version TEXT NOT NULL,
    engine TEXT NOT NULL,
    envelope TEXT NOT NULL CHECK(octet_length(envelope) <= 131072),
    name TEXT NOT NULL
);
CREATE TABLE waltr.script_invocations (
    cursor BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    revision TEXT NOT NULL REFERENCES waltr.script_program_revisions(id),
    record TEXT NOT NULL CHECK(octet_length(record) <= 4096),
    status TEXT NOT NULL CHECK(status IN ('admitted','running','succeeded','failed','interrupted','uncertain')),
    admitted_ms BIGINT NOT NULL,
    updated_ms BIGINT NOT NULL,
    result TEXT CHECK(result IS NULL OR octet_length(result) <= 65536)
);
CREATE TABLE waltr.script_operations (
    cursor BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    invocation TEXT NOT NULL REFERENCES waltr.script_invocations(id),
    record TEXT NOT NULL CHECK(octet_length(record) <= 8192),
    status TEXT NOT NULL CHECK(status IN ('dispatched','completed','failed','uncertain')),
    updated_ms BIGINT NOT NULL,
    result TEXT CHECK(result IS NULL OR octet_length(result) <= 4096)
);
CREATE INDEX script_operations_invocation ON waltr.script_operations(invocation, cursor);
CREATE INDEX script_invocations_status ON waltr.script_invocations(status);
