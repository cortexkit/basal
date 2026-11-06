-- Run-log control plane, installed by fresh initialization or the cutover.
CREATE UNIQUE INDEX run_snapshots_identity ON run_snapshots(snapshot_id, flow_id, flow_version);

-- Execution state belongs to the actor; history_lost survives a missing log.
CREATE TABLE live_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    flow_id TEXT NOT NULL,
    flow_version INTEGER NOT NULL,
    snapshot_id INTEGER NOT NULL,
    trigger_id TEXT NOT NULL,
    trigger_kind TEXT NOT NULL,
    trigger TEXT NOT NULL CHECK(json_valid(trigger)),
    attempt INTEGER NOT NULL CHECK(attempt >= 0),
    admitted_at INTEGER NOT NULL,
    admit_seq INTEGER NOT NULL,
    admit_epoch INTEGER NOT NULL CHECK(admit_epoch >= 0),
    admit_segment INTEGER NOT NULL CHECK(admit_segment >= 0),
    admit_offset INTEGER NOT NULL CHECK(admit_offset >= 0),
    deadline_ms INTEGER NOT NULL CHECK(deadline_ms >= 0),
    self_input TEXT NOT NULL CHECK(json_valid(self_input)),
    params_input TEXT NOT NULL CHECK(json_valid(params_input)),
    grant_guards TEXT NOT NULL CHECK(json_valid(grant_guards)),
    template_incarnation INTEGER,
    end_instance INTEGER NOT NULL CHECK(end_instance IN (0,1)),
    history_lost INTEGER NOT NULL DEFAULT 0 CHECK(history_lost IN (0,1)),
    FOREIGN KEY(snapshot_id, flow_id, flow_version)
        REFERENCES run_snapshots(snapshot_id, flow_id, flow_version)
) WITHOUT ROWID;
CREATE INDEX live_runs_flow_order ON live_runs(flow_id, admit_seq);

CREATE TABLE run_history (
    run_id TEXT PRIMARY KEY NOT NULL,
    flow_id TEXT NOT NULL,
    flow_version INTEGER NOT NULL,
    snapshot_id INTEGER NOT NULL,
    trigger_id TEXT NOT NULL,
    trigger_kind TEXT NOT NULL,
    trigger TEXT NOT NULL CHECK(json_valid(trigger)),
    admitted_at INTEGER NOT NULL,
    admit_seq INTEGER NOT NULL,
    first_claim_at INTEGER,
    ended_at INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('succeeded','failed','engine_mismatch','cancelled')),
    error_kind TEXT CHECK(length(CAST(error_kind AS BLOB)) <= 256),
    error_detail TEXT CHECK(length(CAST(error_detail AS BLOB)) <= 8192),
    result TEXT CHECK(result IS NULL OR json_valid(result)),
    result_size INTEGER NOT NULL CHECK(result_size >= 0),
    call_counts TEXT NOT NULL CHECK(json_valid(call_counts)),
    writes_sent INTEGER NOT NULL CHECK(writes_sent >= 0),
    tokens_charged INTEGER NOT NULL CHECK(tokens_charged >= 0),
    cost_microusd INTEGER NOT NULL CHECK(cost_microusd >= 0),
    broken INTEGER NOT NULL CHECK(broken >= 0),
    activation_count INTEGER NOT NULL CHECK(activation_count >= 0),
    script_started INTEGER NOT NULL CHECK(script_started IN (0,1)),
    phase TEXT NOT NULL,
    deadline_exempt INTEGER NOT NULL CHECK(deadline_exempt IN (0,1)),
    history_lost INTEGER NOT NULL CHECK(history_lost IN (0,1)),
    commit_seq INTEGER NOT NULL CHECK(commit_seq >= 0),
    receipts TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(receipts) AND json_type(receipts)='object'),
    FOREIGN KEY(snapshot_id, flow_id, flow_version)
        REFERENCES run_snapshots(snapshot_id, flow_id, flow_version)
);
CREATE INDEX run_history_flow_order ON run_history(flow_id, admit_seq DESC, run_id DESC);
CREATE INDEX run_history_retention ON run_history(ended_at, run_id);

-- Identity follows the run through the atomic live-to-history transition.
-- Both arms use the run key for lookups and their flow indexes for liveness.
CREATE VIEW run_identities AS
SELECT r.run_id,r.flow_id,r.flow_version,s.code_hash,r.snapshot_id
FROM live_runs r JOIN run_snapshots s USING(snapshot_id)
UNION ALL
SELECT r.run_id,r.flow_id,r.flow_version,s.code_hash,r.snapshot_id
FROM run_history r JOIN run_snapshots s USING(snapshot_id);

