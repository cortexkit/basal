//! In-memory rehearsal only: this test never opens or migrates a store.

use basal_testkit::cutover::*;
use std::{collections::BTreeMap, convert::Infallible};

fn source_rows(count: u64, detail: &str) -> ObligationRows {
    (0..count)
        .map(|position| (("failed".into(), position), detail.as_bytes().to_vec()))
        .collect()
}

fn fixture() -> Before {
    Before {
        finished: ["success", "failed", "mismatch", "cancelled"]
            .map(String::from)
            .into(),
        live: ["pending", "parked"].map(String::from).into(),
        retained_journal_rows: [
            ("failed".into(), 2),
            ("mismatch".into(), 1),
            ("cancelled".into(), 0),
        ]
        .into(),
        counters: [
            "success",
            "failed",
            "mismatch",
            "cancelled",
            "pending",
            "parked",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, id)| {
            (
                id.into(),
                RunCounters {
                    broken: i as u64,
                    activation_count: i as u64 + 2,
                },
            )
        })
        .collect(),
        accounting: Accounting {
            rate_windows: vec![b"rate:17".to_vec()],
            token_windows: vec![b"token:23".to_vec()],
            token_ledger: vec![b"reserved:11".to_vec()],
            classify_costs: vec![b"reserved:0.3".to_vec()],
        },
        obligation_sources: ObligationSources {
            unsettled_positions: source_rows(1, "outcome:pending"),
            unacknowledged_broca_calls: source_rows(2, "handle:model"),
            reserved_token_rows: source_rows(3, "reservation:11"),
            reserved_dollar_rows: source_rows(4, "reservation:0.3"),
            refusal_outbox_rows: source_rows(5, "refusal:pending"),
            run_cancelled_quarantine_receipts: source_rows(6, "quarantine:cancelled"),
        },
        broca_waits: 7,
        broca_wakes: 8,
        trigger_keys: ["success:t", "failed:t", "mismatch:t", "cancelled:t"]
            .map(String::from)
            .into(),
        write_keys: ["success:w", "failed:w", "mismatch:w", "cancelled:w"]
            .map(String::from)
            .into(),
    }
}

struct Stub {
    before: Before,
    durable_phase: MigrationPhase,
    durable: After,
    lease: CopyLease,
    stopped: bool,
    fault: Fault,
}

#[derive(Clone, Copy, Default)]
enum Fault {
    #[default]
    None,
    MissCrash,
    LoseLease,
    ReleaseAdmission,
    ReleaseActivation,
    SkipPhase,
    BadProjection,
    SameStore,
}

impl CutoverMigration for Stub {
    type Error = Infallible;
    fn copy_lease(&self) -> CopyLease {
        self.lease.clone()
    }
    fn traffic_hold(&self) -> TrafficHold {
        TrafficHold {
            admission: !matches!(self.fault, Fault::ReleaseAdmission),
            activation: !matches!(self.fault, Fault::ReleaseActivation),
        }
    }
    fn before(&self) -> Result<Before, Self::Error> {
        Ok(self.before.clone())
    }
    fn phase(&self) -> Result<MigrationPhase, Self::Error> {
        Ok(self.durable_phase)
    }
    fn advance(&mut self, crash: Option<CrashCut>) -> Result<Advance, Self::Error> {
        assert!(!self.stopped, "must reopen after a crash");
        let phase = self.durable_phase;
        match phase {
            MigrationPhase::Sweep | MigrationPhase::Create | MigrationPhase::PageSize => (),
            MigrationPhase::FinishedRuns => {
                self.durable.history = self.before.finished.iter().cloned().collect();
                self.durable.finished_identities = self.durable.history.clone();
                self.durable.retained_call_rows = self.before.retained_journal_rows.clone();
                self.durable.trigger_tombstones = self.before.trigger_keys.clone();
                self.durable.write_tombstones = self.before.write_keys.clone();
                // All six sources overlap; there are six distinct positions.
                self.durable.open_obligations = 6;
                self.durable.obligation_positions = (0..6).map(|p| ("failed".into(), p)).collect();
                self.durable.obligation_sources = self.before.obligation_sources.clone();
                for id in &self.before.finished {
                    self.durable
                        .counters
                        .insert(id.clone(), self.before.counters[id]);
                }
            }
            MigrationPhase::LiveRuns => {
                self.durable.live = self.before.live.iter().cloned().collect();
                self.durable.first_segment_snapshots = self.durable.live.clone();
                for id in &self.before.live {
                    self.durable
                        .counters
                        .insert(id.clone(), self.before.counters[id]);
                }
            }
            MigrationPhase::BrocaCalls => {
                self.durable.broca_waits = 7;
                self.durable.broca_wakes = 8;
            }
            MigrationPhase::Drop => {
                self.durable.schema_sql = [(
                    "safe_view".into(),
                    "CREATE VIEW safe_view AS SELECT * FROM live_runs".into(),
                )]
                .into();
            }
            MigrationPhase::Finish => {
                if matches!(self.fault, Fault::BadProjection) {
                    self.durable.first_segment_snapshots.clear();
                }
            }
            MigrationPhase::Done => panic!("Done is not a step"),
        }
        // Partial work survives before its phase marker, forcing the resumed
        // stub to redo that phase idempotently rather than merely skip it.
        let cut = crash.filter(|_| !matches!(self.fault, Fault::MissCrash));
        if cut.is_some_and(|c| c.side == CutSide::BeforePhaseCommit) {
            self.stopped = true;
            return Ok(Advance::Crashed);
        }
        let index = MigrationPhase::STEPS
            .iter()
            .position(|p| *p == phase)
            .unwrap();
        self.durable_phase = MigrationPhase::STEPS
            .get(
                index
                    + if matches!(self.fault, Fault::SkipPhase) {
                        2
                    } else {
                        1
                    },
            )
            .copied()
            .unwrap_or(MigrationPhase::Done);
        if cut.is_some() {
            self.stopped = true;
            Ok(Advance::Crashed)
        } else {
            Ok(Advance::Committed)
        }
    }
    fn reopen(&mut self) -> Result<(), Self::Error> {
        self.stopped = false;
        if matches!(self.fault, Fault::LoseLease) {
            self.lease.writer_lease_held = false;
        }
        Ok(())
    }
    fn after(&self) -> Result<After, Self::Error> {
        Ok(self.durable.clone())
    }
}

