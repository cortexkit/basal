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

pub const MIGRATIONS: &[Migration] = &[Migration {
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
}];
