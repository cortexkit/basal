-- Rebuild the closed journal refusal set without losing pending answers or
-- Broca completion ownership. Failed cleanup remains durable but is not due.
CREATE TABLE migration_45_mailbox AS SELECT * FROM mailbox;
-- Deleted answers still reserve their AUTOINCREMENT ids.
CREATE TABLE migration_45_mailbox_sequence AS SELECT seq FROM sqlite_sequence WHERE name='mailbox';
CREATE TABLE migration_45_broca AS SELECT * FROM broca_calls;
DROP TABLE mailbox;
DROP TABLE broca_calls;
CREATE TABLE journal_new (
    run_id TEXT NOT NULL REFERENCES runs(run_id), position INTEGER NOT NULL CHECK (position>=0),
    kind_code INTEGER NOT NULL, module TEXT, op TEXT, args TEXT NOT NULL,
    args_digest BLOB NOT NULL CHECK (length(args_digest)=32), idempotency_key TEXT NOT NULL UNIQUE,
    class TEXT NOT NULL CHECK (class IN ('sync','local','query','mutation','keyed_mutation')),
    dispatch TEXT NOT NULL CHECK (dispatch IN ('none','sent','accepted','unknown','not_applied','deferred')),
    attempts INTEGER NOT NULL DEFAULT 0, handle TEXT,
    settlement TEXT CHECK (settlement IN ('fulfilled','rejected')), value TEXT, payload_hash BLOB,
    delivery_order INTEGER CHECK (delivery_order>=0), clock_ms REAL, issued_generation INTEGER NOT NULL, request TEXT,
    unknown_reason TEXT CHECK (unknown_reason IN ('basal_restarted','connection_lost','reply_timeout','reply_unreadable','retries_exhausted','provider_lost_run'))
    CHECK (unknown_reason IS NOT NULL OR dispatch <> 'unknown'),
    refusal TEXT CHECK (refusal IN ('scope_not_carrier','scope_ended','scope_not_live','scope_epoch_required','scope_not_synced','scope_changed','scope_unsupported','resource_busy','consent_unavailable','module_grant_absent','agent_grant_absent','sink_board_target_unavailable','sink_audience_unavailable','sink_store_failed','secret_vault_unavailable')),
    retry_not_before INTEGER, refusal_detail TEXT,
    targeted INTEGER NOT NULL DEFAULT 0 CHECK (targeted IN (0,1) AND (targeted=0 OR kind_code=0)),
    scope_cut INTEGER NOT NULL DEFAULT 0 CHECK (scope_cut IN (0,1)),
    PRIMARY KEY (run_id,position),
    CHECK ((kind_code=0)=(module IS NOT NULL AND op IS NOT NULL)),
    CHECK ((settlement IS NULL)=(value IS NULL)), CHECK ((settlement IS NULL)=(payload_hash IS NULL)),
    CHECK ((settlement IS NULL)=(delivery_order IS NULL)),
    CHECK ((dispatch='deferred')=(refusal IS NOT NULL)), CHECK ((dispatch='deferred')=(retry_not_before IS NOT NULL)),
    CHECK (retry_not_before IS NULL OR typeof(retry_not_before)='integer'),
    CHECK ((dispatch='deferred')=(refusal_detail IS NOT NULL)),
    CHECK (dispatch<>'deferred' OR (settlement IS NULL AND json_valid(refusal_detail)
        AND json_type(refusal_detail)='object' AND json_extract(refusal_detail,'$.reason') IS refusal
        AND json_type(refusal_detail,'$.provider') IS 'text' AND json_type(refusal_detail,'$.action') IS 'text'))
);
INSERT INTO journal_new SELECT * FROM journal;
DROP TABLE journal;
ALTER TABLE journal_new RENAME TO journal;
CREATE UNIQUE INDEX journal_delivery_order ON journal(run_id,delivery_order) WHERE delivery_order IS NOT NULL;
CREATE INDEX journal_deferred ON journal(run_id,position) WHERE dispatch='deferred';
CREATE INDEX journal_deferred_retry ON journal(retry_not_before,run_id) WHERE dispatch='deferred' AND retry_not_before IS NOT NULL;
CREATE TABLE mailbox (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL, position INTEGER NOT NULL, handle TEXT,
    settlement TEXT NOT NULL CHECK (settlement IN ('fulfilled','rejected')), value TEXT NOT NULL, payload_hash BLOB NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('host','completion','local','reconcile')), arrived_generation INTEGER NOT NULL,
    UNIQUE (run_id,position), FOREIGN KEY (run_id,position) REFERENCES journal(run_id,position)
);
INSERT INTO mailbox SELECT * FROM migration_45_mailbox;
UPDATE sqlite_sequence SET seq=MAX(seq,COALESCE((SELECT MAX(seq) FROM migration_45_mailbox_sequence),seq)) WHERE name='mailbox';
DROP TABLE migration_45_mailbox;
DROP TABLE migration_45_mailbox_sequence;
CREATE TABLE broca_calls (
    send_id TEXT PRIMARY KEY, run_id TEXT NOT NULL, position INTEGER NOT NULL CHECK (position>=0), snapshot TEXT NOT NULL,
    UNIQUE (run_id,position), FOREIGN KEY (run_id,position) REFERENCES journal(run_id,position) ON DELETE CASCADE
);
INSERT INTO broca_calls SELECT * FROM migration_45_broca;
DROP TABLE migration_45_broca;
CREATE INDEX broca_calls_pending ON broca_calls(send_id) WHERE json_extract(snapshot, '$.acknowledged') = 0 AND COALESCE(json_extract(snapshot, '$.deferred'), 0) = 0;
CREATE TRIGGER broca_calls_forget_wait AFTER DELETE ON broca_calls BEGIN
    DELETE FROM broca_waits WHERE send_id = old.send_id;
    DELETE FROM broca_wakes WHERE send_id = old.send_id;
END;
ALTER TABLE outbox ADD COLUMN failed_at INTEGER;
ALTER TABLE outbox ADD COLUMN failure_code TEXT;
CREATE INDEX secret_revoke_due ON outbox(retry_at) WHERE kind='secret.revoke_flow_grants' AND delivered_at IS NULL AND failed_at IS NULL;
