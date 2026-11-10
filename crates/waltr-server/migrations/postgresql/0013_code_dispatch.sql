CREATE TABLE waltr.script_code_dispatch (
    principal TEXT NOT NULL,
    session_id TEXT NOT NULL,
    incarnation TEXT NOT NULL,
    tool_call_id TEXT NOT NULL,
    source_revision TEXT NOT NULL,
    invocation_id TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL DEFAULT 'associated' CHECK (state IN ('associated', 'terminal')),
    outcome TEXT,
    PRIMARY KEY (principal, session_id, incarnation, tool_call_id)
);
