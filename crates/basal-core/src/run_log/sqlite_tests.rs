use super::*;
use crate::{
    Durability, Store,
    run_store_sql::{self as sql, EpochLsn, GcFloor, Meta},
};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RunFact {
    run_id: String,
    facts: Facts,
}
impl RunFact {
    fn live() -> Self {
        Self {
            run_id: "live".into(),
            facts: Facts::committed(),
        }
    }
}
impl LogRecord for RunFact {
    fn record_type(&self) -> u8 {
        3
    }
    fn decode(kind: u8, payload: &[u8]) -> Result<Self> {
        if kind != 3 {
            return Err(Error::Corrupt("unknown test record"));
        }
        format::decode(payload)
    }
    fn run_id(&self) -> Option<&str> {
        Some(&self.run_id)
    }
}

struct Fixture {
    scratch: Scratch,
    store: Arc<Store>,
    registry: Arc<SqliteRegistry>,
}
impl Fixture {
    fn new() -> Self {
        let scratch = Scratch::new();
        let store =
            Arc::new(Store::open(scratch.0.join("store.db"), Durability::default()).unwrap());
        let state = Meta {
            store_id: store.store_id().into(),
            log_epoch: 3,
            log_hwm: EpochLsn {
                log_epoch: 3,
                lsn: Lsn::default(),
            },
            commit_seq: 0,
            gc_floor: GcFloor {
                log_epoch: 3,
                first_retained: 0,
                predecessor: None,
                synced_lsn: Lsn::default(),
                live_starts: BTreeMap::new(),
            },
            migration_phase: "test".into(),
        };
        store.write(|tx| {
            sql::create_test_schema(tx, &state)?;
            tx.execute("INSERT INTO run_snapshots(snapshot_id,flow_id,flow_version,code_hash,script,manifest)
                VALUES(1,'flow',1,?1,'return 1','{}')", [[42u8;32].as_slice()])?;
            Ok(())
        }).unwrap();
        let registry = Arc::new(SqliteRegistry::new(store.clone(), 3));
        Self {
            scratch,
            store,
            registry,
        }
    }
    fn config(&self, hooks: Arc<dyn Hooks>) -> Config {
        let mut config = self.scratch.config(hooks, self.store.lease_epoch());
        config.identity.store_id = self.store.store_id().as_bytes().try_into().unwrap();
        config
    }
    fn admit(&self, id: &str, lsn: Lsn) {
        self.store
            .write(|tx| {
                sql::insert_identity(
                    tx,
                    &sql::RunIdentity {
                        run_id: id.into(),
                        flow_id: "flow".into(),
                        flow_version: 1,
                        code_hash: [42; 32],
                        snapshot_id: 1,
                    },
                )?;
                sql::insert_live(
                    tx,
                    &sql::LiveRun {
                        run_id: id.into(),
                        flow_id: "flow".into(),
                        flow_version: 1,
                        trigger_id: id.into(),
                        trigger_kind: "manual".into(),
                        trigger: json!({}),
                        attempt: 0,
                        admitted_at: 10,
                        admit_seq: 1,
                        admit_lsn: EpochLsn { log_epoch: 3, lsn },
                        deadline_ms: 100,
                        self_input: json!({}),
                        params_input: json!({}),
                        grant_guards: json!([]),
                        template_incarnation: None,
                        end_instance: false,
                    },
                )
            })
            .unwrap();
    }
    fn meta(&self) -> Meta {
        self.store.read(sql::meta).unwrap()
    }
}

fn history(id: &str) -> sql::RunHistory {
    sql::RunHistory {
        run_id: id.into(),
        flow_id: "flow".into(),
        flow_version: 1,
        trigger_id: id.into(),
        trigger_kind: "manual".into(),
        trigger: json!({}),
        admitted_at: 10,
        admit_seq: 1,
        first_claim_at: Some(11),
        ended_at: 20,
        state: crate::RunState::Succeeded,
        error_kind: None,
        error_detail: None,
        result: None,
        result_size: 0,
        call_counts: json!({}),
        writes_sent: 1,
        tokens_charged: 4,
        cost_microusd: 5,
        broken: 2,
        activation_count: 3,
        script_started: true,
        phase: "script".into(),
        deadline_exempt: false,
        history_lost: false,
        commit_seq: 1,
    }
}

#[test]
fn sqlite_gc_floor_commit_precedes_unlink_and_recovers_at_every_boundary() {
    for boundary in Boundary::RUN_LOG_GC {
        gc_crash_case(boundary);
    }
}

pub(crate) fn gc_crash_case(boundary: Boundary) {
    let f = Fixture::new();
    let cut = Cut::new();
    let log = RunLog::<RunFact>::create(f.config(cut.clone()), f.registry.clone()).unwrap();
    f.admit("live", log.synced_lsn());
    f.admit("finished", log.synced_lsn());
    log.append(vec![
        RunFact::live(),
        RunFact {
            run_id: "finished".into(),
            facts: Facts::committed(),
        },
    ])
    .unwrap();
    log.rotate().unwrap();
    log.append(vec![RunFact::live()]).unwrap();
    log.rotate().unwrap();
    let start = log.append(vec![RunFact::live()]).unwrap();
    log.sync_through(start).unwrap();
    sql::write_synced(&f.store, &log.watermark(), |tx| {
        sql::finish_live(tx, &history("finished"))
    })
    .unwrap();
    cut.arm(boundary.clone());
    assert_eq!(
        log.reclaim(BTreeMap::from([("live".into(), start)])),
        Err(Error::Cut)
    );
    let meta = f.meta();
    assert_eq!(meta.gc_floor.first_retained, 2, "{boundary:?}");
    assert_eq!(meta.gc_floor.predecessor.unwrap().lsn.sequence, 1);
    assert_eq!(
        meta.gc_floor.live_starts,
        BTreeMap::from([("live".into(), start)])
    );
    assert_eq!(meta.gc_floor.synced_lsn, start);
    assert_eq!(meta.log_hwm.lsn, start);
    assert_eq!(f.registry.snapshot().unwrap().first_retained, 2);
    assert!(!f.registry.snapshot().unwrap().segments.contains_key(&0));
    assert!(!f.registry.snapshot().unwrap().segments.contains_key(&1));
    let present = (f.scratch.file(0).exists(), f.scratch.file(1).exists());
    assert_eq!(
        present,
        match boundary {
            Boundary::RunLogGcCommitted => (true, true),
            Boundary::RunLogGcUnlinked => (false, true),
            Boundary::RunLogGcSynced => (false, false),
            _ => panic!("new GC boundary needs crash assertions"),
        }
    );
    drop(log);
    for _ in 0..2 {
        let (log, recovered) =
            RunLog::<RunFact>::open(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
        assert_eq!(recovered.records.len(), 1);
        // Compare recovery to the literal values in Facts::committed, not
        // values derived from the recovered log: deadlines, broken counts,
        // budgets, retry keys and acknowledged outcomes must survive GC.
        assert_eq!(recovered.records[0].record, RunFact::live());
        assert!(!f.scratch.file(0).exists());
        assert!(!f.scratch.file(1).exists());
        assert_eq!(f.meta(), meta);
        assert!(
            !f.store
                .read(|c| Ok(sql::history(c, "finished")?.unwrap().history_lost))
                .unwrap()
        );
        drop(log);
    }
}

#[test]
fn sqlite_gc_requires_committed_finished_runs_and_durable_live_starts() {
    let f = Fixture::new();
    let log = RunLog::<RunFact>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    f.admit("live", log.synced_lsn());
    log.append(vec![
        RunFact::live(),
        RunFact {
            run_id: "finished".into(),
            facts: Facts::committed(),
        },
    ])
    .unwrap();
    log.rotate().unwrap();
    let start = log.append(vec![RunFact::live()]).unwrap();
    assert_eq!(
        log.reclaim(BTreeMap::from([("live".into(), start)]))
            .unwrap(),
        0
    );
    log.sync_through(start).unwrap();
    assert_eq!(log.reclaim(BTreeMap::new()).unwrap(), 0);
    assert_eq!(f.meta().log_hwm.lsn, start);
    assert_eq!(
        log.reclaim(BTreeMap::from([("live".into(), start)]))
            .unwrap(),
        0
    );
    assert!(f.scratch.file(0).exists());
    f.admit("finished", log.synced_lsn());
    sql::write_synced(&f.store, &log.watermark(), |tx| {
        sql::finish_live(tx, &history("finished"))
    })
    .unwrap();
    let starts = BTreeMap::from([("live".into(), start)]);
    assert_eq!(log.reclaim(starts.clone()).unwrap(), 1);
    assert_eq!(log.reclaim(starts).unwrap(), 0);
    assert!(f.scratch.file(1).exists());
    assert_eq!(f.meta().gc_floor.first_retained, 1);
}

#[test]
fn sqlite_gc_rolls_back_floor_with_registry_deletion_and_rebuilds_after_restart() {
    let f = Fixture::new();
    let log = RunLog::<RunFact>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    f.admit("live", log.synced_lsn());
    log.append(vec![RunFact::live()]).unwrap();
    log.rotate().unwrap();
    let start = log.append(vec![RunFact::live()]).unwrap();
    log.sync_through(start).unwrap();
    f.store
        .write(|tx| {
            tx.execute_batch(
                "CREATE TEMP TRIGGER fail_gc BEFORE DELETE ON run_log_segments
            BEGIN SELECT RAISE(ABORT, 'injected GC delete failure'); END",
            )?;
            Ok(())
        })
        .unwrap();
    let before = f.meta();
    assert!(
        log.reclaim(BTreeMap::from([("live".into(), start)]))
            .is_err()
    );
    assert_eq!(f.meta(), before);
    assert!(f.registry.snapshot().unwrap().segments.contains_key(&0));
    assert!(f.scratch.file(0).exists());
    drop(log);
    f.store
        .write(|tx| {
            tx.execute_batch("DROP TRIGGER fail_gc")?;
            Ok(())
        })
        .unwrap();
    let (log, recovered) =
        RunLog::<RunFact>::open(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    assert_eq!(recovered.records.len(), 2);
    assert_eq!(
        log.reclaim(BTreeMap::from([("live".into(), start)]))
            .unwrap(),
        1
    );
    assert!(!f.scratch.file(0).exists());
    drop(log);
    let (_log, recovered) =
        RunLog::<RunFact>::open(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    assert_eq!(recovered.records.len(), 1);
    assert_eq!(recovered.records[0].record, RunFact::live());
}

#[test]
fn sqlite_gc_accepts_pruned_history_only_with_a_committed_tombstone() {
    let f = Fixture::new();
    let log = RunLog::<RunFact>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    f.admit("live", log.synced_lsn());
    log.append(vec![RunFact::live()]).unwrap();
    log.rotate().unwrap();
    sql::write_synced(&f.store, &log.watermark(), |tx| {
        sql::finish_live(tx, &history("live"))?;
        tx.execute("INSERT INTO tombstones(kind,key,run_id,pruned_at) VALUES('trigger','flow:live','live',30)", [])?;
        sql::prune_history(tx, "live")?;
        Ok(())
    }).unwrap();
    assert!(
        f.store
            .read(|c| Ok(sql::history(c, "live")?.is_none()))
            .unwrap()
    );
    assert_eq!(log.reclaim(BTreeMap::new()).unwrap(), 1);
    assert!(!f.scratch.file(0).exists());
}

struct Terminal;
impl crate::run_store::TerminalBuilder<crate::run_records::Records> for Terminal {
    fn is_success(&self) -> bool {
        true
    }
    fn build(
        &self,
        id: &str,
        _: &crate::run_records::Records,
        _: &[crate::run_store::Reservation],
    ) -> crate::Result<Box<dyn crate::run_store::TerminalWrite>> {
        Ok(Box::new(TerminalRows(id.into())))
    }
}
struct TerminalRows(String);
impl crate::run_store::TerminalWrite for TerminalRows {
    fn apply(&self, tx: &rusqlite::Transaction<'_>) -> crate::Result<()> {
        sql::finish_live(tx, &history(&self.0))?;
        sql::insert_obligation(
            tx,
            &sql::Obligation {
                run_id: self.0.clone(),
                position: 1,
                handle: Some("handle".into()),
                state: sql::ObligationState::Open,
                audit_at: 11,
                detail: json!({}),
                receipt: None,
                settled_at: None,
            },
        )
    }
}
struct Driver;
impl crate::run_store::CloseDriver for Driver {
    fn stop_worker(&self, _: &str) -> crate::Result<()> {
        Ok(())
    }
    fn closed(&self, _: &str) {}
}

#[test]
fn sqlite_terminal_and_closed_obligation_commit_current_watermark() {
    use crate::{
        run_records::{Body, Record, Records},
        run_store::RunStore,
    };
    let f = Fixture::new();
    let log = Arc::new(
        RunLog::<Record>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap(),
    );
    f.admit("live", log.synced_lsn());
    let runs = RunStore::new_bound(
        f.store.clone(),
        Arc::new(NoHooks),
        Arc::new(log.watermark()),
    );
    let run = runs
        .insert(
            "live".into(),
            "flow".into(),
            1,
            Records::new("live".into(), 10, log.clone()),
        )
        .unwrap();
    let cut = run
        .activate(1, |a| {
            a.records().append(
                1,
                Body::Claimed {
                    first_claim_ms: 11,
                    deadline_at: 1234,
                },
            )?;
            a.records().append(1, Body::Activated)
        })
        .unwrap();
    log.sync_through(cut).unwrap();
    assert!(f.meta().log_hwm.lsn < cut);
    assert_eq!(runs.reclaim_starts().len(), 1);
    runs.close(&run, &Terminal, &Driver).unwrap();
    assert_eq!(
        f.meta().log_hwm,
        EpochLsn {
            log_epoch: 3,
            lsn: cut
        }
    );
    let later = log
        .append(vec![Record {
            run_id: "other".into(),
            generation: 1,
            body: Body::Claimed {
                first_claim_ms: 12,
                deadline_at: 2345,
            },
        }])
        .unwrap();
    log.sync_through(later).unwrap();
    assert_eq!(
        runs.accounting_write("flow", &f.store, |tx| {
            sql::settle_obligation(
                tx,
                "live",
                1,
                Some("handle"),
                sql::Receipt::Value([9; 32]),
                30,
            )
        })
        .unwrap(),
        sql::SettleResult::Settled
    );
    assert_eq!(
        f.meta().log_hwm,
        EpochLsn {
            log_epoch: 3,
            lsn: later
        }
    );
    let stale = Watermark {
        epoch: 2,
        synced: Arc::new(Mutex::new(later)),
    };
    assert!(
        sql::write_synced(&f.store, &stale, |tx| {
            tx.execute("DELETE FROM obligations", [])?;
            Ok(())
        })
        .is_err()
    );
    assert!(
        f.store
            .read(|c| Ok(sql::obligation(c, "live", 1)?.is_some()))
            .unwrap()
    );
}

#[test]
fn sqlite_rotation_promotes_after_link_before_watermark_and_repairs_reserved_link() {
    for boundary in [Boundary::RunLogLinked, Boundary::RunLogPromoted] {
        let f = Fixture::new();
        let cut = Cut::new();
        let log = RunLog::<RunFact>::create(f.config(cut.clone()), f.registry.clone()).unwrap();
        let old = log.synced_lsn();
        log.append(vec![RunFact::live()]).unwrap();
        cut.arm(boundary.clone());
        assert_eq!(log.rotate(), Err(Error::Cut));
        assert_eq!(log.synced_lsn(), old);
        let state = f.registry.snapshot().unwrap();
        assert_eq!(
            state.segments[&1],
            if boundary == Boundary::RunLogLinked {
                SegmentState::Reserved
            } else {
                SegmentState::Active
            }
        );
        if boundary == Boundary::RunLogLinked {
            assert_eq!(state.log_hwm, old);
        } else {
            assert_eq!(state.log_hwm.sequence, 1);
        }
        // The Link really exists on disk before either SQLite state is seen.
        let mut file = File::open(f.scratch.file(1)).unwrap();
        Header::read(&mut file).unwrap();
        assert_eq!(format::read_frame(&mut file).unwrap().unwrap().kind, LINK);
        drop(log);
        let (log, recovered) =
            RunLog::<RunFact>::open(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
        assert_eq!(recovered.records[0].record, RunFact::live());
        assert_eq!(
            f.registry.snapshot().unwrap().segments[&1],
            SegmentState::Active
        );
        assert_eq!(f.meta().log_hwm.lsn.sequence, 1);
        drop(log);
    }
}

#[test]
fn sqlite_registry_fences_each_mutation_and_filters_other_epochs() {
    let f = Fixture::new();
    let log = RunLog::<RunFact>::create(f.config(Arc::new(NoHooks)), f.registry.clone()).unwrap();
    let lease = f.store.lease_epoch();
    assert_eq!(f.registry.reserve(lease + 1, 10), Err(Error::Fenced));
    assert_eq!(
        f.registry.promote(
            lease + 1,
            1,
            Lsn {
                sequence: 1,
                offset: 100
            }
        ),
        Err(Error::Fenced)
    );
    assert_eq!(
        f.registry
            .reclaim(lease + 1, &[], log.synced_lsn(), &BTreeMap::new()),
        Err(Error::Fenced)
    );
    f.store
        .write(|tx| {
            sql::reserve_segment(tx, 2, 10)?;
            Ok(())
        })
        .unwrap();
    assert!(!f.registry.snapshot().unwrap().segments.contains_key(&10));
    assert!(SqliteRegistry::new(f.store.clone(), 2).snapshot().is_err());
    assert!(
        f.registry
            .promote(
                lease,
                99,
                Lsn {
                    sequence: 99,
                    offset: 100
                }
            )
            .is_err()
    );
    f.store
        .write(|tx| {
            tx.execute("UPDATE cortexkit_fence SET epoch=?1", [lease + 1])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(f.registry.check_lease(lease), Err(Error::Fenced));
    assert!(f.registry.reserve(lease, 10).is_err());
    assert!(
        f.registry
            .promote(
                lease,
                1,
                Lsn {
                    sequence: 1,
                    offset: 100
                }
            )
            .is_err()
    );
    assert!(
        f.registry
            .reclaim(lease, &[], log.synced_lsn(), &BTreeMap::new())
            .is_err()
    );
}

#[path = "restore_tests.rs"]
pub(crate) mod restore;
