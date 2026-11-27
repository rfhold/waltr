CREATE TABLE definition_contracts (
    name TEXT PRIMARY KEY NOT NULL,
    definition TEXT NOT NULL CHECK (json_valid(definition) AND json_type(definition) = 'object')
) STRICT;
