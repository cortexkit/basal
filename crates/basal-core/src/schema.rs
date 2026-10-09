//! The store schema, as versioned migrations.
//!
//! The journal is an issue log. Each call gets one row when it is issued,
//! before it is dispatched; the row's outcome and its delivery order are
//! written together, only when the outcome is handed to a worker (or, for a
//! synchronous clock or random call, in the same transaction that issues it).
//! An outcome that has arrived but has not been handed over waits in
//! `mailbox`. So a journal outcome always carries its delivery order, and the
//! prefix a worker replays never contains an outcome whose release order was
//! not committed: that is the replay barrier, enforced by a CHECK.

use cortexkit_store::Migration;

/// The namespace basal's chain is recorded under in the shared version table.
pub const NAMESPACE: &str = "basal_core";

/// The journal layout, part of a run's runtime fingerprint: a run recorded
/// under one layout is never replayed under another.
pub const JOURNAL_FORMAT: u32 = 1;

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        statements: r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE runs (
    run_id       TEXT PRIMARY KEY,
    flow_id      TEXT NOT NULL,
    trigger_id   TEXT NOT NULL,
    attempt      INTEGER NOT NULL,
    trigger      TEXT NOT NULL,
    script       TEXT NOT NULL,
    manifest     TEXT NOT NULL,
    code_hash    BLOB NOT NULL,
    fingerprint  TEXT,
    state        TEXT NOT NULL CHECK (state IN ('pending', 'running', 'suspended',
                    'succeeded', 'failed', 'needs_reconcile', 'engine_mismatch', 'cancelled')),
    owner        TEXT,
    generation   INTEGER NOT NULL DEFAULT 0,
    readiness    INTEGER NOT NULL DEFAULT 0,
    awaited      TEXT,
    result       TEXT,
    error_kind   TEXT,
    error_detail TEXT,
    broken       INTEGER NOT NULL DEFAULT 0,
    admitted_at  INTEGER NOT NULL,
    ended_at     INTEGER,
    -- Exactly the running state has an owner.
    CHECK ((state = 'running') = (owner IS NOT NULL))
);
CREATE INDEX runs_state ON runs (state);

CREATE TABLE trigger_inbox (
    flow_id     TEXT NOT NULL,
    trigger_id  TEXT NOT NULL,
    attempt     INTEGER NOT NULL,
    run_id      TEXT NOT NULL UNIQUE,
    admitted_at INTEGER NOT NULL,
    PRIMARY KEY (flow_id, trigger_id, attempt)
);

CREATE TABLE tombstones (
    kind      TEXT NOT NULL CHECK (kind IN ('trigger', 'idempotency_key')),
    key       TEXT NOT NULL,
    run_id    TEXT NOT NULL,
    pruned_at INTEGER NOT NULL,
    PRIMARY KEY (kind, key)
);

CREATE TABLE journal (
    run_id            TEXT NOT NULL REFERENCES runs (run_id),
    position          INTEGER NOT NULL CHECK (position >= 0),
    kind_code         INTEGER NOT NULL,
    module            TEXT,
    op                TEXT,
    args              TEXT NOT NULL,
    args_digest       BLOB NOT NULL CHECK (length(args_digest) = 32),
    idempotency_key   TEXT NOT NULL UNIQUE,
    class             TEXT NOT NULL CHECK (class IN ('sync', 'local', 'query', 'mutation', 'keyed_mutation')),
    dispatch          TEXT NOT NULL CHECK (dispatch IN ('none', 'sent', 'accepted', 'unknown', 'not_applied')),
    attempts          INTEGER NOT NULL DEFAULT 0,
    handle            TEXT,
    settlement        TEXT CHECK (settlement IN ('fulfilled', 'rejected')),
    value             TEXT,
    payload_hash      BLOB,
    delivery_order    INTEGER CHECK (delivery_order >= 0),
    clock_ms          REAL,
    issued_generation INTEGER NOT NULL,
    PRIMARY KEY (run_id, position),
    CHECK ((kind_code = 0) = (module IS NOT NULL AND op IS NOT NULL)),
    CHECK ((settlement IS NULL) = (value IS NULL)),
    CHECK ((settlement IS NULL) = (payload_hash IS NULL)),
    -- The replay barrier: an outcome enters the journal only together with
    -- the order in which it was released to a worker.
    CHECK ((settlement IS NULL) = (delivery_order IS NULL))
);
CREATE UNIQUE INDEX journal_delivery_order ON journal (run_id, delivery_order)
    WHERE delivery_order IS NOT NULL;

CREATE TABLE mailbox (
    seq                INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id             TEXT NOT NULL,
    position           INTEGER NOT NULL,
    handle             TEXT,
    settlement         TEXT NOT NULL CHECK (settlement IN ('fulfilled', 'rejected')),
    value              TEXT NOT NULL,
    payload_hash       BLOB NOT NULL,
    source             TEXT NOT NULL CHECK (source IN ('host', 'completion', 'local', 'reconcile')),
    arrived_generation INTEGER NOT NULL,
    UNIQUE (run_id, position),
    FOREIGN KEY (run_id, position) REFERENCES journal (run_id, position)
);

