CREATE TABLE attribution_facts (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    fact_id TEXT NOT NULL UNIQUE,
    body TEXT NOT NULL CHECK (
        json_valid(body) AND json_type(body)='object'
        AND length(CAST(body AS BLOB))<4096
        AND json_extract(body,'$.fact_id') IS fact_id
        AND json_type(body,'$.workload') IS 'text'
        AND length(CAST(json_extract(body,'$.workload') AS BLOB)) BETWEEN 1 AND 64
        AND json_extract(body,'$.workload') NOT GLOB '*[^a-z0-9_-]*'
        AND json_type(body,'$.task') IS 'text'
        AND length(CAST(json_extract(body,'$.task') AS BLOB)) BETWEEN 1 AND 128
        AND json_extract(body,'$.task') NOT GLOB '*[^a-z0-9_.-]*'
        AND json_type(body,'$.effective_from_ms') IS 'integer'
        AND json_type(body,'$.recorded_at_ms') IS 'integer'
        AND json_extract(body,'$.recorded_at_ms') IS recorded_at
    ),
    recorded_at INTEGER NOT NULL
);
CREATE TRIGGER attribution_facts_no_update BEFORE UPDATE ON attribution_facts
BEGIN SELECT RAISE(ABORT,'attribution facts are permanent'); END;
CREATE TRIGGER attribution_facts_no_delete BEFORE DELETE ON attribution_facts
BEGIN SELECT RAISE(ABORT,'attribution facts are permanent'); END;
CREATE TRIGGER attribution_facts_outbox AFTER INSERT ON attribution_facts BEGIN
    INSERT INTO outbox(at,kind,flow_id,body,bus_retry_at)
    VALUES(NEW.recorded_at,'attribution.fact',
        COALESCE(json_extract(NEW.body,'$.subject.flow_id'),json_extract(NEW.body,'$.parent.flow_id'),''),
        NEW.body,NEW.recorded_at);
END;

-- Use only tables present both before and after the run-log cutover. Broca
-- snapshots precede every send and survive until the corresponding run is
-- pruned; cost envelopes also retain sessions after call payloads disappear.
WITH
stamp(ms) AS (SELECT CAST(strftime('%s','now') AS INTEGER)*1000),
owned AS (
    SELECT f.*,
        CASE WHEN f.package IS NOT NULL THEN f.created_at
            ELSE COALESCE(f.first_approved_at,(SELECT MIN(i.approved_at) FROM installs i WHERE i.flow_id=f.flow_id)) END AS effective,
        CASE WHEN f.package IS NOT NULL THEN 'basal:flow:'||f.flow_id||':created'
            ELSE 'basal:flow:'||f.flow_id||':v'||f.approved_version END AS fact_id,
        CASE WHEN f.owner LIKE 'agent\_%' ESCAPE '\' THEN f.owner END AS alfonso,
        'basal.'||COALESCE(f.package,f.flow_id) AS task
    FROM flows f WHERE f.owner IS NOT NULL
),
flow_sessions(session,run_id,position,flow_id,at) AS (
    SELECT json_extract(b.snapshot,'$.route.session'),b.run_id,b.position,
        COALESCE(json_extract(b.snapshot,'$.route.flow_id'),
            substr(json_extract(b.snapshot,'$.route.session'),12,
                instr(substr(json_extract(b.snapshot,'$.route.session'),12),':')-1)),
        COALESCE((SELECT MIN(c.created_at) FROM classify_costs c WHERE c.kind='flow' AND c.run_id=b.run_id AND c.position=b.position),
            (SELECT MIN(t.reserved_at) FROM token_ledger t WHERE t.run_id=b.run_id AND t.position=b.position))
    FROM broca_calls b
    UNION ALL
    SELECT json_extract(args,'$.session'),run_id,position,flow_id,created_at FROM classify_costs
    WHERE kind='flow' AND json_valid(args) AND json_type(args,'$.session')='text'
),
program_sessions(session,run_id,position) AS (
    SELECT json_extract(snapshot,'$.route.session'),run_id,position FROM codemode_broca_calls
    UNION
    SELECT json_extract(envelope,'$.session'),run_id,position FROM codemode_models
    WHERE envelope IS NOT NULL AND json_valid(envelope) AND json_type(envelope,'$.session')='text'
    UNION
    SELECT json_extract(args,'$.session'),run_id,position FROM classify_costs
    WHERE kind='program' AND json_valid(args) AND json_type(args,'$.session')='text'
),
candidates(kind_order,fact_id,subject,alfonso,parent,workload,task,effective) AS (
    SELECT 0,fact_id,json_object('kind','flow','flow_id',flow_id),alfonso,NULL,'flow',task,effective FROM owned
    UNION ALL
    SELECT 1,'basal:session:'||s.session,json_object('kind','session','runner','broca','harness','broca','session',s.session),
        f.alfonso,json_object('kind','flow','flow_id',f.flow_id),'flow',f.task,COALESCE(MIN(s.at),f.effective)
    FROM flow_sessions s JOIN owned f USING(flow_id) WHERE s.session IS NOT NULL GROUP BY s.session
    UNION ALL
    -- Retention can leave a cost envelope after deleting the run's owner and
    -- bind. Those orphaned program sessions deliberately remain unattributed.
    SELECT 1,'basal:session:'||s.session,json_object('kind','session','runner','broca','harness','broca','session',s.session),
        CASE WHEN COALESCE(r.on_behalf_of_agent,r.agent_id) LIKE 'agent\_%' ESCAPE '\'
            THEN COALESCE(r.on_behalf_of_agent,r.agent_id) END,
        CASE WHEN length(json_extract(r.bind_identity,'$.runner'))>0 AND length(json_extract(r.bind_identity,'$.harness'))>0 AND length(json_extract(r.bind_identity,'$.session'))>0
            THEN json_object('kind','session','runner',json_extract(r.bind_identity,'$.runner'),'harness',json_extract(r.bind_identity,'$.harness'),'session',json_extract(r.bind_identity,'$.session')) END,
        'codemode','basal.codemode',
        COALESCE((SELECT c.intent_at FROM codemode_calls c WHERE c.run_id=s.run_id AND c.position=s.position),
            (SELECT c.created_at FROM classify_costs c WHERE c.kind='program' AND c.run_id=s.run_id AND c.position=s.position),r.admitted_at)
    FROM program_sessions s JOIN codemode_runs r USING(run_id)
    WHERE s.session IS NOT NULL AND COALESCE(r.on_behalf_of_agent,r.agent_id)<>''
)
INSERT INTO attribution_facts(fact_id,body,recorded_at)
SELECT fact_id,json_object('fact_id',fact_id,'supersedes',NULL,'subject',json(subject),'alfonso',alfonso,
    'parent',json(parent),'workload',workload,'task',task,'project_id',NULL,
    'effective_from_ms',effective,'recorded_at_ms',stamp.ms),stamp.ms
FROM candidates CROSS JOIN stamp WHERE true ORDER BY kind_order,fact_id
ON CONFLICT(fact_id) DO NOTHING;

DROP INDEX outbox_bus_pending;
CREATE INDEX outbox_bus_pending ON outbox(bus_retry_at,seq)
WHERE bus_published_at IS NULL AND kind IN ('flow.owner_notice','codemode.late_result_ready','attribution.fact');
