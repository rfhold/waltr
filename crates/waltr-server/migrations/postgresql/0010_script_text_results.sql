CREATE TABLE waltr.script_text_result_usage (
    invocation TEXT PRIMARY KEY NOT NULL REFERENCES waltr.script_invocations(id),
    encoded_bytes BIGINT NOT NULL CHECK(encoded_bytes >= 0 AND encoded_bytes <= 8388608),
    result_count BIGINT NOT NULL CHECK(result_count >= 0 AND result_count <= 32)
);
CREATE TABLE waltr.script_text_results (
    id TEXT PRIMARY KEY NOT NULL,
    invocation TEXT NOT NULL REFERENCES waltr.script_invocations(id),
    name TEXT NOT NULL CHECK(octet_length(name) BETWEEN 1 AND 64),
    descriptor TEXT NOT NULL CHECK(octet_length(descriptor) <= 1024),
    utf8_bytes BIGINT NOT NULL CHECK(utf8_bytes BETWEEN 0 AND 1048576),
    encoded_charge BIGINT NOT NULL CHECK(encoded_charge BETWEEN 1 AND 8388608),
    content BYTEA NOT NULL CHECK(octet_length(content) = utf8_bytes),
    UNIQUE(invocation, name)
);