#[derive(Default)]
struct Copies {
    made: usize,
    fault: Fault,
}
impl MigrationCopies for Copies {
    type Migration = Stub;
    fn fresh_leased_copy(&mut self) -> Result<Stub, Infallible> {
        self.made += 1;
        let before = fixture();
        Ok(Stub {
            durable: After {
                accounting: before.accounting.clone(),
                ..Default::default()
            },
            before,
            durable_phase: MigrationPhase::Sweep,
            stopped: false,
            fault: self.fault,
            lease: CopyLease {
                source: "source".into(),
                copy: if matches!(self.fault, Fault::SameStore) {
                    "source".into()
                } else {
                    format!("copy-{}", self.made)
                },
                writer_lease_held: true,
            },
        })
    }
}

#[test]
fn every_migration_phase_resumes_on_both_sides_of_its_commit_with_hold_checks() {
    let mut copies = Copies::default();
    let covered = rehearse(&mut copies).unwrap();
    assert_eq!(copies.made, 16);
    assert_eq!(covered.len(), 16);
    for phase in MigrationPhase::STEPS {
        for side in [CutSide::BeforePhaseCommit, CutSide::AfterPhaseCommit] {
            assert_eq!(
                covered
                    .iter()
                    .filter(|c| **c == CrashCut { phase, side })
                    .count(),
                1
            );
        }
    }
}

#[test]
fn rehearsal_rejects_missing_crashes_lost_leases_traffic_and_bad_resume() {
    for fault in [
        Fault::MissCrash,
        Fault::LoseLease,
        Fault::ReleaseAdmission,
        Fault::ReleaseActivation,
        Fault::SkipPhase,
        Fault::BadProjection,
        Fault::SameStore,
    ] {
        assert!(rehearse(&mut Copies { made: 0, fault }).is_err());
    }
}

fn completed() -> (Before, After) {
    let mut stub = Copies::default().fresh_leased_copy().unwrap();
    for _ in MigrationPhase::STEPS {
        stub.advance(None).unwrap();
    }
    (stub.before, stub.durable)
}

