-- Keep immutable code once per admitted identity. Version zero names legacy
-- runs admitted before versioned installs existed; real versions start at one.
CREATE TABLE run_snapshots (
    snapshot_id INTEGER PRIMARY KEY,
    flow_id TEXT NOT NULL,
    flow_version INTEGER NOT NULL,
    code_hash BLOB NOT NULL CHECK (length(code_hash)=32),
    script TEXT NOT NULL,
    manifest TEXT NOT NULL,
    UNIQUE(flow_id,flow_version,code_hash)
);
INSERT INTO run_snapshots(flow_id,flow_version,code_hash,script,manifest)
    SELECT flow_id,COALESCE(flow_version,0),code_hash,script,manifest FROM runs
    WHERE rowid IN (SELECT MIN(rowid) FROM runs GROUP BY flow_id,COALESCE(flow_version,0),code_hash);
-- Refuse inconsistent identities rather than silently losing an old snapshot.
CREATE TEMP TABLE snapshot_identity_check(ok INTEGER CHECK(ok=1));
INSERT INTO snapshot_identity_check SELECT NOT EXISTS (
    SELECT 1 FROM runs r JOIN run_snapshots s ON s.flow_id=r.flow_id
      AND s.flow_version=COALESCE(r.flow_version,0) AND s.code_hash=r.code_hash
    WHERE s.script<>r.script OR s.manifest<>r.manifest);
DROP TABLE snapshot_identity_check;

CREATE TABLE runs_new (
    run_id TEXT PRIMARY KEY, flow_id TEXT NOT NULL, trigger_id TEXT NOT NULL,
    attempt INTEGER NOT NULL, trigger TEXT NOT NULL,
    snapshot_id INTEGER NOT NULL REFERENCES run_snapshots(snapshot_id),
    code_hash BLOB NOT NULL, fingerprint TEXT,
    state TEXT NOT NULL CHECK(state IN ('pending','running','suspended','succeeded','failed','needs_reconcile','engine_mismatch','cancelled')),
    owner TEXT, generation INTEGER NOT NULL DEFAULT 0, readiness INTEGER NOT NULL DEFAULT 0,
    awaited TEXT, result TEXT, error_kind TEXT, error_detail TEXT,
    broken INTEGER NOT NULL DEFAULT 0, admitted_at INTEGER NOT NULL, ended_at INTEGER,
    flow_version INTEGER, admit_seq INTEGER, deadline_ms INTEGER, deadline_at INTEGER,
    self_input TEXT CHECK(self_input IS NULL OR json_valid(self_input)),
    params_input TEXT CHECK(params_input IS NULL OR json_valid(params_input)),
    grant_guards TEXT CHECK(grant_guards IS NULL OR json_valid(grant_guards)),
    template_incarnation INTEGER,
    end_instance INTEGER NOT NULL DEFAULT 0 CHECK(end_instance IN (0,1)),
    script_started INTEGER NOT NULL DEFAULT 0,
    payloads_pruned_at INTEGER,
    CHECK((state='running')=(owner IS NOT NULL))
);
INSERT INTO runs_new SELECT r.run_id,r.flow_id,r.trigger_id,r.attempt,r.trigger,s.snapshot_id,
    r.code_hash,r.fingerprint,r.state,r.owner,r.generation,r.readiness,r.awaited,r.result,
    r.error_kind,r.error_detail,r.broken,r.admitted_at,r.ended_at,r.flow_version,r.admit_seq,
    r.deadline_ms,r.deadline_at,r.self_input,r.params_input,r.grant_guards,
    r.template_incarnation,r.end_instance,r.script_started,NULL
    FROM runs r JOIN run_snapshots s ON s.flow_id=r.flow_id
      AND s.flow_version=COALESCE(r.flow_version,0) AND s.code_hash=r.code_hash;

