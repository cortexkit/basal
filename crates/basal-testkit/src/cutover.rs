//! Cutover rehearsal contract, without a production migration implementation.
//!
//! A future migration adapter must operate on a fresh, isolated, leased copy for
//! every case. Rehearsal never releases the admission/activation hold or starts
//! traffic. Running an adapter against a live-store copy requires the operator's
//! separately recorded migration approval, backup and rollback plan.

use std::collections::{BTreeMap, BTreeSet};

pub use basal_core::run_cutover::Phase as MigrationPhase;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CutSide {
    BeforePhaseCommit,
    AfterPhaseCommit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrashCut {
    pub phase: MigrationPhase,
    pub side: CutSide,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advance {
    Committed,
    Crashed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyLease {
    /// Stable source/copy identities, not merely two spellings of one path.
    pub source: String,
    pub copy: String,
    pub writer_lease_held: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrafficHold {
    pub admission: bool,
    pub activation: bool,
}

/// Named seam for the later resumable migration. No production adapter is
/// supplied here. `phase` is the next step, decoded from the durable marker.
pub trait CutoverMigration {
    type Error;
    fn copy_lease(&self) -> CopyLease;
    fn traffic_hold(&self) -> TrafficHold;
    fn before(&self) -> Result<Before, Self::Error>;
    fn phase(&self) -> Result<MigrationPhase, Self::Error>;
    /// Execute one idempotent phase. A requested crash stops the adapter;
    /// reopening must redo durable partial work if its marker did not commit.
    fn advance(&mut self, crash: Option<CrashCut>) -> Result<Advance, Self::Error>;
    /// Drop all process state, reacquire the copy's lease, read its marker and
    /// perform normal migration recovery, without starting the runtime.
    fn reopen(&mut self) -> Result<(), Self::Error>;
    fn after(&self) -> Result<After, Self::Error>;
}

pub trait MigrationCopies {
    type Migration: CutoverMigration;
    fn fresh_leased_copy(
        &mut self,
    ) -> Result<Self::Migration, <Self::Migration as CutoverMigration>::Error>;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunCounters {
    pub broken: u64,
    pub activation_count: u64,
}

/// Complete rows, canonically encoded and sorted by the adapter. Equality must
/// include all columns, not just row counts or a projection of numeric totals.
pub type TableImage = Vec<Vec<u8>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accounting {
    pub rate_windows: TableImage,
    pub token_windows: TableImage,
    pub token_ledger: TableImage,
    pub classify_costs: TableImage,
}

/// Source rows keyed by run and position, with canonical full source detail.
pub type ObligationRows = BTreeMap<(String, u64), Vec<u8>>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObligationSources {
    pub unsettled_positions: ObligationRows,
    pub unacknowledged_broca_calls: ObligationRows,
    pub reserved_token_rows: ObligationRows,
    pub reserved_dollar_rows: ObligationRows,
    pub refusal_outbox_rows: ObligationRows,
    pub run_cancelled_quarantine_receipts: ObligationRows,
}

impl ObligationSources {
    pub fn rows(&self) -> [(&str, &ObligationRows); 6] {
        [
            ("unsettled positions", &self.unsettled_positions),
            (
                "unacknowledged broca_calls",
                &self.unacknowledged_broca_calls,
            ),
            ("reserved token rows", &self.reserved_token_rows),
            ("reserved dollar rows", &self.reserved_dollar_rows),
            ("refusal outbox rows", &self.refusal_outbox_rows),
            (
                "run_cancelled quarantine receipts",
                &self.run_cancelled_quarantine_receipts,
            ),
        ]
    }

    /// Report each source separately so overlapping reservations remain visible.
    pub fn counts(&self) -> BTreeMap<&str, usize> {
        self.rows()
            .into_iter()
            .map(|(name, rows)| (name, rows.len()))
            .collect()
    }

    pub fn positions(&self) -> BTreeSet<(String, u64)> {
        self.rows()
            .into_iter()
            .flat_map(|(_, rows)| rows.keys().cloned())
            .collect()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Before {
    pub finished: BTreeSet<String>,
    pub live: BTreeSet<String>,
    /// Journal counts per failed, cancelled or worker-engine-incompatible
    /// (`engine_mismatch`) run, including runs with zero calls. Successful runs
    /// must not appear in this map.
    pub retained_journal_rows: BTreeMap<String, u64>,
    pub counters: BTreeMap<String, RunCounters>,
    pub accounting: Accounting,
    pub obligation_sources: ObligationSources,
    pub broca_waits: u64,
    pub broca_wakes: u64,
    /// Full trigger/write dedup keys for every finished run.
    pub trigger_keys: BTreeSet<String>,
    pub write_keys: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct After {
    // Vectors preserve duplicates, so duplicate Snapshot records cannot hide
    // behind an apparently correct set of run ids.
    pub history: Vec<String>,
    pub finished_identities: Vec<String>,
    pub live: Vec<String>,
    pub first_segment_snapshots: Vec<String>,
    pub retained_call_rows: BTreeMap<String, u64>,
    pub counters: BTreeMap<String, RunCounters>,
    pub accounting: Accounting,
    pub open_obligations: u64,
    pub obligation_positions: BTreeSet<(String, u64)>,
    /// Independently extracted source details from the migrated obligations.
    pub obligation_sources: ObligationSources,
    pub broca_waits: u64,
    pub broca_wakes: u64,
    pub trigger_tombstones: BTreeSet<String>,
    pub write_tombstones: BTreeSet<String>,
    /// Every view and trigger's SQL from sqlite_schema, keyed by object name.
    pub schema_sql: BTreeMap<String, String>,
}

/// All failures are reported together; none of the checks releases the hold.
pub fn hold_point(before: &Before, after: &After) -> Vec<String> {
    let mut failures = Vec::new();
    let same_ids = |rows: &[String], expected: &BTreeSet<String>| {
        rows.len() == expected.len() && rows.iter().cloned().collect::<BTreeSet<_>>() == *expected
    };
    for (name, rows, expected) in [
        ("run_history", &after.history, &before.finished),
        (
            "finished run_identities",
            &after.finished_identities,
            &before.finished,
        ),
        ("live_runs", &after.live, &before.live),
        (
            "first-segment Snapshots",
            &after.first_segment_snapshots,
            &before.live,
        ),
    ] {
        if !same_ids(rows, expected) {
            failures.push(format!("{name}: run ids/count differ"));
        }
    }
    if before.retained_journal_rows != after.retained_call_rows {
        failures.push("run_calls: per-run journal counts differ".into());
    }
    let all_runs: BTreeSet<_> = before.finished.union(&before.live).cloned().collect();
    if !before.finished.is_disjoint(&before.live)
        || before.counters.keys().cloned().collect::<BTreeSet<_>>() != all_runs
    {
        failures.push("before: incomplete or overlapping run inventory".into());
    }
    if before.counters != after.counters {
        failures.push("broken/activation_count: per-run values differ".into());
    }
    for (name, old, new) in [
        (
            "rate_windows",
            &before.accounting.rate_windows,
            &after.accounting.rate_windows,
        ),
        (
            "token_windows",
            &before.accounting.token_windows,
            &after.accounting.token_windows,
        ),
        (
            "token_ledger",
            &before.accounting.token_ledger,
            &after.accounting.token_ledger,
        ),
        (
            "classify_costs",
            &before.accounting.classify_costs,
            &after.accounting.classify_costs,
        ),
    ] {
        if old != new {
            failures.push(format!("{name}: rows changed"));
        }
    }
    // One position can owe journal, Broca and reservation work simultaneously.
    // Count their union, not their sum, then compare every source independently:
    // equal totals alone could hide a dropped handle, reservation or outcome.
    let positions = before.obligation_sources.positions();
    if after.open_obligations != positions.len() as u64 || after.obligation_positions != positions {
        failures.push("open obligations: source union differs".into());
    }
    for ((name, expected), (_, actual)) in before
        .obligation_sources
        .rows()
        .into_iter()
        .zip(after.obligation_sources.rows())
    {
        if expected != actual {
            failures.push(format!("open obligations: {name} detail differs"));
        }
    }
    if before.broca_waits != after.broca_waits {
        failures.push("broca_waits: count changed".into());
    }
    if before.broca_wakes != after.broca_wakes {
        failures.push("broca_wakes: count changed".into());
    }
    if !before.trigger_keys.is_subset(&after.trigger_tombstones) {
        failures.push("trigger tombstones: missing finished-run keys".into());
    }
    if !before.write_keys.is_subset(&after.write_tombstones) {
        failures.push("write tombstones: missing finished-run keys".into());
    }
    for (name, sql) in &after.schema_sql {
        if let Some(table) = dropped_reference(sql) {
            failures.push(format!("sqlite_schema {name}: references {table}"));
        }
    }
    failures
}

// Conservative identifier scan: literals/comments are skipped, quoted names
// are retained, and aliases matching a dropped name also keep the hold closed.
fn dropped_reference(sql: &str) -> Option<String> {
    let mut chars = sql.chars().peekable();
    let mut identifiers = Vec::new();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            while let Some(c) = chars.next() {
                if c == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        } else if ch == '-' && chars.peek() == Some(&'-') {
            for c in chars.by_ref() {
                if c == '\n' {
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            let mut word = String::from(ch);
            while chars
                .peek()
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                word.push(chars.next().unwrap());
            }
            identifiers.push(word.to_ascii_lowercase());
        }
    }
    identifiers.into_iter().find(|word| {
        [
            "runs",
            "journal",
            "journal_payloads",
            "mailbox",
            "call_audit",
            "activations",
            "event_body_journal",
            "journal_with_payloads",
            "runs_with_snapshots",
        ]
        .contains(&word.as_str())
    })
}

#[derive(Debug)]
pub enum RehearsalError<E> {
    Adapter(E),
    Contract(&'static str),
    Hold(Vec<String>),
}

/// Exercise both sides of every phase commit on separate copies, then reopen a
/// completed migration once more to verify that Done is stable. No store I/O is
/// performed by the harness itself; all current callers use an in-memory stub.
pub fn rehearse<F: MigrationCopies>(
    factory: &mut F,
) -> Result<Vec<CrashCut>, RehearsalError<<F::Migration as CutoverMigration>::Error>> {
    use RehearsalError::{Adapter, Contract, Hold};
    let mut covered = Vec::new();
    for phase in MigrationPhase::STEPS {
        for side in [CutSide::BeforePhaseCommit, CutSide::AfterPhaseCommit] {
            let cut = CrashCut { phase, side };
            let mut migration = factory.fresh_leased_copy().map_err(Adapter)?;
            let identity = migration.copy_lease();
            if identity.source == identity.copy || !identity.writer_lease_held {
                return Err(Contract("needs an isolated copy with its writer lease"));
            }
            let before = migration.before().map_err(Adapter)?;
            for step in MigrationPhase::STEPS {
                if migration.phase().map_err(Adapter)? != step {
                    return Err(Contract("phase marker skipped or regressed"));
                }
                check_hold(&migration).map_err(Contract)?;
                let crashed = step == phase;
                let result = migration.advance(crashed.then_some(cut)).map_err(Adapter)?;
                if crashed {
                    if result != Advance::Crashed {
                        return Err(Contract("requested crash was not reached"));
                    }
                    migration.reopen().map_err(Adapter)?;
                    if migration.copy_lease() != identity {
                        return Err(Contract("reopen changed copy identity or lost lease"));
                    }
                    let expected = if side == CutSide::BeforePhaseCommit {
                        step
                    } else {
                        step.next()
                    };
                    if migration.phase().map_err(Adapter)? != expected {
                        return Err(Contract("wrong durable phase after crash"));
                    }
                    break;
                }
                if result != Advance::Committed {
                    return Err(Contract("unexpected crash"));
                }
            }
            for _ in 0..=MigrationPhase::STEPS.len() {
                check_hold(&migration).map_err(Contract)?;
                let step = migration.phase().map_err(Adapter)?;
                if step == MigrationPhase::Done {
                    break;
                }
                if migration.advance(None).map_err(Adapter)? != Advance::Committed
                    || migration.phase().map_err(Adapter)? != step.next()
                {
                    return Err(Contract("resume failed to advance exactly one phase"));
                }
            }
            if migration.phase().map_err(Adapter)? != MigrationPhase::Done {
                return Err(Contract("resume did not finish"));
            }
            let after = migration.after().map_err(Adapter)?;
            let failures = hold_point(&before, &after);
            if !failures.is_empty() {
                return Err(Hold(failures));
            }
            migration.reopen().map_err(Adapter)?;
            check_hold(&migration).map_err(Contract)?;
            if migration.copy_lease() != identity
                || migration.phase().map_err(Adapter)? != MigrationPhase::Done
                || migration.after().map_err(Adapter)? != after
            {
                return Err(Contract("repeat boot changed completed migration"));
            }
            covered.push(cut);
        }
    }
    Ok(covered)
}

fn check_hold<M: CutoverMigration>(migration: &M) -> Result<(), &'static str> {
    let hold = migration.traffic_hold();
    if !hold.admission || !hold.activation {
        Err("admission and activation must remain held")
    } else {
        Ok(())
    }
}