CREATE TABLE obligations (
    run_id TEXT NOT NULL REFERENCES run_history(run_id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK(position >= 0),
    handle TEXT,
    state TEXT NOT NULL CHECK(state IN ('open','unknown','settled')),
    audit_at INTEGER NOT NULL,
    detail TEXT NOT NULL CHECK(json_valid(detail)),
    value_hash BLOB CHECK(value_hash IS NULL OR length(value_hash) = 32),
    receipt_unknown INTEGER NOT NULL DEFAULT 0 CHECK(receipt_unknown IN (0,1)),
    settled_at INTEGER,
    PRIMARY KEY(run_id, position),
    CHECK ((state = 'settled') = (settled_at IS NOT NULL)),
    CHECK ((state = 'settled') = (value_hash IS NOT NULL OR receipt_unknown = 1)),
    CHECK (NOT (value_hash IS NOT NULL AND receipt_unknown = 1))
);
CREATE INDEX obligations_open ON obligations(audit_at, run_id, position) WHERE state <> 'settled';

CREATE TABLE run_calls (
    run_id TEXT NOT NULL REFERENCES run_history(run_id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK(position >= 0),
    kind INTEGER NOT NULL,
    class TEXT NOT NULL CHECK(class IN ('sync','local','query','mutation','keyed_mutation')),
    module TEXT,
    op TEXT,
    args_digest BLOB NOT NULL CHECK(length(args_digest) = 32),
    outcome TEXT NOT NULL CHECK(outcome IN ('pending','fulfilled','rejected','unknown')),
    error_code TEXT,
    issued_at INTEGER NOT NULL,
    arrived_at INTEGER,
    released_at INTEGER,
    detail TEXT NOT NULL CHECK(json_valid(detail)),
    PRIMARY KEY(run_id, position),
    CHECK ((kind = 0) = (module IS NOT NULL AND op IS NOT NULL))
);

CREATE TABLE run_log_segments (
    log_epoch INTEGER NOT NULL CHECK(log_epoch >= 0),
    seq INTEGER NOT NULL CHECK(seq >= 0),
    state TEXT NOT NULL CHECK(state IN ('reserved','active')),
    PRIMARY KEY(log_epoch, seq)
) WITHOUT ROWID;

CREATE INDEX outbox_run_removals ON outbox(CAST(json_extract(body,'$.run_id') AS TEXT))
    WHERE kind='run.removed' AND delivered_at IS NULL;

-- The cutover replaces these shared control tables, preserving their rows.
-- Small keyed control rows fit index leaves. Snapshots, history, call detail
-- and unsettled obligations retain rowids: their large payloads would spill
-- at the lower index-page threshold of WITHOUT ROWID tables.
CREATE TABLE slim_trigger_inbox (
    flow_id TEXT NOT NULL,
    trigger_id TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    run_id TEXT NOT NULL UNIQUE,
    admitted_at INTEGER NOT NULL,
    PRIMARY KEY(flow_id, trigger_id, attempt)
) WITHOUT ROWID;
INSERT INTO slim_trigger_inbox SELECT * FROM trigger_inbox;
DROP TABLE trigger_inbox;
ALTER TABLE slim_trigger_inbox RENAME TO trigger_inbox;

CREATE TABLE slim_tombstones (
    kind TEXT NOT NULL CHECK(kind IN ('trigger','idempotency_key')),
    key TEXT NOT NULL,
    run_id TEXT NOT NULL,
    pruned_at INTEGER NOT NULL,
    PRIMARY KEY(kind, key)
) WITHOUT ROWID;
INSERT INTO slim_tombstones SELECT * FROM tombstones;
DROP TABLE tombstones;
ALTER TABLE slim_tombstones RENAME TO tombstones;

CREATE TABLE slim_rate_windows (
    flow_id TEXT NOT NULL,
    window_ms INTEGER NOT NULL,
    window_start INTEGER NOT NULL,
    runs INTEGER NOT NULL DEFAULT 0,
    dispatches INTEGER NOT NULL DEFAULT 0,
    saturated INTEGER NOT NULL DEFAULT 0 CHECK(saturated IN (0,1)),
    PRIMARY KEY(flow_id, window_ms, window_start)
) WITHOUT ROWID;
INSERT INTO slim_rate_windows SELECT * FROM rate_windows;
DROP TABLE rate_windows;
ALTER TABLE slim_rate_windows RENAME TO rate_windows;
CREATE INDEX rate_windows_retention ON rate_windows(window_start+window_ms);

CREATE TABLE slim_schedules (
    flow_id TEXT PRIMARY KEY NOT NULL,
    version INTEGER NOT NULL CHECK(version >= 0),
    spec TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('active','disabled')),
    anchor_ms INTEGER NOT NULL,
    next_due_ms INTEGER,
    last_fired_ms INTEGER,
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;
INSERT INTO slim_schedules SELECT * FROM schedules;
DROP TABLE schedules;
ALTER TABLE slim_schedules RENAME TO schedules;
CREATE INDEX schedules_due ON schedules(state, next_due_ms);

-- Staged planning allocates seq from meta.next_run in the same transaction
-- as the fire. The shared allocator survives empty queues and process restarts.
CREATE TABLE slim_schedule_fires (
    seq INTEGER PRIMARY KEY,
    flow_id TEXT NOT NULL,
    trigger_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    due_ms INTEGER NOT NULL,
    payload TEXT NOT NULL,
    planned_at INTEGER NOT NULL,
    UNIQUE(flow_id, trigger_id)
);
INSERT INTO slim_schedule_fires SELECT * FROM schedule_fires;
INSERT INTO meta(key,value)
SELECT 'next_run', CAST(MAX(seq)+1 AS TEXT) FROM (
    SELECT seq FROM schedule_fires
    UNION ALL SELECT seq FROM sqlite_sequence WHERE name='schedule_fires'
) HAVING MAX(seq) IS NOT NULL
ON CONFLICT(key) DO UPDATE SET value=CAST(MAX(CAST(meta.value AS INTEGER),CAST(excluded.value AS INTEGER)) AS TEXT);
DROP TABLE schedule_fires;
ALTER TABLE slim_schedule_fires RENAME TO schedule_fires;
