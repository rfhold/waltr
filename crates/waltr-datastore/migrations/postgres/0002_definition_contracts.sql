CREATE TABLE definition_contracts (
    name TEXT PRIMARY KEY,
    definition JSONB NOT NULL CHECK (jsonb_typeof(definition) = 'object')
);