CREATE TABLE quarantine (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id       TEXT NOT NULL,
    position     INTEGER NOT NULL,
    handle       TEXT,
    settlement   TEXT NOT NULL,
    value        TEXT NOT NULL,
    payload_hash BLOB NOT NULL,
    reason       TEXT NOT NULL CHECK (reason IN ('contradicts_outcome', 'unknown_call',
                    'handle_mismatch', 'run_cancelled')),
    at           INTEGER NOT NULL,
    UNIQUE (run_id, position, payload_hash, reason)
);

CREATE TABLE activations (
    run_id     TEXT NOT NULL,
    generation INTEGER NOT NULL,
    owner      TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, generation)
);

CREATE TABLE audit (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    at       INTEGER NOT NULL,
    actor    TEXT NOT NULL,
    action   TEXT NOT NULL,
    run_id   TEXT,
    position INTEGER,
    detail   TEXT NOT NULL
);

-- A stand-in for a flow's own durable state: the local, eager effect class.
-- Its effect commits in the same transaction as the call's outcome, so a
-- crash leaves either both or neither. local_effects logs every application
-- so tests can count them per idempotency key.
CREATE TABLE local_kv (
    flow_id TEXT NOT NULL,
    key     TEXT NOT NULL,
    value   TEXT NOT NULL,
    PRIMARY KEY (flow_id, key)
);
CREATE TABLE local_effects (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL,
    flow_id         TEXT NOT NULL,
    op              TEXT NOT NULL
);
"#,
    },
    Migration {
        version: 2,
        statements: DISPATCH,
    },
    // Schedules. A schedule's fires are planned in the same commit that
    // advances it and wait in `schedule_fires` until they are admitted, so
    // a restart between the two never recomputes a catch-up fire over due
    // times already accounted for. Times are milliseconds since the Unix
    // epoch, UTC.
    Migration {
        version: 3,
        statements: r#"
-- One row per flow whose approved version has a schedule trigger. It
-- holds the schedule's own state only: the code a fire runs is the flow's
-- approved version at admission (installs), never a copy kept here.
CREATE TABLE schedules (
    flow_id       TEXT PRIMARY KEY,
    version       INTEGER NOT NULL CHECK (version >= 0),
    spec          TEXT NOT NULL,
    state         TEXT NOT NULL CHECK (state IN ('active', 'disabled')),
    anchor_ms     INTEGER NOT NULL,
    next_due_ms   INTEGER,
    last_fired_ms INTEGER,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX schedules_due ON schedules (state, next_due_ms);

-- Fires decided by a tick and not yet admitted. `version` is the version
-- whose schedule planned the fire, kept for the record: admission binds the
-- fire to whatever version is approved when it is admitted.
CREATE TABLE schedule_fires (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    flow_id    TEXT NOT NULL,
    trigger_id TEXT NOT NULL,
    version    INTEGER NOT NULL,
    due_ms     INTEGER NOT NULL,
    payload    TEXT NOT NULL,
    planned_at INTEGER NOT NULL,
    UNIQUE (flow_id, trigger_id)
);

-- Planned fires admission refused for good (the flow was disabled, had no
-- approved version, or was over its run rate limit): dropped, and recorded
-- here so a missing run can be explained.
CREATE TABLE schedule_dropped (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    flow_id    TEXT NOT NULL,
    trigger_id TEXT NOT NULL,
    due_ms     INTEGER NOT NULL,
    payload    TEXT NOT NULL,
    reason     TEXT NOT NULL CHECK (reason IN ('disabled', 'not_approved', 'rate_limited')),
    at         INTEGER NOT NULL
);
"#,
    },
    Migration {
        version: 4,
        statements: CARDS,
    },
    Migration {
        version: 5,
        statements: r#"
-- One snapshot per Broca model call: its frozen send, its Broca run id, its
-- outcome once Broca reports one, and whether the runtime has recorded it.
-- Kept beside the journal so the snapshot is deleted with its call.
CREATE TABLE broca_calls (
    send_id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    snapshot TEXT NOT NULL,
    UNIQUE (run_id, position),
    FOREIGN KEY (run_id, position) REFERENCES journal (run_id, position) ON DELETE CASCADE
);
"#,
    },
    Migration {
        version: 6,
        statements: DECISIONS,
    },
    Migration {
        version: 7,
        statements: UNKNOWN_REASON,
    },
    Migration {
        version: 8,
        statements: r#"
-- A version core no longer stands behind (the operator revoked it in core,
-- core holds no install of it, or core approved other code under it), with
-- why. It is never activated or approved again. A revoked version that was
-- approved is moved to 'superseded'; revoked_at is what tells the two apart.
ALTER TABLE installs ADD COLUMN revoked_at INTEGER;
ALTER TABLE installs ADD COLUMN revoked_reason TEXT;
"#,
    },
    Migration {
        version: 9,
        statements: FLOW_DEFERRALS,
    },
    Migration {
        version: 10,
        statements: PACKAGES,
    },
    Migration {
        version: 11,
        statements: HEALTH_INDEXES,
    },
    Migration {
        version: 12,
        statements: RETENTION_INDEXES,
    },
    Migration {
        version: 13,
        statements: WAKE_INDEXES,
    },
    Migration {
        version: 14,
        statements: CODEMODE,
    },
];

