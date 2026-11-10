-- Keep the closed event set aligned with template_events::EVENTS; the migration
-- test checks equality. The outbox has no AUTOINCREMENT sequence or triggers.
CREATE TABLE template_event_outbox_new (
    flow_id TEXT NOT NULL REFERENCES template_instances(flow_id),
    seq INTEGER NOT NULL CHECK (seq > 0),
    event_name TEXT NOT NULL CHECK (event_name IN ('flow_instance_created','flow_instance_extended','flow_instance_ended','flow_instance_expired','flow_instance_rescheduled')),
    body TEXT NOT NULL CHECK (json_valid(body)),
    published_at INTEGER,
    retry_at INTEGER NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (flow_id, seq)
);
INSERT INTO template_event_outbox_new
    (flow_id, seq, event_name, body, published_at, retry_at, attempts)
    SELECT flow_id, seq, event_name, body, published_at, retry_at, attempts
    FROM template_event_outbox;
DROP TABLE template_event_outbox;
ALTER TABLE template_event_outbox_new RENAME TO template_event_outbox;
CREATE INDEX template_event_pending ON template_event_outbox(retry_at) WHERE published_at IS NULL;
