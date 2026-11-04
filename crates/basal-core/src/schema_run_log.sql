-- Staged run-log control plane. Not part of the production migration chain.
CREATE TABLE run_identities (
    run_id TEXT PRIMARY KEY NOT NULL,
    flow_id TEXT NOT NULL,
    flow_version INTEGER NOT NULL CHECK(flow_version >= 0),
    code_hash BLOB NOT NULL CHECK(length(code_hash) = 32),
    snapshot_id INTEGER NOT NULL REFERENCES run_snapshots(snapshot_id),
    UNIQUE(run_id, flow_id, flow_version)
);
CREATE INDEX run_identities_flow ON run_identities(flow_id, flow_version);

-- Only admission data lives here; execution state belongs to the run actor.
CREATE TABLE live_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    flow_id TEXT NOT NULL,
    flow_version INTEGER NOT NULL,
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
    FOREIGN KEY(run_id, flow_id, flow_version)
        REFERENCES run_identities(run_id, flow_id, flow_version)
);
CREATE INDEX live_runs_flow_order ON live_runs(flow_id, admit_seq);

CREATE TABLE run_history (
    run_id TEXT PRIMARY KEY NOT NULL,
    flow_id TEXT NOT NULL,
    flow_version INTEGER NOT NULL,
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
    FOREIGN KEY(run_id, flow_id, flow_version)
        REFERENCES run_identities(run_id, flow_id, flow_version)
);
CREATE INDEX run_history_flow_order ON run_history(flow_id, admit_seq DESC, run_id DESC);
CREATE INDEX run_history_retention ON run_history(ended_at, run_id);

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
);