CREATE TABLE journal_new (
    run_id TEXT NOT NULL REFERENCES runs_new(run_id), position INTEGER NOT NULL CHECK(position>=0),
    kind_code INTEGER NOT NULL, module TEXT, op TEXT,
    args_digest BLOB NOT NULL CHECK(length(args_digest)=32), idempotency_key TEXT NOT NULL UNIQUE,
    class TEXT NOT NULL CHECK(class IN ('sync','local','query','mutation','keyed_mutation')),
    dispatch TEXT NOT NULL CHECK(dispatch IN ('none','sent','accepted','unknown','not_applied','deferred')),
    attempts INTEGER NOT NULL DEFAULT 0, handle TEXT,
    settlement TEXT CHECK(settlement IN ('fulfilled','rejected')), payload_hash BLOB,
    delivery_order INTEGER CHECK(delivery_order>=0), clock_ms REAL, issued_generation INTEGER NOT NULL,
    unknown_reason TEXT CHECK(unknown_reason IN ('basal_restarted','connection_lost','reply_timeout','reply_unreadable','retries_exhausted','provider_lost_run'))
        CHECK(unknown_reason IS NOT NULL OR dispatch<>'unknown'),
    refusal TEXT CHECK(refusal IN ('scope_not_carrier','scope_ended','scope_not_live','scope_epoch_required','scope_not_synced','scope_changed','scope_unsupported','resource_busy','consent_unavailable','module_grant_absent','agent_grant_absent','sink_board_target_unavailable','sink_audience_unavailable','sink_store_failed','secret_vault_unavailable')),
    retry_not_before INTEGER, refusal_detail TEXT,
    targeted INTEGER NOT NULL DEFAULT 0 CHECK(targeted IN (0,1) AND (targeted=0 OR kind_code=0)),
    scope_cut INTEGER NOT NULL DEFAULT 0 CHECK(scope_cut IN (0,1)),
    args_bytes INTEGER NOT NULL CHECK(args_bytes>=0),
    request_bytes INTEGER NOT NULL DEFAULT 0 CHECK(request_bytes>=0),
    value_bytes INTEGER CHECK(value_bytes>=0),
    PRIMARY KEY(run_id,position),
    CHECK((kind_code=0)=(module IS NOT NULL AND op IS NOT NULL)),
    -- Settlement retains its original size, hash and release order even after
    -- payload deletion. NULL size means no outcome has been released.
    CHECK((settlement IS NULL)=(value_bytes IS NULL)),
    CHECK((settlement IS NULL)=(payload_hash IS NULL)),
    CHECK((settlement IS NULL)=(delivery_order IS NULL)),
    CHECK((dispatch='deferred')=(refusal IS NOT NULL)),
    CHECK((dispatch='deferred')=(retry_not_before IS NOT NULL)),
    CHECK(retry_not_before IS NULL OR typeof(retry_not_before)='integer'),
    CHECK((dispatch='deferred')=(refusal_detail IS NOT NULL)),
    CHECK(dispatch<>'deferred' OR (settlement IS NULL AND json_valid(refusal_detail)
        AND json_type(refusal_detail)='object' AND json_extract(refusal_detail,'$.reason') IS refusal
        AND json_type(refusal_detail,'$.provider') IS 'text' AND json_type(refusal_detail,'$.action') IS 'text'))
);
INSERT INTO journal_new SELECT run_id,position,kind_code,module,op,args_digest,idempotency_key,
    class,dispatch,attempts,handle,settlement,payload_hash,delivery_order,clock_ms,issued_generation,
    unknown_reason,refusal,retry_not_before,refusal_detail,targeted,scope_cut,
    length(CAST(args AS BLOB)),COALESCE(length(CAST(request AS BLOB)),0),length(CAST(value AS BLOB)) FROM journal;
CREATE TABLE journal_payloads (
    run_id TEXT NOT NULL, position INTEGER NOT NULL,
    args TEXT NOT NULL, request TEXT, value TEXT,
    PRIMARY KEY(run_id,position),
    FOREIGN KEY(run_id,position) REFERENCES journal_new(run_id,position) ON DELETE CASCADE
);
INSERT INTO journal_payloads SELECT run_id,position,args,request,value FROM journal;

-- Move children before dropping parents: foreign_keys stays enabled throughout
-- the migration transaction. Journal payloads copy directly to their final table.
CREATE TABLE migration_47_mailbox AS SELECT * FROM mailbox;
CREATE TABLE migration_47_mailbox_sequence AS SELECT seq FROM sqlite_sequence WHERE name='mailbox';
CREATE TABLE migration_47_broca AS SELECT * FROM broca_calls;
DROP TABLE mailbox;
DROP TABLE broca_calls;
CREATE TABLE event_body_journal_new (
    run_id TEXT PRIMARY KEY REFERENCES runs_new(run_id) ON DELETE CASCADE,
    body BLOB, digest TEXT, attempt INTEGER NOT NULL DEFAULT 0, retry_at INTEGER,
    last_provider_refusal TEXT
);
INSERT INTO event_body_journal_new SELECT * FROM event_body_journal;
DROP TABLE event_body_journal;
DROP TABLE journal;
DROP TABLE runs;
ALTER TABLE runs_new RENAME TO runs;
ALTER TABLE journal_new RENAME TO journal;
ALTER TABLE event_body_journal_new RENAME TO event_body_journal;