/// Codemode runs and their tool calls, kept apart from the flow tables.
///
/// A codemode run is never a flow run: it has no flow, lease, trigger or
/// replay, and none of the flow paths (the journal, call recovery, retry,
/// suspension, reconcile, flow retention or the flow ops) reads these
/// tables. Each call is attempted at most once, so nothing here records
/// attempts or resend state.
///
/// The triggers make the outcome rules hold for any writer, not only the
/// functions in `codemode::store`: a recorded call outcome and a terminal
/// run never change, a run becomes terminal only once none of its calls is
/// pending, calls are added only while their run is running, and a run's row
/// is deleted only behind its permanent tombstone, whose id is never used
/// again.
const CODEMODE: &str = r#"
-- One row per admitted run. `catalog` is the catalog as admitted and
-- `catalog_digest` its digest; `limits` and `scope` are the admitted
-- request's, as JSON. `ended_at` and the frozen `duration_ms` are set
-- exactly when the run is terminal. `value` is the program's return value
-- (JSON), only for a completed run; `error_code` and `error_message` are
-- present together.
CREATE TABLE codemode_runs (
    run_id         TEXT PRIMARY KEY,
    agent_id       TEXT NOT NULL,
    program        TEXT NOT NULL,
    catalog        TEXT NOT NULL CHECK (json_valid(catalog) AND json_type(catalog) = 'array'),
    catalog_digest TEXT NOT NULL CHECK (length(catalog_digest) = 64 AND catalog_digest NOT GLOB '*[^0-9a-f]*'),
    description    TEXT CHECK (description IS NULL OR (length(CAST(description AS BLOB)) <= 1024
                       AND instr(description, char(10)) = 0 AND instr(description, char(13)) = 0
                       AND instr(description, char(8232)) = 0 AND instr(description, char(8233)) = 0)),
    limits         TEXT NOT NULL CHECK (json_valid(limits) AND json_type(limits) = 'object'),
    scope          TEXT NOT NULL CHECK (json_valid(scope) AND json_type(scope) = 'object'),
    deadline_ms    INTEGER NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('running', 'completed', 'failed',
                       'budget_exhausted:js_cpu', 'budget_exhausted:memory', 'budget_exhausted:stack',
                       'budget_exhausted:wall', 'budget_exhausted:tool_calls', 'cancelled', 'interrupted')),
    value          TEXT CHECK (value IS NULL OR (status = 'completed' AND json_valid(value))),
    error_code     TEXT,
    error_message  TEXT CHECK (error_message IS NULL OR length(error_message) > 0),
    output         TEXT NOT NULL DEFAULT '',
    warnings       TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(warnings) AND json_type(warnings) = 'array'),
    admitted_at    INTEGER NOT NULL,
    ended_at       INTEGER,
    duration_ms    INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    CHECK ((status = 'running') = (ended_at IS NULL)),
    CHECK ((status = 'running') = (duration_ms IS NULL)),
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK ((error_code IS NOT NULL) = (status = 'failed' OR status = 'interrupted'
        OR status LIKE 'budget_exhausted:%')),
    CHECK (status NOT LIKE 'budget_exhausted:%' OR error_code = status)
);
CREATE INDEX codemode_runs_active ON codemode_runs (agent_id) WHERE status = 'running';
CREATE INDEX codemode_runs_ended ON codemode_runs (ended_at) WHERE ended_at IS NOT NULL;

-- One row per tool call the parent recorded, by the worker's position.
-- `intent_at` is set when the intent to send was committed, just before the
-- single send; a row without it was never sent. `outcome` is 'pending'
-- while the call is queued or awaiting its answer. `duration_ms` runs from
-- `intent_at` to the recorded outcome and exists only for a sent call with
-- an outcome. `input_bytes` is the size of the call's input, which is not
-- stored.
CREATE TABLE codemode_calls (
    run_id          TEXT NOT NULL REFERENCES codemode_runs (run_id),
    position        INTEGER NOT NULL CHECK (position >= 0),
    tool            TEXT NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    input_bytes     INTEGER NOT NULL CHECK (input_bytes >= 0),
    intent_at       INTEGER,
    outcome         TEXT NOT NULL CHECK (outcome IN ('pending', 'ok', 'error', 'refused',
                        'consent_unavailable', 'tool_unavailable', 'outcome_unknown', 'cancelled')),
    code            TEXT,
    duration_ms     INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    PRIMARY KEY (run_id, position),
    CHECK ((code IS NULL) = (outcome IN ('pending', 'ok', 'cancelled'))),
    CHECK (outcome NOT IN ('ok', 'error', 'consent_unavailable', 'outcome_unknown') OR intent_at IS NOT NULL),
    CHECK (outcome <> 'cancelled' OR intent_at IS NULL),
    CHECK ((duration_ms IS NOT NULL) = (intent_at IS NOT NULL AND outcome <> 'pending'))
);

