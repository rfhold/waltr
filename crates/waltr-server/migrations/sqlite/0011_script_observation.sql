ALTER TABLE script_invocations ADD COLUMN source_session TEXT CHECK(source_session IS NULL OR length(CAST(source_session AS BLOB)) BETWEEN 1 AND 256);
ALTER TABLE script_invocations ADD COLUMN public_definition TEXT CHECK(public_definition IS NULL OR length(CAST(public_definition AS BLOB)) <= 65536);
CREATE TABLE script_observation_head (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    cursor BIGINT NOT NULL CHECK(cursor >= 0),
    cursor_key BLOB NOT NULL CHECK(length(cursor_key)=32)
);
CREATE TABLE script_invocation_publications (
    invocation TEXT PRIMARY KEY REFERENCES script_invocations(id),
    publications TEXT NOT NULL CHECK(length(CAST(publications AS BLOB))<=65536)
);
INSERT INTO script_observation_head VALUES (1, 0,randomblob(32));
CREATE TABLE script_observations (
    cursor INTEGER PRIMARY KEY,
    principal TEXT NOT NULL,
    source_session TEXT,
    kind TEXT NOT NULL CHECK(kind IN ('invocation','operation')),
    entity TEXT NOT NULL,
    invocation TEXT NOT NULL REFERENCES script_invocations(id),
    admission_cursor BIGINT NOT NULL,
    record TEXT NOT NULL,
    status TEXT NOT NULL,
    updated_ms BIGINT NOT NULL,
    result TEXT,
    text_results TEXT NOT NULL,
    publications TEXT NOT NULL
);
CREATE INDEX script_observations_owner_cursor ON script_observations(principal,cursor);
CREATE INDEX script_observations_entity ON script_observations(principal,kind,entity,cursor);
CREATE INDEX script_invocations_owner_cursor ON script_invocations(json_extract(record,'$.caller.principal'),cursor);
CREATE VIEW script_invocation_observation AS
SELECT json_extract(i.record,'$.caller.principal') AS principal, i.source_session, 'invocation' AS kind,
       i.id AS entity, i.id AS invocation, i.cursor AS admission_cursor,
       i.record, i.status, i.updated_ms, i.result,
       (SELECT COALESCE(json_group_array(json(descriptor)), '[]') FROM
        (SELECT descriptor FROM script_text_results WHERE invocation=i.id ORDER BY name)) AS text_results,
       COALESCE((SELECT publications FROM script_invocation_publications WHERE invocation=i.id),'[]') AS publications
FROM script_invocations i;
CREATE VIEW script_operation_observation AS
SELECT json_extract(i.record,'$.caller.principal') AS principal, i.source_session, 'operation' AS kind,
       o.id AS entity, i.id AS invocation, o.cursor AS admission_cursor,
       o.record, o.status, o.updated_ms, o.result, '[]' AS text_results, '[]' AS publications
FROM script_operations o JOIN script_invocations i ON i.id=o.invocation;
INSERT INTO script_observations
SELECT ROW_NUMBER() OVER (ORDER BY kind,admission_cursor), * FROM (
    SELECT * FROM script_invocation_observation UNION ALL SELECT * FROM script_operation_observation
);
UPDATE script_observation_head SET cursor=(SELECT COALESCE(MAX(cursor),0) FROM script_observations);
CREATE TRIGGER script_observe_invocation_insert AFTER INSERT ON script_invocations BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_invocation_observation v WHERE v.entity=NEW.id;
END;
CREATE TRIGGER script_observe_invocation_update AFTER UPDATE ON script_invocations
WHEN OLD.status IS NOT NEW.status OR OLD.updated_ms IS NOT NEW.updated_ms OR OLD.result IS NOT NEW.result BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_invocation_observation v WHERE v.entity=NEW.id;
END;
CREATE TRIGGER script_observe_operation_insert AFTER INSERT ON script_operations BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_operation_observation v WHERE v.entity=NEW.id;
END;
CREATE TRIGGER script_observe_operation_update AFTER UPDATE ON script_operations
WHEN OLD.status IS NOT NEW.status OR OLD.updated_ms IS NOT NEW.updated_ms OR OLD.result IS NOT NEW.result BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_operation_observation v WHERE v.entity=NEW.id;
END;
CREATE TRIGGER script_observe_text_insert AFTER INSERT ON script_text_results BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_invocation_observation v WHERE v.entity=NEW.invocation;
END;
CREATE TRIGGER script_observe_publication_insert AFTER INSERT ON script_invocation_publications BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_invocation_observation v WHERE v.entity=NEW.invocation;
END;
CREATE TRIGGER script_observe_publication_update AFTER UPDATE ON script_invocation_publications BEGIN
    UPDATE script_observation_head SET cursor=cursor+1;
    INSERT INTO script_observations SELECT h.cursor,v.* FROM script_observation_head h,script_invocation_observation v WHERE v.entity=NEW.invocation;
END;
CREATE TRIGGER script_invocation_provenance_immutable BEFORE UPDATE ON script_invocations
WHEN OLD.source_session IS NOT NEW.source_session OR OLD.public_definition IS NOT NEW.public_definition BEGIN
    SELECT RAISE(ABORT,'immutable script admission provenance');
END;