CREATE INDEX runs_state ON runs(state);
CREATE INDEX runs_flow_order ON runs(flow_id,admit_seq);
CREATE INDEX runs_state_deadline ON runs(state,deadline_at) WHERE deadline_at IS NOT NULL;
CREATE INDEX runs_retention ON runs(state,ended_at,run_id) WHERE state IN ('succeeded','failed','engine_mismatch','cancelled');
CREATE INDEX runs_flow_finished ON runs(flow_id, admit_seq DESC, run_id DESC) WHERE state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled');
CREATE INDEX runs_flow_state_admission ON runs(flow_id, state, admitted_at, admit_seq);
CREATE INDEX runs_slot_holders ON runs(flow_id, admit_seq, state, run_id) WHERE state IN ('pending', 'running', 'suspended', 'needs_reconcile');
CREATE INDEX runs_snapshot ON runs(snapshot_id);
CREATE INDEX runs_payload_retention ON runs(state,ended_at,run_id) WHERE state='succeeded' AND payloads_pruned_at IS NULL;
CREATE UNIQUE INDEX journal_delivery_order ON journal(run_id,delivery_order) WHERE delivery_order IS NOT NULL;
CREATE INDEX journal_deferred ON journal(run_id,position) WHERE dispatch='deferred';
CREATE INDEX journal_deferred_retry ON journal(retry_not_before,run_id) WHERE dispatch='deferred' AND retry_not_before IS NOT NULL;
CREATE TABLE mailbox (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL, position INTEGER NOT NULL, handle TEXT,
    settlement TEXT NOT NULL CHECK(settlement IN ('fulfilled','rejected')), value TEXT NOT NULL, payload_hash BLOB NOT NULL,
    source TEXT NOT NULL CHECK(source IN ('host','completion','local','reconcile')), arrived_generation INTEGER NOT NULL,
    UNIQUE(run_id,position), FOREIGN KEY(run_id,position) REFERENCES journal(run_id,position)
);
INSERT INTO mailbox SELECT * FROM migration_47_mailbox;
UPDATE sqlite_sequence SET seq=MAX(seq,COALESCE((SELECT MAX(seq) FROM migration_47_mailbox_sequence),seq)) WHERE name='mailbox';
DROP TABLE migration_47_mailbox;
DROP TABLE migration_47_mailbox_sequence;
CREATE TABLE broca_calls (
    send_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, position INTEGER NOT NULL CHECK(position>=0), snapshot TEXT NOT NULL,
    UNIQUE(run_id,position), FOREIGN KEY(run_id,position) REFERENCES journal(run_id,position) ON DELETE CASCADE
);
INSERT INTO broca_calls SELECT * FROM migration_47_broca;
DROP TABLE migration_47_broca;
CREATE INDEX broca_calls_pending ON broca_calls(send_id) WHERE json_extract(snapshot,'$.acknowledged')=0 AND COALESCE(json_extract(snapshot,'$.deferred'),0)=0;
CREATE TRIGGER broca_calls_forget_wait AFTER DELETE ON broca_calls BEGIN
    DELETE FROM broca_waits WHERE send_id=old.send_id;
    DELETE FROM broca_wakes WHERE send_id=old.send_id;
END;

CREATE VIEW journal_with_payloads AS SELECT j.*,p.args,p.request,p.value,
    r.payloads_pruned_at IS NOT NULL AS payload_pruned
    FROM journal j JOIN runs r USING(run_id) LEFT JOIN journal_payloads p USING(run_id,position);
CREATE VIEW runs_with_snapshots AS SELECT r.*,s.script,s.manifest
    FROM runs r JOIN run_snapshots s USING(snapshot_id);
PRAGMA foreign_key_check;
CREATE TEMP TABLE migration_47_foreign_keys(ok INTEGER CHECK(ok=1));
INSERT INTO migration_47_foreign_keys SELECT NOT EXISTS(SELECT 1 FROM pragma_foreign_key_check);
DROP TABLE migration_47_foreign_keys;