-- A pruned run's id, kept forever so the id is never admitted again.
CREATE TABLE codemode_tombstones (
    run_id    TEXT PRIMARY KEY,
    pruned_at INTEGER NOT NULL
);

CREATE TRIGGER codemode_calls_outcome_is_final BEFORE UPDATE ON codemode_calls
WHEN OLD.outcome <> 'pending'
BEGIN SELECT RAISE(ABORT, 'a recorded codemode call outcome never changes'); END;

CREATE TRIGGER codemode_calls_only_while_running BEFORE INSERT ON codemode_calls
WHEN NOT EXISTS (SELECT 1 FROM codemode_runs WHERE run_id = NEW.run_id AND status = 'running')
BEGIN SELECT RAISE(ABORT, 'codemode calls are recorded only while their run is running'); END;

CREATE TRIGGER codemode_runs_terminal_is_final BEFORE UPDATE ON codemode_runs
WHEN OLD.status <> 'running'
BEGIN SELECT RAISE(ABORT, 'a terminal codemode run never changes'); END;

CREATE TRIGGER codemode_runs_end_with_no_pending_call BEFORE UPDATE OF status ON codemode_runs
WHEN NEW.status <> 'running'
    AND EXISTS (SELECT 1 FROM codemode_calls WHERE run_id = NEW.run_id AND outcome = 'pending')
BEGIN SELECT RAISE(ABORT, 'a codemode run ends only once none of its calls is pending'); END;

CREATE TRIGGER codemode_runs_not_tombstoned BEFORE INSERT ON codemode_runs
WHEN EXISTS (SELECT 1 FROM codemode_tombstones WHERE run_id = NEW.run_id)
BEGIN SELECT RAISE(ABORT, 'a pruned codemode run id is never used again'); END;

CREATE TRIGGER codemode_runs_delete_behind_tombstone BEFORE DELETE ON codemode_runs
WHEN OLD.status = 'running'
    OR NOT EXISTS (SELECT 1 FROM codemode_tombstones WHERE run_id = OLD.run_id)
BEGIN SELECT RAISE(ABORT, 'a codemode run is deleted only once it has ended and been tombstoned'); END;

CREATE TRIGGER codemode_tombstones_no_update BEFORE UPDATE ON codemode_tombstones
BEGIN SELECT RAISE(ABORT, 'codemode tombstones are permanent'); END;

CREATE TRIGGER codemode_tombstones_no_delete BEFORE DELETE ON codemode_tombstones
BEGIN SELECT RAISE(ABORT, 'codemode tombstones are permanent'); END;
"#;

// The idle engine sleeps until its earliest timer. Finding that timer takes the
// smallest live run deadline and the smallest deferred-call retry time; these
// ordered indexes let each MIN stop at its first entry instead of visiting
// every live run or every deferred call.
const WAKE_INDEXES: &str = r#"
CREATE INDEX runs_state_deadline ON runs(state,deadline_at) WHERE deadline_at IS NOT NULL;
CREATE INDEX journal_deferred_retry ON journal(retry_not_before,run_id) WHERE dispatch='deferred' AND retry_not_before IS NOT NULL;
"#;

// Bounded cleanup searches age ranges and checks the obligations that can keep
// a run or a refundable window alive. The trigger inbox's unique run_id index
// already covers run deletion. Quarantine's unique index puts payload_hash
// before reason, so it cannot seek directly to a cancellation receipt.
// outbox_refusal_run indexes the run id inside each refusal record, cast to
// TEXT like runs.run_id: without the matching type SQLite scans the whole
// partial index instead of looking up one run. runs_retention leads with
// state so SQLite prefers it to runs_state for the age search even before
// the store has planner statistics.
const RETENTION_INDEXES: &str = r#"
CREATE INDEX runs_retention ON runs(state,ended_at,run_id) WHERE state IN ('succeeded','failed','engine_mismatch','cancelled');
CREATE INDEX quarantine_obligation ON quarantine(run_id,position,reason);
CREATE INDEX outbox_refusal_run ON outbox(CAST(json_extract(body,'$.run_id') AS TEXT)) WHERE kind='refusal_committed';
CREATE INDEX call_audit_refund_window ON call_audit(flow_id,at,run_id);
CREATE INDEX token_ledger_retention_window ON token_ledger(flow_id,window_ms,window_start,run_id,state);
CREATE INDEX quarantine_retention ON quarantine(at);
CREATE INDEX call_audit_retention ON call_audit(at);
CREATE INDEX audit_retention ON audit(at);
CREATE INDEX outbox_retention ON outbox(delivered_at) WHERE delivered_at IS NOT NULL AND kind<>'refusal_committed';
CREATE INDEX schedule_dropped_retention ON schedule_dropped(at);
CREATE INDEX token_ledger_retention ON token_ledger(settled_at) WHERE state='settled';
CREATE INDEX token_windows_retention ON token_windows(window_start+window_ms);
CREATE INDEX rate_windows_retention ON rate_windows(window_start+window_ms);
CREATE INDEX decision_cards_retention ON decision_cards(answered_at) WHERE state<>'open';
"#;

