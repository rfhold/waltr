CREATE FUNCTION prepare_link_rule() RETURNS trigger LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = pg_catalog, waltr_datastore, pg_temp AS $$
DECLARE
    store_name TEXT := split_part(NEW.namespace, '/', 2);
    partition TEXT := split_part(NEW.namespace, '/', 3);
    contract JSONB;
    rule JSONB;
BEGIN
    IF split_part(NEW.namespace, '/', 1) = 'relational' THEN
        SELECT definition INTO contract FROM definition_contracts WHERE name = store_name;
    END IF;
    IF contract IS NOT NULL THEN
        rule := contract -> 'relations' -> NEW.relation;
        IF NOT coalesce((
            contract ->> 'type' = 'relational'
            AND NEW.namespace = 'relational/' || store_name || '/' || partition
            AND CASE contract #>> '{access,scope}'
                WHEN 'shared' THEN partition = 'shared'
                WHEN 'principal' THEN octet_length(partition) = 74
                    AND partition COLLATE "C" ~ '^principal-[0-9a-f]{64}$'
                ELSE false
            END
            AND (rule ->> 'from', rule ->> 'to', rule ->> 'cardinality') =
                (NEW.from_collection, NEW.to_collection, NEW.cardinality)
        ), false) THEN
            RAISE EXCEPTION 'invalid logical relation rule' USING ERRCODE = '23514';
        END IF;

        INSERT INTO relation_rules (namespace, relation, from_collection, to_collection, cardinality)
        SELECT NEW.namespace, NEW.relation, NEW.from_collection, NEW.to_collection, NEW.cardinality
        WHERE NOT EXISTS (
            SELECT 1 FROM relation_rules WHERE namespace = NEW.namespace AND relation = NEW.relation
        )
        ON CONFLICT (namespace, relation) DO NOTHING;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER prepare_link_rule BEFORE INSERT ON links
    FOR EACH ROW EXECUTE FUNCTION prepare_link_rule();
