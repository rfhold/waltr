CREATE TABLE records (
    namespace TEXT NOT NULL,
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    fields TEXT NOT NULL CHECK (
        length(CAST(fields AS BLOB)) <= 131072
        AND CASE WHEN json_valid(fields) THEN json_type(fields) = 'object' ELSE 0 END
    ),
    created_by TEXT,
    last_edited_by TEXT,
    PRIMARY KEY (namespace, collection, id)
) STRICT;

CREATE TABLE relation_rules (
    namespace TEXT NOT NULL,
    relation TEXT NOT NULL,
    from_collection TEXT NOT NULL,
    to_collection TEXT NOT NULL,
    cardinality TEXT NOT NULL CHECK (cardinality IN ('one-to-one', 'one-to-many', 'many-to-many')),
    PRIMARY KEY (namespace, relation)
) STRICT;

CREATE TABLE links (
    namespace TEXT NOT NULL,
    relation TEXT NOT NULL,
    from_collection TEXT NOT NULL,
    from_id TEXT NOT NULL,
    to_collection TEXT NOT NULL,
    to_id TEXT NOT NULL,
    cardinality TEXT NOT NULL,
    created_by TEXT,
    last_edited_by TEXT,
    PRIMARY KEY (namespace, relation, from_id, to_id)
) STRICT;

CREATE UNIQUE INDEX links_from_unique ON links (namespace, relation, from_id)
    WHERE cardinality = 'one-to-one';
CREATE UNIQUE INDEX links_to_unique ON links (namespace, relation, to_id)
    WHERE cardinality IN ('one-to-one', 'one-to-many');
CREATE INDEX links_inbound ON links (namespace, relation, to_id, from_id);
CREATE INDEX links_from_record ON links (namespace, from_collection, from_id);
CREATE INDEX links_to_record ON links (namespace, to_collection, to_id);

CREATE TABLE commit_receipts (
    invocation TEXT PRIMARY KEY NOT NULL
) STRICT;

CREATE TABLE field_indexes (
    namespace TEXT NOT NULL,
    collection TEXT NOT NULL,
    id TEXT NOT NULL,
    field TEXT NOT NULL,
    kind TEXT NOT NULL,
    text_value TEXT,
    integer_value INTEGER,
    number_value REAL,
    boolean_value INTEGER,
    PRIMARY KEY (namespace, collection, id, field)
) STRICT;

CREATE INDEX field_text ON field_indexes (namespace, collection, field, text_value, id);
CREATE INDEX field_integer ON field_indexes (namespace, collection, field, integer_value, id);
CREATE INDEX field_number ON field_indexes (namespace, collection, field, number_value, id);
CREATE INDEX field_boolean ON field_indexes (namespace, collection, field, boolean_value, id);
CREATE INDEX field_kind ON field_indexes (namespace, collection, field, kind, id);

CREATE VIEW record_field_values AS
    SELECT r.namespace, r.collection, r.id, f.key AS field, f.type AS kind,
        CASE WHEN f.type = 'text' AND length(CAST(f.atom AS BLOB)) <= 1024 THEN f.atom END AS text_value,
        CASE WHEN f.type = 'integer' AND typeof(f.atom) = 'integer' THEN f.atom END AS integer_value,
        CASE WHEN f.type IN ('integer', 'real') AND abs(CAST(f.atom AS REAL)) <= 1.7976931348623157e308
            THEN CAST(f.atom AS REAL) END AS number_value,
        CASE WHEN f.type IN ('true', 'false') THEN f.atom END AS boolean_value
    FROM records AS r, json_each(r.fields) AS f
    WHERE length(CAST(f.key AS BLOB)) <= 128;

-- SQLite foreign keys and replacement delete triggers may be disabled on stock connections.
CREATE TRIGGER record_insert_guard BEFORE INSERT ON records BEGIN
    SELECT RAISE(ABORT, 'record exists') WHERE EXISTS (
        SELECT 1 FROM records WHERE namespace = NEW.namespace AND collection = NEW.collection AND id = NEW.id
    );
    SELECT RAISE(ABORT, 'duplicate field') WHERE EXISTS (
        SELECT 1 FROM json_each(NEW.fields) GROUP BY key HAVING count(*) > 1
    );
END;

CREATE TRIGGER record_keys BEFORE UPDATE OF namespace, collection, id ON records BEGIN
    SELECT RAISE(ABORT, 'immutable record identity')
    WHERE (OLD.namespace, OLD.collection, OLD.id) <> (NEW.namespace, NEW.collection, NEW.id);