// Health reads the finished suffix and pending work of each flow. Decision
// lookups need the latest episode and open cards, not every historical card.
const HEALTH_INDEXES: &str = r#"
CREATE INDEX runs_flow_finished ON runs(flow_id, admit_seq DESC, run_id DESC) WHERE state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled');
CREATE INDEX runs_flow_state_admission ON runs(flow_id, state, admitted_at, admit_seq);
CREATE INDEX journal_deferred ON journal(run_id, position) WHERE dispatch = 'deferred';
CREATE INDEX decision_cards_kind_flow_instance ON decision_cards(kind, flow_id, instance DESC);
CREATE INDEX decision_cards_open ON decision_cards(seq) WHERE state = 'open';
CREATE INDEX decision_cards_subject ON decision_cards(kind, flow_id, run_id, call_key, seq DESC);
CREATE INDEX broca_calls_pending ON broca_calls(send_id) WHERE json_extract(snapshot, '$.acknowledged') = 0 AND COALESCE(json_extract(snapshot, '$.deferred'), 0) = 0;
"#;

const PACKAGES: &str = r#"
CREATE TABLE package_versions (
    package TEXT NOT NULL,
    version INTEGER NOT NULL CHECK (version >= 1),
    script TEXT NOT NULL,
    manifest TEXT NOT NULL,
    code_hash BLOB NOT NULL CHECK (length(code_hash) = 32),
    registered_at INTEGER NOT NULL,
    PRIMARY KEY (package, version)
);
CREATE TRIGGER package_versions_no_update BEFORE UPDATE ON package_versions
BEGIN SELECT RAISE(ABORT, 'package versions are immutable'); END;
CREATE TRIGGER package_versions_no_delete BEFORE DELETE ON package_versions
BEGIN SELECT RAISE(ABORT, 'package versions are immutable'); END;
CREATE TRIGGER package_versions_no_replace BEFORE INSERT ON package_versions
WHEN EXISTS (SELECT 1 FROM package_versions WHERE package=NEW.package AND version=NEW.version)
BEGIN SELECT RAISE(ABORT, 'package versions are immutable'); END;
ALTER TABLE flows ADD COLUMN package TEXT;
ALTER TABLE flows ADD COLUMN removed INTEGER NOT NULL DEFAULT 0 CHECK (removed IN (0,1));
CREATE TABLE instance_revocations (
    flow_id TEXT NOT NULL REFERENCES flows(flow_id),
    version INTEGER NOT NULL,
    reason TEXT NOT NULL,
    revoked_at INTEGER NOT NULL,
    PRIMARY KEY(flow_id,version)
);
-- Ordering exists even before a flow does. The first reply is durable so a
-- resend cannot accidentally report the state left by a subsequent call.
CREATE TABLE instance_generations (
    package TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('ensure','remove')),
    version INTEGER,
    reply TEXT NOT NULL CHECK (json_valid(reply)),
    PRIMARY KEY (package, agent_id),
    CHECK ((operation = 'ensure') = (version IS NOT NULL))
);
-- Snapshot the owner as activation input, independently of the flow row.
ALTER TABLE runs ADD COLUMN self_input TEXT CHECK (self_input IS NULL OR json_valid(self_input));
"#;

