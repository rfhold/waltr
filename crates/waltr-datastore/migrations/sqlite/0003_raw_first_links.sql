DROP TRIGGER link_insert_guard;

CREATE TRIGGER link_insert_guard BEFORE INSERT ON links BEGIN
    -- Resolve the logical name from either canonical namespace partition.
    SELECT RAISE(ABORT, 'invalid logical relation rule') WHERE EXISTS (
        SELECT 1
        FROM definition_contracts AS d
        WHERE substr(NEW.namespace, 1, 11) = 'relational/'
          AND d.name = substr(NEW.namespace, 12, instr(substr(NEW.namespace, 12), '/') - 1)
          AND NOT coalesce((
            json_extract(d.definition, '$.type') = 'relational'
            AND CASE json_extract(d.definition, '$.access.scope')
                WHEN 'shared' THEN NEW.namespace = 'relational/' || d.name || '/shared'
                WHEN 'principal' THEN length(CAST(NEW.namespace AS BLOB)) = length(d.name) + 86
                    AND substr(NEW.namespace, 13 + length(d.name), 10) = 'principal-'
                    AND substr(NEW.namespace, -64) NOT GLOB '*[^0-9a-f]*'
                ELSE 0
            END
            AND EXISTS (
                SELECT 1 FROM json_each(d.definition, '$.relations') AS r
                WHERE r.key = NEW.relation AND
                    (json_extract(r.value, '$.from'), json_extract(r.value, '$.to'),
                     json_extract(r.value, '$.cardinality')) =
                    (NEW.from_collection, NEW.to_collection, NEW.cardinality)
            )
          ), 0)
    );

    -- Only a validated durable definition may create the first rule on behalf of a link writer.
    INSERT INTO relation_rules (namespace, relation, from_collection, to_collection, cardinality)
    SELECT NEW.namespace, NEW.relation, NEW.from_collection, NEW.to_collection, NEW.cardinality
    WHERE EXISTS (
        SELECT 1 FROM definition_contracts AS d
        WHERE substr(NEW.namespace, 1, 11) = 'relational/'
          AND d.name = substr(NEW.namespace, 12, instr(substr(NEW.namespace, 12), '/') - 1)
    ) AND NOT EXISTS (
        SELECT 1 FROM relation_rules WHERE namespace = NEW.namespace AND relation = NEW.relation
    );

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
