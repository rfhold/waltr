CREATE TABLE script_program_revisions (
    id TEXT PRIMARY KEY NOT NULL,
    api_version TEXT NOT NULL,
    engine TEXT NOT NULL,
    envelope TEXT NOT NULL CHECK(length(CAST(envelope AS BLOB)) <= 131072),
    name TEXT NOT NULL
);
CREATE TABLE script_invocations (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    revision TEXT NOT NULL REFERENCES script_program_revisions(id),
    record TEXT NOT NULL CHECK(length(CAST(record AS BLOB)) <= 4096),
    status TEXT NOT NULL CHECK(status IN ('admitted','running','succeeded','failed','interrupted','uncertain')),
    admitted_ms BIGINT NOT NULL,
    updated_ms BIGINT NOT NULL,
    result TEXT CHECK(result IS NULL OR length(CAST(result AS BLOB)) <= 65536)
);
CREATE TABLE script_operations (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    invocation TEXT NOT NULL REFERENCES script_invocations(id),
    record TEXT NOT NULL CHECK(length(CAST(record AS BLOB)) <= 8192),
    status TEXT NOT NULL CHECK(status IN ('dispatched','completed','failed','uncertain')),
    updated_ms BIGINT NOT NULL,
    result TEXT CHECK(result IS NULL OR length(CAST(result AS BLOB)) <= 4096)
);
CREATE INDEX script_operations_invocation ON script_operations(invocation, cursor);
CREATE INDEX script_invocations_status ON script_invocations(status);