END;

CREATE TRIGGER record_update_guard BEFORE UPDATE OF fields ON records BEGIN
    SELECT RAISE(ABORT, 'duplicate field') WHERE EXISTS (
        SELECT 1 FROM json_each(NEW.fields) GROUP BY key HAVING count(*) > 1
    );
END;

CREATE TRIGGER record_insert AFTER INSERT ON records BEGIN
    UPDATE records SET created_by = NULL, last_edited_by = NULL
        WHERE namespace = NEW.namespace AND collection = NEW.collection AND id = NEW.id;
    INSERT INTO field_indexes SELECT * FROM record_field_values
        WHERE namespace = NEW.namespace AND collection = NEW.collection AND id = NEW.id;
END;

CREATE TRIGGER record_update AFTER UPDATE OF fields ON records BEGIN
    UPDATE records SET created_by = OLD.created_by, last_edited_by = NULL
        WHERE namespace = NEW.namespace AND collection = NEW.collection AND id = NEW.id;
    DELETE FROM field_indexes WHERE namespace = OLD.namespace AND collection = OLD.collection AND id = OLD.id;
    INSERT INTO field_indexes SELECT * FROM record_field_values
        WHERE namespace = NEW.namespace AND collection = NEW.collection AND id = NEW.id;
END;

CREATE TRIGGER record_delete AFTER DELETE ON records BEGIN
    DELETE FROM field_indexes WHERE namespace = OLD.namespace AND collection = OLD.collection AND id = OLD.id;
    DELETE FROM links WHERE namespace = OLD.namespace AND from_collection = OLD.collection AND from_id = OLD.id;
    DELETE FROM links WHERE namespace = OLD.namespace AND to_collection = OLD.collection AND to_id = OLD.id;
END;

CREATE TRIGGER rule_insert BEFORE INSERT ON relation_rules BEGIN
    SELECT RAISE(ABORT, 'rule exists') WHERE EXISTS (
        SELECT 1 FROM relation_rules WHERE namespace = NEW.namespace AND relation = NEW.relation
    );
END;

CREATE TRIGGER rule_update BEFORE UPDATE ON relation_rules BEGIN
    SELECT RAISE(ABORT, 'immutable relation rule');
END;

CREATE TRIGGER rule_delete BEFORE DELETE ON relation_rules BEGIN
    SELECT RAISE(ABORT, 'rule still in use') WHERE EXISTS (
        SELECT 1 FROM links WHERE namespace = OLD.namespace AND relation = OLD.relation
    );
END;

CREATE TRIGGER link_insert_guard BEFORE INSERT ON links BEGIN
    SELECT RAISE(ABORT, 'invalid relation rule') WHERE NOT EXISTS (
        SELECT 1 FROM relation_rules
        WHERE namespace = NEW.namespace AND relation = NEW.relation
          AND (from_collection, to_collection, cardinality) = (NEW.from_collection, NEW.to_collection, NEW.cardinality)
    );
    SELECT RAISE(ABORT, 'missing endpoint') WHERE NOT EXISTS (
        SELECT 1 FROM records WHERE namespace = NEW.namespace AND collection = NEW.from_collection AND id = NEW.from_id
    ) OR NOT EXISTS (
        SELECT 1 FROM records WHERE namespace = NEW.namespace AND collection = NEW.to_collection AND id = NEW.to_id
    );
    SELECT RAISE(ABORT, 'link conflict') WHERE EXISTS (
        SELECT 1 FROM links WHERE namespace = NEW.namespace AND relation = NEW.relation AND
            ((from_id = NEW.from_id AND to_id = NEW.to_id)
                OR (NEW.cardinality = 'one-to-one' AND from_id = NEW.from_id)
                OR (NEW.cardinality IN ('one-to-one', 'one-to-many') AND to_id = NEW.to_id))
    );
END;

CREATE TRIGGER link_keys BEFORE UPDATE OF namespace, relation, from_collection, from_id, to_collection, to_id, cardinality ON links BEGIN
    SELECT RAISE(ABORT, 'immutable link identity');
END;

CREATE TRIGGER link_insert AFTER INSERT ON links BEGIN
    UPDATE links SET created_by = NULL, last_edited_by = NULL
        WHERE namespace = NEW.namespace AND relation = NEW.relation AND from_id = NEW.from_id AND to_id = NEW.to_id;
END;