// SQLite cannot widen an existing CHECK with ALTER TABLE. Copy the issue log
// and its two foreign-key children in the migration transaction, preserving
// every old column, including already released outcomes and mailbox sequence.
const FLOW_DEFERRALS: &str = r#"
CREATE TABLE migration_9_mailbox AS SELECT * FROM mailbox;
CREATE TABLE migration_9_broca AS SELECT * FROM broca_calls;
DROP TABLE mailbox;
DROP TABLE broca_calls;
CREATE TABLE journal_new (
    run_id TEXT NOT NULL REFERENCES runs (run_id),
    position INTEGER NOT NULL CHECK (position >= 0),
    kind_code INTEGER NOT NULL, module TEXT, op TEXT, args TEXT NOT NULL,
    args_digest BLOB NOT NULL CHECK (length(args_digest) = 32),
    idempotency_key TEXT NOT NULL UNIQUE,
    class TEXT NOT NULL CHECK (class IN ('sync', 'local', 'query', 'mutation', 'keyed_mutation')),
    dispatch TEXT NOT NULL CHECK (dispatch IN ('none', 'sent', 'accepted', 'unknown', 'not_applied', 'deferred')),
    attempts INTEGER NOT NULL DEFAULT 0, handle TEXT,
    settlement TEXT CHECK (settlement IN ('fulfilled', 'rejected')), value TEXT,
    payload_hash BLOB, delivery_order INTEGER CHECK (delivery_order >= 0),
    clock_ms REAL, issued_generation INTEGER NOT NULL, request TEXT,
    unknown_reason TEXT CHECK (unknown_reason IN ('basal_restarted', 'connection_lost', 'reply_timeout', 'reply_unreadable', 'retries_exhausted', 'provider_lost_run'))
        CHECK (dispatch <> 'unknown' OR unknown_reason IS NOT NULL),
    refusal TEXT CHECK (refusal IN ('scope_not_carrier', 'scope_ended', 'scope_not_live', 'scope_epoch_required', 'scope_not_synced', 'scope_changed', 'scope_unsupported', 'resource_busy', 'consent_unavailable')),
    retry_not_before INTEGER,
    refusal_detail TEXT,
    PRIMARY KEY (run_id, position),
    CHECK ((kind_code = 0) = (module IS NOT NULL AND op IS NOT NULL)),
    CHECK ((settlement IS NULL) = (value IS NULL)),
    CHECK ((settlement IS NULL) = (payload_hash IS NULL)),
    CHECK ((settlement IS NULL) = (delivery_order IS NULL)),
    CHECK ((dispatch = 'deferred') = (refusal IS NOT NULL)),
    CHECK ((dispatch = 'deferred') = (retry_not_before IS NOT NULL)),
    CHECK (retry_not_before IS NULL OR typeof(retry_not_before) = 'integer'),
    CHECK ((dispatch = 'deferred') = (refusal_detail IS NOT NULL)),
    CHECK (dispatch <> 'deferred' OR (settlement IS NULL AND json_valid(refusal_detail)
        AND json_type(refusal_detail) = 'object'
        AND json_extract(refusal_detail, '$.reason') IS refusal
        AND json_type(refusal_detail, '$.provider') IS 'text'
        AND json_type(refusal_detail, '$.action') IS 'text'))
);
INSERT INTO journal_new SELECT *, NULL, NULL, NULL FROM journal;
DROP TABLE journal;
ALTER TABLE journal_new RENAME TO journal;
CREATE UNIQUE INDEX journal_delivery_order ON journal (run_id, delivery_order) WHERE delivery_order IS NOT NULL;
CREATE TABLE mailbox (
    seq INTEGER PRIMARY KEY AUTOINCREMENT, run_id TEXT NOT NULL, position INTEGER NOT NULL,
    handle TEXT, settlement TEXT NOT NULL CHECK (settlement IN ('fulfilled', 'rejected')),
    value TEXT NOT NULL, payload_hash BLOB NOT NULL,
    source TEXT NOT NULL CHECK (source IN ('host', 'completion', 'local', 'reconcile')),
    arrived_generation INTEGER NOT NULL,
    UNIQUE (run_id, position),
    FOREIGN KEY (run_id, position) REFERENCES journal (run_id, position)
);
INSERT INTO mailbox SELECT * FROM migration_9_mailbox;
DROP TABLE migration_9_mailbox;
CREATE TABLE broca_calls (
    send_id TEXT PRIMARY KEY, run_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0), snapshot TEXT NOT NULL,
    UNIQUE (run_id, position),
    FOREIGN KEY (run_id, position) REFERENCES journal (run_id, position) ON DELETE CASCADE
);
INSERT INTO broca_calls SELECT * FROM migration_9_broca;
DROP TABLE migration_9_broca;
ALTER TABLE flows ADD COLUMN scope_health TEXT CHECK (scope_health IS NULL OR json_valid(scope_health));
"#;

/// Why each unknown call's outcome is unknown, from a closed set, recorded
/// when the outcome becomes unknown and required from then on.
///
/// A reason cannot be derived afterwards, so this migration refuses a store
/// that already holds an unknown call. No such store was ever deployed:
/// basal had not shipped, and its test rig builds fresh stores. A dev or rig
/// store that hits the refusal is replaced with a fresh one; there is no
/// backfill and no path for a call without a reason.
const UNKNOWN_REASON: &str = r#"
-- The refusal: the guard's CHECK fails, naming the problem, if any call is
-- already unknown, and the whole migration rolls back.
CREATE TABLE migration_7_guard (
    unknown_calls INTEGER NOT NULL
        CONSTRAINT this_store_holds_unknown_calls_recorded_without_a_reason_replace_it_with_a_fresh_store
        CHECK (unknown_calls = 0)
);
INSERT INTO migration_7_guard (unknown_calls)
    SELECT COUNT(*) FROM journal WHERE dispatch = 'unknown';
DROP TABLE migration_7_guard;

ALTER TABLE journal ADD COLUMN unknown_reason TEXT
    CHECK (unknown_reason IN ('basal_restarted', 'connection_lost', 'reply_timeout',
        'reply_unreadable', 'retries_exhausted', 'provider_lost_run'))
    CHECK (dispatch <> 'unknown' OR unknown_reason IS NOT NULL);
"#;

/// Operator decision cards: the questions only the operator may answer
/// (what happened to a call whose outcome is unknown, whether an
/// auto-disabled flow runs again), raised by basal on core's consent plane.
/// A row is the durable intent to raise its card, written before core is
/// asked, so a crash cannot lose a card, and core's deduplication key makes
/// raising it again after a crash show the same card.
const DECISIONS: &str = r#"
-- The decision card an operator's action came through, when it came
-- through one.
ALTER TABLE audit ADD COLUMN elicitation_id TEXT;

