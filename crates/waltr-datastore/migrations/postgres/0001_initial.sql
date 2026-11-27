CREATE TABLE records (
    namespace TEXT COLLATE "C" NOT NULL,
    collection TEXT COLLATE "C" NOT NULL,
    id TEXT COLLATE "C" NOT NULL,
    fields JSONB NOT NULL CHECK (jsonb_typeof(fields) = 'object' AND octet_length(fields::TEXT) <= 131072),
    created_by TEXT,
    last_edited_by TEXT,
    PRIMARY KEY (namespace, collection, id)
);

CREATE TABLE relation_rules (
    namespace TEXT COLLATE "C" NOT NULL,
    relation TEXT COLLATE "C" NOT NULL,
    from_collection TEXT COLLATE "C" NOT NULL,
    to_collection TEXT COLLATE "C" NOT NULL,
    cardinality TEXT COLLATE "C" NOT NULL CHECK (cardinality IN ('one-to-one', 'one-to-many', 'many-to-many')),
    PRIMARY KEY (namespace, relation),
    UNIQUE (namespace, relation, from_collection, to_collection, cardinality)
);

CREATE TABLE links (
    namespace TEXT COLLATE "C" NOT NULL,
    relation TEXT COLLATE "C" NOT NULL,
    from_collection TEXT COLLATE "C" NOT NULL,
    from_id TEXT COLLATE "C" NOT NULL,
    to_collection TEXT COLLATE "C" NOT NULL,
    to_id TEXT COLLATE "C" NOT NULL,
    cardinality TEXT COLLATE "C" NOT NULL,
    created_by TEXT,
    last_edited_by TEXT,
    PRIMARY KEY (namespace, relation, from_id, to_id),
    FOREIGN KEY (namespace, relation, from_collection, to_collection, cardinality)
        REFERENCES relation_rules (namespace, relation, from_collection, to_collection, cardinality),
    FOREIGN KEY (namespace, from_collection, from_id) REFERENCES records ON DELETE CASCADE,
    FOREIGN KEY (namespace, to_collection, to_id) REFERENCES records ON DELETE CASCADE
);

CREATE UNIQUE INDEX links_from_unique ON links (namespace, relation, from_id)
    WHERE cardinality = 'one-to-one';
CREATE UNIQUE INDEX links_to_unique ON links (namespace, relation, to_id)
    WHERE cardinality IN ('one-to-one', 'one-to-many');
CREATE INDEX links_inbound ON links (namespace, relation, to_id, from_id);
CREATE INDEX links_from_record ON links (namespace, from_collection, from_id);
CREATE INDEX links_to_record ON links (namespace, to_collection, to_id);

CREATE TABLE commit_receipts (
    invocation TEXT COLLATE "C" PRIMARY KEY
);

CREATE TABLE field_indexes (
    namespace TEXT COLLATE "C" NOT NULL,
    collection TEXT COLLATE "C" NOT NULL,
    id TEXT COLLATE "C" NOT NULL,
    field TEXT COLLATE "C" NOT NULL,
    kind TEXT COLLATE "C" NOT NULL,
    text_value TEXT COLLATE "C",
    integer_value BIGINT,
    number_value DOUBLE PRECISION,
    boolean_value BIGINT,
    PRIMARY KEY (namespace, collection, id, field),
    FOREIGN KEY (namespace, collection, id) REFERENCES records ON DELETE CASCADE
);

CREATE INDEX field_text ON field_indexes (namespace, collection, field, text_value, id);
CREATE INDEX field_integer ON field_indexes (namespace, collection, field, integer_value, id);
CREATE INDEX field_number ON field_indexes (namespace, collection, field, number_value, id);
CREATE INDEX field_boolean ON field_indexes (namespace, collection, field, boolean_value, id);
CREATE INDEX field_kind ON field_indexes (namespace, collection, field, kind, id);

CREATE FUNCTION record_attribution() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.created_by := NULL;
    IF TG_OP = 'UPDATE' THEN
        NEW.created_by := OLD.created_by;
    END IF;
    NEW.last_edited_by := NULL;
    RETURN NEW;
END;
$$;

CREATE TRIGGER record_attribution BEFORE INSERT OR UPDATE OF fields ON records
    FOR EACH ROW EXECUTE FUNCTION record_attribution();

CREATE FUNCTION immutable_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'immutable datastore identity';
END;
$$;

CREATE TRIGGER record_keys BEFORE UPDATE OF namespace, collection, id ON records
    FOR EACH ROW WHEN ((OLD.namespace, OLD.collection, OLD.id) IS DISTINCT FROM (NEW.namespace, NEW.collection, NEW.id))
    EXECUTE FUNCTION immutable_identity();
CREATE TRIGGER rule_update BEFORE UPDATE ON relation_rules
    FOR EACH ROW EXECUTE FUNCTION immutable_identity();
CREATE TRIGGER link_keys BEFORE UPDATE OF namespace, relation, from_collection, from_id, to_collection, to_id, cardinality ON links
    FOR EACH ROW EXECUTE FUNCTION immutable_identity();

CREATE FUNCTION link_attribution() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.created_by := NULL;
    NEW.last_edited_by := NULL;
    RETURN NEW;
END;
$$;

CREATE TRIGGER link_attribution BEFORE INSERT ON links
    FOR EACH ROW EXECUTE FUNCTION link_attribution();

CREATE FUNCTION project_record() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = pg_catalog, waltr_datastore, pg_temp AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        DELETE FROM field_indexes WHERE namespace = OLD.namespace AND collection = OLD.collection AND id = OLD.id;
    END IF;
    INSERT INTO field_indexes
        SELECT NEW.namespace, NEW.collection, NEW.id, f.key, f.kind,
            CASE WHEN f.kind = 'string' AND octet_length(f.scalar) <= 1024 THEN f.scalar END,
            CASE WHEN f.kind = 'number' THEN CASE WHEN f.scalar ~ '^-?(0|[1-9][0-9]*)$'
                      AND f.scalar::NUMERIC BETWEEN '-9223372036854775808'::NUMERIC AND '9223372036854775807'::NUMERIC
                 THEN f.scalar::BIGINT END END,
            CASE WHEN f.kind = 'number' THEN CASE
                WHEN abs(f.scalar::NUMERIC) BETWEEN '2.4703282292062328e-324'::NUMERIC AND '1.7976931348623157e308'::NUMERIC
                    THEN f.scalar::DOUBLE PRECISION
                WHEN abs(f.scalar::NUMERIC) < '2.4703282292062328e-324'::NUMERIC THEN 0::DOUBLE PRECISION
            END END,
            CASE WHEN f.kind = 'boolean' THEN CASE WHEN f.scalar = 'true' THEN 1 ELSE 0 END END
        FROM jsonb_each(NEW.fields) AS j
        CROSS JOIN LATERAL (SELECT j.key, jsonb_typeof(j.value) AS kind, j.value #>> '{}' AS scalar) AS f
        WHERE octet_length(f.key) <= 128;
    RETURN NULL;
END;
$$;

CREATE TRIGGER project_record AFTER INSERT OR UPDATE OF fields ON records
    FOR EACH ROW EXECUTE FUNCTION project_record();
