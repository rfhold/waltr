ALTER TABLE waltr.script_invocations ADD COLUMN source_session TEXT CHECK(source_session IS NULL OR octet_length(source_session) BETWEEN 1 AND 256);
ALTER TABLE waltr.script_invocations ADD COLUMN public_definition TEXT CHECK(public_definition IS NULL OR octet_length(public_definition) <= 65536);
CREATE TABLE waltr.script_observation_head (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    cursor BIGINT NOT NULL CHECK(cursor >= 0),
    cursor_key BYTEA NOT NULL CHECK(octet_length(cursor_key)=32)
);
CREATE TABLE waltr.script_invocation_publications (
    invocation TEXT PRIMARY KEY REFERENCES waltr.script_invocations(id),
    publications TEXT NOT NULL CHECK(octet_length(publications)<=65536)
);
INSERT INTO waltr.script_observation_head VALUES (1,0,decode(replace(gen_random_uuid()::text || gen_random_uuid()::text,'-',''),'hex'));
CREATE TABLE waltr.script_observations (
    cursor BIGINT PRIMARY KEY,
    principal TEXT NOT NULL,
    source_session TEXT,
    kind TEXT NOT NULL CHECK(kind IN ('invocation','operation')),
    entity TEXT NOT NULL,
    invocation TEXT NOT NULL REFERENCES waltr.script_invocations(id),
    admission_cursor BIGINT NOT NULL,
    record TEXT NOT NULL,
    status TEXT NOT NULL,
    updated_ms BIGINT NOT NULL,
    result TEXT,
    text_results TEXT NOT NULL,
    publications TEXT NOT NULL
);
CREATE INDEX script_observations_owner_cursor ON waltr.script_observations(principal,cursor);
CREATE INDEX script_observations_entity ON waltr.script_observations(principal,kind,entity,cursor);
CREATE INDEX script_invocations_owner_cursor ON waltr.script_invocations(((record::jsonb)->'caller'->>'principal'),cursor);
CREATE VIEW waltr.script_invocation_observation AS
SELECT i.record::jsonb->'caller'->>'principal' AS principal,i.source_session, 'invocation'::text AS kind,
       i.id AS entity, i.id AS invocation, i.cursor AS admission_cursor,
       i.record,i.status,i.updated_ms,i.result,
       (SELECT COALESCE(json_agg(descriptor::json ORDER BY name)::text,'[]') FROM waltr.script_text_results WHERE invocation=i.id) AS text_results,
       COALESCE((SELECT publications FROM waltr.script_invocation_publications WHERE invocation=i.id),'[]') AS publications
FROM waltr.script_invocations i;
CREATE VIEW waltr.script_operation_observation AS
SELECT i.record::jsonb->'caller'->>'principal' AS principal,i.source_session, 'operation'::text AS kind,
       o.id AS entity,i.id AS invocation,o.cursor AS admission_cursor,
       o.record,o.status,o.updated_ms,o.result,'[]'::text AS text_results,'[]'::text AS publications
FROM waltr.script_operations o JOIN waltr.script_invocations i ON i.id=o.invocation;
INSERT INTO waltr.script_observations
SELECT ROW_NUMBER() OVER (ORDER BY kind,admission_cursor),v.* FROM (
    SELECT * FROM waltr.script_invocation_observation UNION ALL SELECT * FROM waltr.script_operation_observation
) v;
UPDATE waltr.script_observation_head SET cursor=(SELECT COALESCE(MAX(cursor),0) FROM waltr.script_observations);
CREATE FUNCTION waltr.script_observation_lock() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM cursor FROM waltr.script_observation_head WHERE singleton=1 FOR UPDATE;
    RETURN NULL;
END;
$$;
CREATE TRIGGER script_observation_lock_invocation BEFORE INSERT OR UPDATE ON waltr.script_invocations FOR EACH STATEMENT EXECUTE FUNCTION waltr.script_observation_lock();
CREATE TRIGGER script_observation_lock_operation BEFORE INSERT OR UPDATE ON waltr.script_operations FOR EACH STATEMENT EXECUTE FUNCTION waltr.script_observation_lock();
CREATE TRIGGER script_observation_lock_text BEFORE INSERT ON waltr.script_text_results FOR EACH STATEMENT EXECUTE FUNCTION waltr.script_observation_lock();
CREATE TRIGGER script_observation_lock_publication BEFORE INSERT OR UPDATE ON waltr.script_invocation_publications FOR EACH STATEMENT EXECUTE FUNCTION waltr.script_observation_lock();
CREATE FUNCTION waltr.script_observe_row() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE next_cursor BIGINT; selected_id TEXT;
BEGIN
    IF TG_OP='UPDATE' AND TG_TABLE_NAME IN ('script_invocations','script_operations') THEN
        IF OLD.status IS NOT DISTINCT FROM NEW.status AND OLD.updated_ms IS NOT DISTINCT FROM NEW.updated_ms AND OLD.result IS NOT DISTINCT FROM NEW.result THEN
            RETURN NEW;
        END IF;
    END IF;
    UPDATE waltr.script_observation_head SET cursor=cursor+1 WHERE singleton=1 RETURNING cursor INTO next_cursor;
    IF TG_TABLE_NAME='script_operations' THEN
        INSERT INTO waltr.script_observations SELECT next_cursor,v.* FROM waltr.script_operation_observation v WHERE v.entity=NEW.id;
    ELSE
        IF TG_TABLE_NAME IN ('script_text_results','script_invocation_publications') THEN selected_id=NEW.invocation; ELSE selected_id=NEW.id; END IF;
        INSERT INTO waltr.script_observations SELECT next_cursor,v.* FROM waltr.script_invocation_observation v WHERE v.entity=selected_id;
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER script_observe_invocation AFTER INSERT OR UPDATE ON waltr.script_invocations FOR EACH ROW EXECUTE FUNCTION waltr.script_observe_row();
CREATE TRIGGER script_observe_operation AFTER INSERT OR UPDATE ON waltr.script_operations FOR EACH ROW EXECUTE FUNCTION waltr.script_observe_row();
CREATE TRIGGER script_observe_text AFTER INSERT ON waltr.script_text_results FOR EACH ROW EXECUTE FUNCTION waltr.script_observe_row();
CREATE TRIGGER script_observe_publication AFTER INSERT OR UPDATE ON waltr.script_invocation_publications FOR EACH ROW EXECUTE FUNCTION waltr.script_observe_row();
CREATE FUNCTION waltr.script_provenance_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.source_session IS DISTINCT FROM NEW.source_session OR OLD.public_definition IS DISTINCT FROM NEW.public_definition THEN
        RAISE EXCEPTION 'immutable script admission provenance';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER script_invocation_provenance_immutable BEFORE UPDATE ON waltr.script_invocations FOR EACH ROW EXECUTE FUNCTION waltr.script_provenance_immutable();