#[test]
fn hold_point_defends_every_acceptance_comparison() {
    let (before, after) = completed();
    assert!(hold_point(&before, &after).is_empty());
    type Corruption = (&'static str, fn(&mut After));
    let corruptions: &[Corruption] = &[
        ("run_history", |a| {
            a.history[0] = "other".into();
        }),
        ("finished run_identities", |a| {
            a.finished_identities.pop();
        }),
        ("live_runs", |a| {
            a.live[0] = "other".into();
        }),
        ("first-segment Snapshots", |a| {
            a.first_segment_snapshots
                .push(a.first_segment_snapshots[0].clone());
        }),
        ("run_calls", |a| {
            *a.retained_call_rows.get_mut("failed").unwrap() -= 1;
        }),
        ("broken/activation_count", |a| {
            a.counters.get_mut("failed").unwrap().broken = 0;
        }),
        ("broken/activation_count", |a| {
            a.counters.get_mut("parked").unwrap().activation_count = 0;
        }),
        ("rate_windows", |a| {
            a.accounting.rate_windows[0][0] ^= 1;
        }),
        ("token_windows", |a| {
            a.accounting.token_windows[0][0] ^= 1;
        }),
        ("token_ledger", |a| {
            a.accounting.token_ledger[0][0] ^= 1;
        }),
        ("classify_costs", |a| {
            a.accounting.classify_costs[0][0] ^= 1;
        }),
        ("open obligations", |a| {
            a.open_obligations -= 1;
        }),
        ("broca_waits", |a| {
            a.broca_waits -= 1;
        }),
        ("broca_wakes", |a| {
            a.broca_wakes -= 1;
        }),
        ("trigger tombstones", |a| {
            a.trigger_tombstones.pop_first();
        }),
        ("write tombstones", |a| {
            a.write_tombstones.pop_first();
        }),
        ("sqlite_schema", |a| {
            a.schema_sql.insert("bad_trigger".into(), "CREATE TRIGGER bad_trigger AFTER DELETE ON run_history BEGIN DELETE FROM [JoUrNaL]; END".into());
        }),
    ];
    for (expected, corrupt) in corruptions {
        let mut changed = after.clone();
        corrupt(&mut changed);
        let failures = hold_point(&before, &changed);
        assert_eq!(failures.len(), 1, "{expected}: {failures:?}");
        assert!(failures[0].contains(expected), "{failures:?}");
    }
    let mut missing_counter = before.clone();
    missing_counter.counters = BTreeMap::new();
    assert!(
        hold_point(&missing_counter, &after)
            .iter()
            .any(|s| s.contains("inventory"))
    );
}

#[test]
fn hold_point_checks_each_overlapping_obligation_source_and_its_detail() {
    let (before, after) = completed();
    assert_eq!(before.obligation_sources.positions().len(), 6);
    assert_eq!(
        before.obligation_sources.counts().values().sum::<usize>(),
        21
    );
    for source in 0..6 {
        let mut changed = after.clone();
        let fields = [
            &mut changed.obligation_sources.unsettled_positions,
            &mut changed.obligation_sources.unacknowledged_broca_calls,
            &mut changed.obligation_sources.reserved_token_rows,
            &mut changed.obligation_sources.reserved_dollar_rows,
            &mut changed.obligation_sources.refusal_outbox_rows,
            &mut changed.obligation_sources.run_cancelled_quarantine_receipts,
        ];
        let rows = fields.into_iter().nth(source).unwrap();
        rows.first_entry().unwrap().get_mut().push(b'!');
        let failures = hold_point(&before, &changed);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains(before.obligation_sources.rows()[source].0));
        assert!(failures[0].ends_with("detail differs"));
    }
    let mut changed = after.clone();
    changed
        .obligation_sources
        .unacknowledged_broca_calls
        .clear();
    // Other sources still cover both positions, and the row count is unchanged.
    assert_eq!(changed.open_obligations, 6);
    assert_eq!(
        hold_point(&before, &changed),
        ["open obligations: unacknowledged broca_calls detail differs"]
    );
    let mut changed = after;
    changed.obligation_positions.remove(&("failed".into(), 0));
    changed.obligation_positions.insert(("other".into(), 0));
    assert_eq!(
        hold_point(&before, &changed),
        ["open obligations: source union differs"]
    );
}

#[test]
fn schema_hold_check_catches_all_dropped_names_and_quoting_forms() {
    let (before, mut after) = completed();
    after.schema_sql.insert("safe".into(), "CREATE VIEW safe AS SELECT 'runs', 'journal' FROM run_history -- mailbox\n/* activations */".into());
    assert!(hold_point(&before, &after).is_empty());
    for table in [
        "runs",
        "journal",
        "journal_payloads",
        "mailbox",
        "call_audit",
        "activations",
        "event_body_journal",
        "journal_with_payloads",
        "runs_with_snapshots",
    ] {
        for quoted in [
            table.into(),
            format!("\"{table}\""),
            format!("[{table}]"),
            format!("`{table}`"),
        ] {
            after.schema_sql.insert(
                "bad".into(),
                format!("CREATE VIEW bad AS SELECT * FROM {quoted}"),
            );
            assert_eq!(
                hold_point(&before, &after),
                [format!("sqlite_schema bad: references {table}")]
            );
        }
    }
}