-- One row per card. `instance` tells two occurrences of one decision
-- apart: for a reconcile card, the call's send attempt that ended unknown
-- (a call reconciled as not applied, sent again and lost again needs a new
-- decision); for a re-enable card, the auto-disable episode. `revision`
-- counts changes to what the card shows and `raised_revision` the last
-- one core accepted, so a changed card is raised again under its key.
-- `open` until an answer arrives; then `applied` (an action was taken),
-- `declined` (the do-nothing option), `expired` (nobody answered) or
-- `stale` (the decision had been settled another way, so nothing was
-- done).
CREATE TABLE decision_cards (
    seq             INTEGER PRIMARY KEY AUTOINCREMENT,
    dedup_key       TEXT NOT NULL,
    kind            TEXT NOT NULL CHECK (kind IN ('reconcile', 'reenable')),
    flow_id         TEXT NOT NULL,
    version         INTEGER NOT NULL CHECK (version >= 0),
    run_id          TEXT,
    position        INTEGER CHECK (position >= 0),
    call_key        TEXT,
    instance        INTEGER NOT NULL CHECK (instance >= 0),
    card            TEXT NOT NULL,
    revision        INTEGER NOT NULL DEFAULT 1 CHECK (revision >= 1),
    raised_revision INTEGER,
    elicitation_id  TEXT,
    state           TEXT NOT NULL CHECK (state IN ('open', 'applied', 'declined', 'expired', 'stale')),
    choice          TEXT,
    created_at      INTEGER NOT NULL,
    answered_at     INTEGER,
    UNIQUE (dedup_key, instance),
    CHECK ((kind = 'reconcile') = (run_id IS NOT NULL)),
    CHECK ((run_id IS NULL) = (position IS NULL)),
    CHECK ((run_id IS NULL) = (call_key IS NULL)),
    CHECK ((state = 'open') = (answered_at IS NULL))
);
CREATE UNIQUE INDEX decision_cards_one_open ON decision_cards (dedup_key) WHERE state = 'open';
CREATE INDEX decision_cards_elicitation ON decision_cards (elicitation_id);
"#;

/// Install cards: one consent card per installed version, raised before
/// approval. The card's decision arrives later, possibly after a restart, so
/// the link from a card to the exact version (and code hash) it shows is
/// durable, and a decision is applied at most once.
const CARDS: &str = r#"
-- `pending` until the consent plane answers; `approved` and `rejected`
-- record the decision; `stale` marks an approval that arrived after a newer
-- version of the flow had already been approved, so it changed nothing.
CREATE TABLE install_cards (
    card_id      TEXT PRIMARY KEY,
    flow_id      TEXT NOT NULL,
    version      INTEGER NOT NULL CHECK (version >= 1),
    code_hash    BLOB NOT NULL CHECK (length(code_hash) = 32),
    author       TEXT NOT NULL,
    card         TEXT NOT NULL,
    state        TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'rejected', 'stale')),
    decided_by   TEXT,
    created_at   INTEGER NOT NULL,
    decided_at   INTEGER,
    UNIQUE (flow_id, version),
    FOREIGN KEY (flow_id, version) REFERENCES installs (flow_id, version),
    CHECK ((state = 'pending') = (decided_by IS NULL))
);
"#;

