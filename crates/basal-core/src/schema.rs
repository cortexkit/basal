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
];

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