/// Flows, their installed versions and what decides whether a flow may do
/// something, and how much: the call audit, `kv`, token reservations, rate
/// windows and the owner notification outbox.
const DISPATCH: &str = r#"
-- One row per flow id, across its versions: whether it is enabled, which
-- version is approved, and who owns it (the approved version's author).
CREATE TABLE flows (
    flow_id          TEXT PRIMARY KEY,
    owner            TEXT,
    state            TEXT NOT NULL DEFAULT 'enabled' CHECK (state IN ('enabled', 'disabled')),
    approved_version INTEGER,
    disabled_by      TEXT,
    disabled_reason  TEXT,
    disabled_at      INTEGER,
    created_at       INTEGER NOT NULL,
    CHECK ((state = 'disabled') = (disabled_by IS NOT NULL))
);

-- Every validated version. Approval binds to the exact code hash; at most
-- one version per flow is approved at a time, and approving another
-- supersedes it.
CREATE TABLE installs (
    flow_id       TEXT NOT NULL REFERENCES flows (flow_id),
    version       INTEGER NOT NULL CHECK (version >= 1),
    code_hash     BLOB NOT NULL CHECK (length(code_hash) = 32),
    manifest      TEXT NOT NULL,
    script        TEXT NOT NULL,
    author        TEXT NOT NULL,
    loop_override INTEGER NOT NULL DEFAULT 0 CHECK (loop_override IN (0, 1)),
    approval_ref  TEXT,
    state         TEXT NOT NULL CHECK (state IN ('validated', 'approved', 'superseded')),
    installed_at  INTEGER NOT NULL,
    approved_at   INTEGER,
    PRIMARY KEY (flow_id, version),
    CHECK ((state = 'validated') = (approval_ref IS NULL))
);
CREATE UNIQUE INDEX installs_one_approved ON installs (flow_id) WHERE state = 'approved';

-- The version a run was admitted under, its place in its flow's trigger
-- order (the concurrency slot goes to the earliest unfinished run), its
-- wall-clock budget, and the deadline fixed when it first starts.
ALTER TABLE runs ADD COLUMN flow_version INTEGER;
ALTER TABLE runs ADD COLUMN admit_seq INTEGER;
ALTER TABLE runs ADD COLUMN deadline_ms INTEGER;
ALTER TABLE runs ADD COLUMN deadline_at INTEGER;
CREATE INDEX runs_flow_order ON runs (flow_id, admit_seq);

-- The exact bytes sent to the host when they differ from the script's
-- argument bytes: the clamped model request, journaled with the intent so a
-- re-issue sends the same bytes under the same send id.
ALTER TABLE journal ADD COLUMN request TEXT;

-- One row per call, allowed or refused, written in the transaction that
-- journals it, never again (replay serves recorded calls without the
-- parent seeing them).
CREATE TABLE call_audit (
    run_id      TEXT NOT NULL,
    position    INTEGER NOT NULL CHECK (position >= 0),
    flow_id     TEXT NOT NULL,
    op          TEXT NOT NULL,
    args_digest BLOB NOT NULL CHECK (length(args_digest) = 32),
    outcome     TEXT NOT NULL,
    at          INTEGER NOT NULL,
    PRIMARY KEY (run_id, position)
);

-- A flow's own durable state, shared by its versions. Each write commits
-- with its journal row; revision counts the writes to a key.
DROP TABLE local_kv;
DROP TABLE local_effects;
CREATE TABLE kv (
    flow_id  TEXT NOT NULL,
    key      TEXT NOT NULL,
    value    TEXT NOT NULL,
    bytes    INTEGER NOT NULL CHECK (bytes >= 0),
    revision INTEGER NOT NULL CHECK (revision >= 1),
    PRIMARY KEY (flow_id, key)
);

-- One reservation per model call, keyed by its send id. While reserved it
-- counts at its reserved size; once settled, at what was reported (or at
-- its reserved size when nothing was). It stays in the window it was
-- reserved in.
CREATE TABLE token_ledger (
    send_id             TEXT PRIMARY KEY,
    flow_id             TEXT NOT NULL,
    run_id              TEXT NOT NULL,
    position            INTEGER NOT NULL,
    window_ms           INTEGER NOT NULL CHECK (window_ms > 0),
    window_start        INTEGER NOT NULL,
    reserved            INTEGER NOT NULL CHECK (reserved >= 0),
    state               TEXT NOT NULL CHECK (state IN ('reserved', 'settled')),
    input_tokens        INTEGER,
    cache_write_tokens  INTEGER,
    output_tokens       INTEGER,
    cached_input_tokens INTEGER,
    unreported_tokens   INTEGER,
    reserved_at         INTEGER NOT NULL,
    settled_at          INTEGER,
    UNIQUE (run_id, position),
    CHECK ((state = 'settled') = (settled_at IS NOT NULL))
);

-- Per flow and window, the running totals the cap is checked against:
-- outstanding reservations plus settled fresh input, cache write, output
-- and unreported charges. Cached input is kept for display, not capped.
CREATE TABLE token_windows (
    flow_id             TEXT NOT NULL,
    window_ms           INTEGER NOT NULL,
    window_start        INTEGER NOT NULL,
    reserved            INTEGER NOT NULL DEFAULT 0 CHECK (reserved >= 0),
    input_tokens        INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens       INTEGER NOT NULL DEFAULT 0,
    cached_input_tokens INTEGER NOT NULL DEFAULT 0,
    unreported_tokens   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (flow_id, window_ms, window_start)
);

-- Per flow and rate window: runs admitted, calls dispatched, and whether a
-- limit refused something in it (saturated).
CREATE TABLE rate_windows (
    flow_id      TEXT NOT NULL,
    window_ms    INTEGER NOT NULL,
    window_start INTEGER NOT NULL,
    runs         INTEGER NOT NULL DEFAULT 0,
    dispatches   INTEGER NOT NULL DEFAULT 0,
    saturated    INTEGER NOT NULL DEFAULT 0 CHECK (saturated IN (0, 1)),
    PRIMARY KEY (flow_id, window_ms, window_start)
);

-- Notifications for a flow's owner, written in the transaction that
-- decides them. Delivery is a separate concern.
CREATE TABLE outbox (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    at           INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    flow_id      TEXT NOT NULL,
    recipient    TEXT,
    body         TEXT NOT NULL,
    delivered_at INTEGER
);
"#;

#[cfg(test)]
#[path = "schema_flow_deferral_tests.rs"]
mod flow_deferral_tests;

#[cfg(test)]
#[path = "schema_package_tests.rs"]
mod package_tests;

#[cfg(test)]
#[path = "schema_health_tests.rs"]
mod health_tests;

#[cfg(test)]
#[path = "schema_retention_tests.rs"]
mod retention_tests;
