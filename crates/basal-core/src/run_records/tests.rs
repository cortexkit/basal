use super::*;
use crate::{
    Boundary, Durability, Hooks, NoHooks, Step, Store, model::StoredClass, run_store::RunStore,
};
use basal_proto::{ArgsDigest, CallKind, HostCall, JsonText, Primitive, Settlement};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, atomic::Ordering},
};

#[derive(Default)]
struct Memory {
    frames: Mutex<Vec<(Lsn, Record)>>,
    syncs: Mutex<Vec<Lsn>>,
    fail_append: std::sync::atomic::AtomicBool,
    fail_sync: std::sync::atomic::AtomicBool,
}
impl RecordLog for Memory {
    fn reclaim(&self, _: BTreeMap<String, Lsn>) -> Result<usize> {
        Ok(0)
    }
    fn append(&self, record: Record) -> Result<Lsn> {
        if self.fail_append.load(Ordering::SeqCst) {
            return Err(CoreError::Cut);
        }
        let mut frames = self.frames.lock().unwrap();
        let lsn = Lsn {
            sequence: 0,
            offset: frames.len() as u64 + 1,
        };
        frames.push((lsn, record));
        Ok(lsn)
    }
    fn sync_through(&self, cut: Lsn) -> Result<()> {
        if self.fail_sync.load(Ordering::SeqCst) {
            return Err(CoreError::Cut);
        }
        self.syncs.lock().unwrap().push(cut);
        Ok(())
    }
}
impl Memory {
    fn records(&self, durable_only: bool) -> Vec<RecoveredRecord<Record>> {
        let synced = self
            .syncs
            .lock()
            .unwrap()
            .iter()
            .copied()
            .max()
            .unwrap_or_default();
        self.frames
            .lock()
            .unwrap()
            .iter()
            .filter(|(lsn, _)| !durable_only || *lsn <= synced)
            .map(|(lsn, record)| RecoveredRecord {
                lsn: *lsn,
                record: record.clone(),
            })
            .collect()
    }
}
fn json(value: &str) -> JsonText {
    JsonText::new(value).unwrap()
}
fn identity(position: u64, class: StoredClass) -> CallIdentity {
    let kind = if class == StoredClass::Sync {
        CallKind::Primitive(Primitive::Now)
    } else if class == StoredClass::Local {
        CallKind::Primitive(Primitive::KvSet)
    } else {
        CallKind::Op {
            module: "module".into(),
            op: "call".into(),
        }
    };
    CallIdentity::new(
        HostCall {
            position,
            kind,
            args: json("{\"n\":1}"),
        },
        class,
    )
}
fn intent(position: u64, class: StoredClass) -> Intent {
    Intent {
        call: identity(position, class),
        idempotency_key: Some(format!("key-{position}")),
        frozen_request: json("{\"frozen\":true}"),
        attempts: 1,
        dispatch_count_window: Some(DispatchCount {
            window_start: 60,
            count: 1,
        }),
        audit_at: 61,
        reservation: None,
    }
}
fn arrival(position: u64, arrival_seq: u64) -> Arrival {
    let value = json("{\"answer\":42}");
    Arrival {
        position,
        arrival_seq,
        outcome: "completed".into(),
        settlement: Settlement::Fulfilled,
        value_hash: crate::ids::payload_hash(Settlement::Fulfilled, &value),
        value,
        handle: None,
        usage: None,
        cost_micros: None,
        kv_effect: None,
        durability: ArrivalDurability::default(),
    }
}
fn sync(position: u64, delivery_order: u64) -> Sync {
    Sync {
        call: identity(position, StoredClass::Sync),
        delivery_order,
        value: json("123"),
        clock_ms: Some(123),
    }
}
fn resolution(position: u64) -> Resolution {
    Resolution {
        position,
        attempts: 1,
        idempotency_key: Some("key".into()),
        choice: "observe".into(),
        observed_outcome: Some(arrival(position, 0)),
        usage: Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
        }),
        elicitation_id: "decision".into(),
    }
}
fn journal() -> (Records, Arc<Memory>) {
    let log = Arc::new(Memory::default());
    (Records::new("run".into(), 0, log.clone()), log)
}
fn append(records: &mut Records, body: Body) -> Lsn {
    records.append(1, body).unwrap()
}

#[test]
fn every_record_roundtrips_with_strict_frame_types() {
    let bodies = vec![
        Body::Claimed {
            first_claim_ms: 1,
            deadline_at: 999,
        },
        Body::Activated,
        Body::Requeued { broken: 2 },
        Body::Fingerprint {
            value: "engine identity".into(),
        },
        Body::ScriptStarted,
        Body::Intent(intent(0, StoredClass::Query)),
        Body::Issued(identity(0, StoredClass::Local)),
        Body::Accepted {
            position: 0,
            handle: "handle".into(),
        },
        Body::Arrived(arrival(0, 0)),
        Body::Released {
            position: 0,
            delivery_order: 0,
        },
        Body::Sync(sync(0, 0)),
        Body::Deferred(Deferred {
            position: 0,
            retry_not_before: 1,
            cause: "not ready".into(),
            audit_at: 2,
            proven_unsent: true,
            refund_dispatch: true,
            attempt: None,
        }),
        Body::Unknown {
            position: 0,
            reason: "lost".into(),
        },
        Body::ScopeCut {
            deadline_exempt: true,
        },
        Body::EventBody {
            digest: "digest".into(),
            body: Some("{}".into()),
            refusal: None,
            attempt: 2,
            retry_at: Some(3),
        },
        Body::Parked { awaited: vec![0] },
        Body::Woken,
        Body::Resolution(resolution(0)),
        Body::Snapshot {
            cutoff_lsn: Lsn {
                sequence: 2,
                offset: 100,
            },
            next_delivery_order: 0,
            next_arrival_seq: 0,
            state: Box::new(State::new(0)),
        },
        Body::Closed { commit_seq: 42 },
    ];
    let mut types = std::collections::BTreeSet::new();
    for body in bodies {
        let record = Record {
            run_id: "run".into(),
            generation: 7,
            body,
        };
        assert!(types.insert(record.record_type()));
        let mut payload = postcard::to_allocvec(&record).unwrap();
        assert_eq!(
            Record::decode(record.record_type(), &payload).unwrap(),
            record
        );
        assert!(Record::decode(255, &payload).is_err());
        assert!(Record::decode(1, &payload).is_err());
        payload.push(0);
        assert!(Record::decode(record.record_type(), &payload).is_err());
    }
    assert_eq!(types.len(), 20);
    // OpFor must not be normalized to Op during serialization.
    let mut call = identity(0, StoredClass::Query);
    call.kind = CallKind::OpFor {
        module: "m".into(),
        op: "o".into(),
    };
    let record = Record {
        run_id: "run".into(),
        generation: 1,
        body: Body::Issued(call),
    };
    assert_eq!(
        Record::decode(
            record.record_type(),
            &postcard::to_allocvec(&record).unwrap()
        )
        .unwrap(),
        record
    );
}

#[test]
fn local_effect_is_applied_only_when_arrival_is_appended() {
    let (mut records, log) = journal();
    append(&mut records, Body::Issued(identity(0, StoredClass::Local)));
    assert!(records.state().overlay.is_empty());
    let mut arrived = arrival(0, 0);
    arrived.kv_effect = Some(KvEffect::Set {
        key: "key".into(),
        value: json("7"),
    });
    log.fail_append.store(true, Ordering::SeqCst);
    assert_eq!(
        records.append(1, Body::Arrived(arrived.clone())),
        Err(CoreError::Cut)
    );
    assert!(records.state().overlay.is_empty());
    log.fail_append.store(false, Ordering::SeqCst);
    append(&mut records, Body::Arrived(arrived));
    assert_eq!(records.state().overlay.get("key"), Some(&Some("7".into())));
    assert!(records.state().prefix()[0].outcome.is_none());
    append(
        &mut records,
        Body::Released {
            position: 0,
            delivery_order: 0,
        },
    );
    append(&mut records, Body::Issued(identity(1, StoredClass::Local)));
    let mut deleted = arrival(1, 1);
    deleted.kv_effect = Some(KvEffect::Delete { key: "key".into() });
    append(&mut records, Body::Arrived(deleted));
    assert_eq!(records.state().overlay.get("key"), Some(&None));
    assert!(log.syncs.lock().unwrap().is_empty());
    let rebuilt = Records::recover("run".into(), 0, log.clone(), &log.records(false)).unwrap();
    assert_eq!(rebuilt.state(), records.state());
}

#[test]
fn replay_prefix_uses_one_release_and_sync_order_not_arrival_order() {
    let (mut records, log) = journal();
    append(&mut records, Body::Intent(intent(0, StoredClass::Query)));
    append(&mut records, Body::Intent(intent(1, StoredClass::Query)));
    append(&mut records, Body::Arrived(arrival(0, 0)));
    append(&mut records, Body::Arrived(arrival(1, 1)));
    append(
        &mut records,
        Body::Released {
            position: 1,
            delivery_order: 0,
        },
    );
    append(&mut records, Body::Sync(sync(2, 1)));
    let prefix = records.state().prefix();
    assert!(
        prefix[0].outcome.is_none(),
        "mailbox arrival must not leak into replay"
    );
    assert_eq!(prefix[1].outcome.as_ref().unwrap().delivery_order, 0);
    assert_eq!(prefix[2].outcome.as_ref().unwrap().delivery_order, 1);
    append(
        &mut records,
        Body::Released {
            position: 0,
            delivery_order: 2,
        },
    );
    let rebuilt = Records::recover("run".into(), 0, log.clone(), &log.records(false)).unwrap();
    let prefix = rebuilt.state().prefix();
    assert_eq!(
        prefix
            .iter()
            .map(|c| c.outcome.as_ref().unwrap().delivery_order)
            .collect::<Vec<_>>(),
        vec![2, 0, 1]
    );
    let mut bad = sync(3, 2);
    bad.call.kind = CallKind::Primitive(Primitive::Random);
    assert!(
        records.append(1, Body::Sync(bad)).is_err(),
        "sync and release cannot reuse an order"
    );
}

#[test]
fn identities_cover_local_refused_remote_clock_and_random_calls() {
    let (mut records, log) = journal();
    let calls = vec![
        Body::Issued(identity(0, StoredClass::Local)),
        Body::Issued(identity(1, StoredClass::Mutation)),
        Body::Intent(intent(2, StoredClass::KeyedMutation)),
        Body::Sync(sync(3, 0)),
    ];
    for body in calls {
        append(&mut records, body);
    }
    let mut random = sync(4, 1);
    random.call = CallIdentity::new(
        HostCall {
            position: 4,
            kind: CallKind::Primitive(Primitive::Random),
            args: JsonText::null(),
        },
        StoredClass::Sync,
    );
    random.value = json("0.125");
    random.clock_ms = None;
    append(&mut records, Body::Sync(random));
    let recovered = Records::recover("run".into(), 0, log.clone(), &log.records(false)).unwrap();
    assert_eq!(recovered.state().calls.len(), 5);
    for call in recovered.state().calls.values() {
        let issued = HostCall {
            position: call.identity.position,
            kind: call.identity.kind.clone(),
            args: call.identity.args.clone(),
        };
        recovered.state().check_issued(&issued).unwrap();
        let mut changed = issued.clone();
        changed.args = json("{\"n\":2}");
        assert!(matches!(
            recovered.state().check_issued(&changed),
            Err(CoreError::Worker(_))
        ));
        let mut wrong_kind = issued;
        wrong_kind.kind = CallKind::Primitive(Primitive::Facts);
        assert!(recovered.state().check_issued(&wrong_kind).is_err());
    }
    assert!(
        records
            .append(1, Body::Issued(identity(0, StoredClass::Local)))
            .is_err()
    );
    let mut bad = identity(5, StoredClass::Local);
    bad.args_digest = ArgsDigest([0; 32]);
    assert!(records.append(1, Body::Issued(bad)).is_err());
}

#[test]
fn identical_retry_uses_highest_attempt_without_a_second_identity() {
    let (mut records, log) = journal();
    let mut request = intent(0, StoredClass::KeyedMutation);
    append(&mut records, Body::Intent(request.clone()));
    request.attempts = 2;
    append(&mut records, Body::Intent(request.clone()));
    let recovered = Records::recover("run".into(), 0, log.clone(), &log.records(true)).unwrap();
    assert_eq!(recovered.state().calls.len(), 1);
    assert_eq!(recovered.state().calls[&0].prior_intents.len(), 1);
    assert_eq!(
        recovered.state().calls[&0].intent.as_ref().unwrap(),
        &request
    );
    assert_eq!(log.syncs.lock().unwrap().len(), 2);
}

#[test]
fn retry_changed_identity_is_corruption() {
    for change in 0..4 {
        let (mut records, log) = journal();
        let mut request = intent(0, StoredClass::Query);
        append(&mut records, Body::Intent(request.clone()));
        request.attempts = 2;
        match change {
            0 => request.call.kind = CallKind::Primitive(Primitive::Facts),
            1 => request.call.class = StoredClass::Mutation,
            2 => request.call.op = "changed".into(),
            _ => {
                request.call.args = json("2");
                request.call.args_digest = ArgsDigest::of(&request.call.args);
            }
        }
        // Bypass live validation to exercise the same protection during recovery.
        RecordLog::append(
            log.as_ref(),
            Record {
                run_id: "run".into(),
                generation: 1,
                body: Body::Intent(request),
            },
        )
        .unwrap();
        assert!(matches!(
            Records::recover("run".into(), 0, log.clone(), &log.records(false)),
            Err(CoreError::Corrupt(_))
        ));
    }
}

#[test]
fn retry_attempts_must_strictly_increase() {
    for attempts in [0, 1] {
        let (mut records, log) = journal();
        let mut request = intent(0, StoredClass::Mutation);
        append(&mut records, Body::Intent(request.clone()));
        request.attempts = attempts;
        RecordLog::append(
            log.as_ref(),
            Record {
                run_id: "run".into(),
                generation: 1,
                body: Body::Intent(request),
            },
        )
        .unwrap();
        assert!(matches!(
            Records::recover("run".into(), 0, log.clone(), &log.records(false)),
            Err(CoreError::Corrupt(_))
        ));
    }
}

#[test]
fn sync_table_covers_every_record_and_outcome_observability_case() {
    let mut state = State::new(0);
    state
        .apply(1, &Body::Intent(intent(0, StoredClass::Query)))
        .unwrap();
    let lazy = vec![
        Body::Claimed {
            first_claim_ms: 1,
            deadline_at: 2,
        },
        Body::Activated,
        Body::Fingerprint { value: "fp".into() },
        Body::ScriptStarted,
        Body::Issued(identity(1, StoredClass::Local)),
        Body::Released {
            position: 0,
            delivery_order: 0,
        },
        Body::Sync(sync(1, 0)),
        Body::ScopeCut {
            deadline_exempt: true,
        },
        Body::EventBody {
            digest: "d".into(),
            body: None,
            refusal: Some("no".into()),
            attempt: 1,
            retry_at: None,
        },
        Body::Parked { awaited: vec![0] },
        Body::Woken,
        Body::MeasuredUsage {
            position: 0,
            report: UsageReport {
                input_tokens: Some(7),
                cache_write_tokens: Some(2),
                output_tokens: Some(5),
                cached_input_tokens: Some(100),
            },
        },
        Body::Closed { commit_seq: 1 },
    ];
    for body in lazy {
        assert!(!state.requires_sync(&body).unwrap(), "{body:?}");
    }
    let durable = vec![
        Body::Intent(intent(1, StoredClass::Query)),
        Body::Accepted {
            position: 0,
            handle: "h".into(),
        },
        Body::Requeued { broken: 1 },
        Body::Unknown {
            position: 0,
            reason: "lost".into(),
        },
        Body::CallScopeCut { position: 0 },
        Body::Resolution(resolution(0)),
        Body::Snapshot {
            cutoff_lsn: Lsn::default(),
            next_delivery_order: 0,
            next_arrival_seq: 0,
            state: Box::new(state.clone()),
        },
    ];
    for body in durable {
        assert!(state.requires_sync(&body).unwrap(), "{body:?}");
    }
    for class in [
        StoredClass::Local,
        StoredClass::Query,
        StoredClass::Mutation,
        StoredClass::KeyedMutation,
    ] {
        for reason in 0..5 {
            let (mut records, log) = journal();
            append(&mut records, Body::Issued(identity(0, class)));
            let mut arrived = arrival(0, 0);
            match reason {
                1 => arrived.durability.refusal_committed = true,
                2 => arrived.durability.host_callback = true,
                3 => arrived.durability.immediate_completion = true,
                4 => arrived.durability.settles_reservation = true,
                _ => {}
            }
            append(&mut records, Body::Arrived(arrived));
            assert_eq!(
                !log.syncs.lock().unwrap().is_empty(),
                class == StoredClass::Mutation || reason != 0,
                "class {class:?}, observability {reason}"
            );
        }
        for proven_unsent in [false, true] {
            let (mut records, log) = journal();
            append(&mut records, Body::Issued(identity(0, class)));
            append(
                &mut records,
                Body::Deferred(Deferred {
                    position: 0,
                    retry_not_before: 1,
                    cause: "readiness".into(),
                    audit_at: 2,
                    proven_unsent,
                    refund_dispatch: true,
                    attempt: None,
                }),
            );
            assert_eq!(
                !log.syncs.lock().unwrap().is_empty(),
                class == StoredClass::Mutation || proven_unsent
            );
        }
    }
    let (mut records, log) = journal();
    let mut request = intent(0, StoredClass::Query);
    request.reservation = Some(Reservation {
        input_tokens: 100,
        output_tokens: 20,
        cost_micros: 25,
        window_start: 0,
    });
    append(&mut records, Body::Intent(request));
    append(&mut records, Body::Arrived(arrival(0, 0)));
    assert_eq!(
        log.syncs.lock().unwrap().len(),
        2,
        "reservation settlement must sync even without a caller flag"
    );
}

struct Fixture {
    dir: std::path::PathBuf,
    store: Arc<Store>,
}
impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = crate::test_support::timestamp_scratch("run-records", tag);
        let store = Arc::new(Store::open(dir.join("core.db"), Durability::throwaway()).unwrap());
        Self { dir, store }
    }
    fn register(
        &self,
        records: Records,
    ) -> (RunStore<Records>, Arc<crate::run_store::Run<Records>>) {
        let store = RunStore::new_staged(self.store.clone(), Arc::new(NoHooks));
        let run = store
            .insert("run".into(), "flow".into(), 1, records)
            .unwrap();
        (store, run)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn snapshot_trigger_is_third_oldest_segment_or_ten_minutes() {
    let (mut records, _) = journal();
    assert!(
        !records.snapshot_due(600_001, &[0, 1, 2]),
        "no log start yet"
    );
    append(&mut records, Body::Activated);
    assert!(!records.snapshot_due(600_000, &[0, 1]));
    assert!(records.snapshot_due(600_001, &[0, 1]));
    assert!(records.snapshot_due(0, &[0, 1, 2]));
    assert!(
        records.snapshot_due(0, &[1, 2, 3]),
        "a delayed maintenance pass must still pin a newer start"
    );
    let fixture = Fixture::new("trigger");
    let (store, run) = fixture.register(records);
    assert!(
        store
            .snapshot_due_runs(1, &[0, 1], &NoHooks)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .snapshot_due_runs(1, &[0, 1, 2], &NoHooks)
            .unwrap()
            .len(),
        1
    );
    run.activate(1, |a| {
        assert!(a.records().required_start().unwrap().offset > 1);
        Ok(())
    })
    .unwrap();
}

fn seed_invariants(records: &mut Records) {
    append(
        records,
        Body::Claimed {
            first_claim_ms: 10,
            deadline_at: 1000,
        },
    );
    append(records, Body::Activated);
    append(
        records,
        Body::Fingerprint {
            value: "engine and prelude".into(),
        },
    );
    append(records, Body::ScriptStarted);
    let mut unkeyed = intent(0, StoredClass::Mutation);
    unkeyed.idempotency_key = None;
    append(records, Body::Intent(unkeyed));
    append(records, Body::Intent(intent(1, StoredClass::KeyedMutation)));
    let mut budgeted = intent(2, StoredClass::Query);
    budgeted.reservation = Some(Reservation {
        input_tokens: 100,
        output_tokens: 200,
        cost_micros: 33,
        window_start: 60,
    });
    append(records, Body::Intent(budgeted));
    let mut acknowledged = arrival(2, 0);
    acknowledged.durability.host_callback = true;
    acknowledged.usage = Some(Usage {
        input_tokens: 40,
        output_tokens: 50,
    });
    acknowledged.cost_micros = Some(5);
    append(records, Body::Arrived(acknowledged));
    append(records, Body::Requeued { broken: 3 });
}
fn assert_invariants(state: &State) {
    assert_eq!(
        state.claim,
        Some(Claim {
            first_claim_ms: 10,
            deadline_at: 1000
        })
    );
    assert_eq!(state.activation_count, 1);
    assert_eq!(state.broken, 3);
    assert_eq!(
        state.calls[&0].intent.as_ref().unwrap().attempts,
        1,
        "recovery cannot authorize a second unkeyed send"
    );
    assert_eq!(
        state.calls[&0].intent.as_ref().unwrap().idempotency_key,
        None
    );
    assert_eq!(
        state.calls[&1]
            .intent
            .as_ref()
            .unwrap()
            .idempotency_key
            .as_deref(),
        Some("key-1")
    );
    assert_eq!(
        state.calls[&2].arrival.as_ref().unwrap().value,
        json("{\"answer\":42}")
    );
    assert_eq!(
        state.calls[&2]
            .intent
            .as_ref()
            .unwrap()
            .reservation
            .as_ref()
            .unwrap()
            .input_tokens,
        100
    );
    assert_eq!(
        state.calls[&2].arrival.as_ref().unwrap().cost_micros,
        Some(5)
    );
}

#[test]
fn latest_snapshot_applies_only_records_after_cutoff_even_without_older_segments() {
    let fixture = Fixture::new("cutoff");
    let (mut records, log) = journal();
    seed_invariants(&mut records);
    append(&mut records, Body::Issued(identity(3, StoredClass::Local)));
    let mut arrived = arrival(3, 1);
    arrived.kv_effect = Some(KvEffect::Set {
        key: "k".into(),
        value: json("8"),
    });
    append(&mut records, Body::Arrived(arrived));
    let (_store, run) = fixture.register(records);
    let first = run
        .snapshot_if_due(600_001, &[0], &NoHooks)
        .unwrap()
        .unwrap();
    run.activate(1, |a| {
        a.records().append(
            1,
            Body::Released {
                position: 2,
                delivery_order: 0,
            },
        )?;
        Ok(())
    })
    .unwrap();
    let second = run
        .snapshot_if_due(600_001, &[0], &NoHooks)
        .unwrap()
        .unwrap();
    assert!(second > first);
    run.activate(1, |a| {
        a.records().append(
            1,
            Body::Released {
                position: 3,
                delivery_order: 1,
            },
        )?;
        Ok(())
    })
    .unwrap();
    let all = log.records(false);
    // Reapplying pre-cutoff records would duplicate identities and arrival counters.
    let rebuilt = Records::recover("run".into(), 0, log.clone(), &all).unwrap();
    assert_invariants(rebuilt.state());
    assert_eq!(rebuilt.state().overlay.get("k"), Some(&Some("8".into())));
    assert_eq!(rebuilt.state().next_arrival_seq, 2);
    assert_eq!(rebuilt.state().next_delivery_order, 2);
    assert_eq!(rebuilt.required_start(), Some(second));
    let retained: Vec<_> = all.into_iter().filter(|r| r.lsn >= second).collect();
    let after_gc = Records::recover("run".into(), 0, log.clone(), &retained).unwrap();
    assert_eq!(after_gc.state(), rebuilt.state());
    assert_eq!(after_gc.state().prefix(), rebuilt.state().prefix());
}

struct CrashSnapshot;
impl Hooks for CrashSnapshot {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        crate::run_store::lock_order::assert_lock_free();
        if *boundary == Boundary::MidSnapshot {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

#[test]
pub(crate) fn mid_snapshot_crash_keeps_durable_invariants_with_or_without_unsynced_tail() {
    let fixture = Fixture::new("mid-snapshot");
    let (mut records, log) = journal();
    seed_invariants(&mut records);
    let original_start = records.required_start();
    let (_store, run) = fixture.register(records);
    assert_eq!(
        run.snapshot_if_due(600_001, &[0], &CrashSnapshot),
        Err(CoreError::Cut)
    );
    run.activate(1, |a| {
        assert_eq!(a.records().required_start(), original_start);
        Ok(())
    })
    .unwrap();
    for tail_survives in [false, true] {
        let recovered =
            Records::recover("run".into(), 0, log.clone(), &log.records(!tail_survives)).unwrap();
        assert_invariants(recovered.state());
        assert_eq!(recovered.state().prefix().len(), 3);
    }
}

#[test]
fn lifecycle_readiness_and_refusal_state_survive_a_snapshot() {
    let fixture = Fixture::new("state");
    let (mut records, log) = journal();
    append(
        &mut records,
        Body::Issued(identity(0, StoredClass::Mutation)),
    );
    let mut refused = arrival(0, 0);
    refused.outcome = "refused".into();
    refused.settlement = Settlement::Rejected;
    refused.value_hash = crate::ids::payload_hash(refused.settlement, &refused.value);
    refused.durability.refusal_committed = true;
    append(&mut records, Body::Arrived(refused));
    append(
        &mut records,
        Body::Deferred(Deferred {
            position: 0,
            retry_not_before: 50,
            cause: "readiness".into(),
            audit_at: 20,
            proven_unsent: true,
            refund_dispatch: true,
            attempt: None,
        }),
    );
    append(
        &mut records,
        Body::Unknown {
            position: 0,
            reason: "unknown".into(),
        },
    );
    append(&mut records, Body::Resolution(resolution(0)));
    append(
        &mut records,
        Body::ScopeCut {
            deadline_exempt: true,
        },
    );
    append(
        &mut records,
        Body::EventBody {
            digest: "d".into(),
            body: None,
            refusal: Some("denied".into()),
            attempt: 2,
            retry_at: Some(60),
        },
    );
    append(&mut records, Body::Parked { awaited: vec![0] });
    let expected = records.state().clone();
    let (_store, run) = fixture.register(records);
    let snapshot = run
        .snapshot_if_due(600_001, &[0], &NoHooks)
        .unwrap()
        .unwrap();
    let retained: Vec<_> = log
        .records(false)
        .into_iter()
        .filter(|r| r.lsn >= snapshot)
        .collect();
    let rebuilt = Records::recover("run".into(), 0, log.clone(), &retained).unwrap();
    assert_eq!(rebuilt.state(), &expected);
    assert!(
        rebuilt.state().calls[&0].intent.is_none(),
        "pre-authorization refusal must not imply dispatch"
    );
}

#[test]
fn activation_allocates_release_and_sync_orders_from_one_counter() {
    let (mut records, log) = journal();
    append(&mut records, Body::Intent(intent(0, StoredClass::Query)));
    append(&mut records, Body::Intent(intent(1, StoredClass::Query)));
    append(&mut records, Body::Arrived(arrival(1, 0)));
    append(&mut records, Body::Arrived(arrival(0, 1)));
    assert!(records.release_next(1, &[5]).unwrap().is_none());
    let first = records.release_next(1, &[0, 1]).unwrap().unwrap();
    assert_eq!((first.position, first.delivery_order), (1, 0));
    let clock = records
        .reply_sync(
            1,
            HostCall {
                position: 2,
                kind: CallKind::Primitive(Primitive::Now),
                args: JsonText::null(),
            },
            json("42"),
            Some(42),
        )
        .unwrap();
    assert_eq!(clock.delivery_order, 1);
    let last = records.release_next(1, &[0]).unwrap().unwrap();
    assert_eq!((last.position, last.delivery_order), (0, 2));
    assert!(records.all_calls_released());
    let frames = log.records(false);
    assert!(matches!(
        frames.last().unwrap().record.body,
        Body::Released {
            position: 0,
            delivery_order: 2
        }
    ));
}

struct BlockingLog {
    inner: Arc<Memory>,
    copied: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    resume: std::sync::Barrier,
}
impl RecordLog for BlockingLog {
    fn reclaim(&self, _: BTreeMap<String, Lsn>) -> Result<usize> {
        Ok(0)
    }
    fn append(&self, record: Record) -> Result<Lsn> {
        if matches!(record.body, Body::Snapshot { .. }) {
            self.copied
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(())
                .unwrap();
            self.resume.wait();
        }
        self.inner.append(record)
    }
    fn sync_through(&self, cut: Lsn) -> Result<()> {
        self.inner.sync_through(cut)
    }
}

#[test]
fn snapshot_copy_and_append_share_the_actor_critical_section() {
    let fixture = Fixture::new("atomic");
    let (copied, wait) = std::sync::mpsc::channel();
    let memory = Arc::new(Memory::default());
    let log = Arc::new(BlockingLog {
        inner: memory.clone(),
        copied: Mutex::new(Some(copied)),
        resume: std::sync::Barrier::new(2),
    });
    let mut records = Records::new("run".into(), 0, log.clone());
    append(&mut records, Body::Issued(identity(0, StoredClass::Local)));
    append(&mut records, Body::Arrived(arrival(0, 0)));
    let (_store, run) = fixture.register(records);
    let snapshot_run = run.clone();
    let snapshot = std::thread::spawn(move || {
        snapshot_run
            .snapshot_if_due(600_001, &[0], &NoHooks)
            .unwrap()
    });
    wait.recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let change_run = run.clone();
    let (attempting, attempted) = std::sync::mpsc::channel();
    let change = std::thread::spawn(move || {
        attempting.send(()).unwrap();
        change_run
            .activate(1, |a| {
                a.records().release_next(1, &[0])?;
                Ok(())
            })
            .unwrap();
    });
    attempted
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    log.resume.wait();
    snapshot.join().unwrap();
    change.join().unwrap();
    let frames = memory.records(false);
    let checkpoint = frames
        .iter()
        .find(|r| matches!(r.record.body, Body::Snapshot { .. }))
        .unwrap();
    let Body::Snapshot {
        cutoff_lsn, state, ..
    } = &checkpoint.record.body
    else {
        unreachable!()
    };
    assert_eq!(*cutoff_lsn, frames[1].lsn);
    assert_eq!(state.next_delivery_order, 0);
    let released = frames
        .iter()
        .find(|r| matches!(r.record.body, Body::Released { .. }))
        .unwrap();
    assert!(released.lsn > checkpoint.lsn);
    let recovered = Records::recover("run".into(), 0, memory.clone(), &frames).unwrap();
    assert_eq!(recovered.state().next_delivery_order, 1);
}

struct Registry(Mutex<crate::run_log::RegistrySnapshot>);
impl Registry {
    fn new() -> Self {
        Self(Mutex::new(crate::run_log::RegistrySnapshot {
            segments: BTreeMap::new(),
            first_retained: 0,
            predecessor: None,
            log_hwm: Lsn::default(),
            floor_synced: Lsn::default(),
            admit_lsns: Vec::new(),
        }))
    }
}
impl crate::run_log::SegmentRegistry for Registry {
    fn check_lease(&self, expected: u64) -> run_log::Result<()> {
        if expected == 1 {
            Ok(())
        } else {
            Err(run_log::Error::Fenced)
        }
    }
    fn snapshot(&self) -> run_log::Result<crate::run_log::RegistrySnapshot> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn reserve(&self, expected: u64, sequence: u64) -> run_log::Result<()> {
        self.check_lease(expected)?;
        self.0
            .lock()
            .unwrap()
            .segments
            .insert(sequence, run_log::SegmentState::Reserved);
        Ok(())
    }
    fn promote(&self, expected: u64, sequence: u64, synced: Lsn) -> run_log::Result<()> {
        self.check_lease(expected)?;
        let mut state = self.0.lock().unwrap();
        state
            .segments
            .insert(sequence, run_log::SegmentState::Active);
        state.log_hwm = state.log_hwm.max(synced);
        Ok(())
    }
}

#[test]
fn mid_snapshot_crash_reopens_real_log_with_complete_or_torn_snapshot() {
    for torn in [false, true] {
        let fixture = Fixture::new(if torn {
            "torn-checkpoint"
        } else {
            "whole-checkpoint"
        });
        let registry = Arc::new(Registry::new());
        let directory = fixture.dir.join("run-log");
        let config = || run_log::Config {
            directory: directory.clone(),
            identity: run_log::Identity {
                store_id: [7; 16],
                log_epoch: 1,
            },
            lease_epoch: 1,
            target_size: run_log::Config::TARGET_SIZE,
            hooks: Arc::new(NoHooks),
        };
        let log = Arc::new(RunLog::<Record>::create(config(), registry.clone()).unwrap());
        let mut records = Records::new("run".into(), 0, log.clone());
        seed_invariants(&mut records);
        let cutoff = records.last_lsn();
        registry.0.lock().unwrap().log_hwm = log.synced_lsn();
        let (store, run) = fixture.register(records);
        assert_eq!(
            run.snapshot_if_due(600_001, &[0], &CrashSnapshot),
            Err(CoreError::Cut)
        );
        drop(run);
        drop(store);
        drop(log);
        if torn {
            let path = directory.join("e1/0000000000000000.seg");
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            let size = file.metadata().unwrap().len();
            assert!(size > cutoff.offset + 1);
            file.set_len(cutoff.offset + (size - cutoff.offset) / 2)
                .unwrap();
        }
        let (log, recovered) = RunLog::<Record>::open(config(), registry.clone()).unwrap();
        assert_eq!(
            recovered
                .records
                .iter()
                .any(|r| matches!(r.record.body, Body::Snapshot { .. })),
            !torn
        );
        let log = Arc::new(log);
        let rebuilt = Records::recover("run".into(), 0, log.clone(), &recovered.records).unwrap();
        assert_invariants(rebuilt.state());
    }
}

/// Test-only process owner. The core library still never links or spawns a worker.
struct ReplayWorker {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: std::sync::mpsc::Receiver<basal_proto::WorkerMessage>,
    welcome: basal_proto::Welcome,
}
impl ReplayWorker {
    fn start() -> Self {
        let binary = std::env::var_os("BASAL_WORKER_BIN")
            .expect("build ck-basal-worker and set BASAL_WORKER_BIN");
        let mut command = std::process::Command::new(binary);
        #[cfg(target_os = "linux")]
        command.arg("--landlock=required");
        let mut child = command
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (send, output) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(message) = basal_proto::read_worker_message(&mut stdout) {
                if send.send(message).is_err() {
                    break;
                }
            }
        });
        let mut input = input;
        basal_proto::write_parent_message(
            &mut input,
            &basal_proto::ParentMessage::Hello {
                protocol_version: basal_proto::PROTOCOL_VERSION,
            },
        )
        .unwrap();
        let message = output
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        let basal_proto::WorkerMessage::Welcome(welcome) = message else {
            panic!("{message:?}")
        };
        Self {
            child,
            input,
            output,
            welcome,
        }
    }
    fn replay(
        &mut self,
        script: &str,
        prefix: Vec<basal_proto::RecordedCall>,
    ) -> basal_proto::ActivationResult {
        let request = basal_proto::ActivationRequest {
            activation_id: 1,
            profile: basal_proto::Profile::Flow,
            tools: vec![],
            prelude_hash: self.welcome.prelude_hash,
            script: script.into(),
            trigger: JsonText::null(),
            self_input: JsonText::null(),
            params_input: JsonText::null(),
            budgets: basal_proto::Budgets::default(),
            prefix,
        };
        basal_proto::write_parent_message(
            &mut self.input,
            &basal_proto::ParentMessage::Activate(Box::new(request)),
        )
        .unwrap();
        loop {
            match self
                .output
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
            {
                basal_proto::WorkerMessage::Finished {
                    activation_id: 1,
                    result,
                } => return result,
                basal_proto::WorkerMessage::Console { .. }
                | basal_proto::WorkerMessage::Warning { .. } => {}
                other => panic!("replay unexpectedly crossed the host boundary: {other:?}"),
            }
        }
    }
}
impl Drop for ReplayWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires a separately built confined ck-basal-worker via BASAL_WORKER_BIN"]
fn log_rebuilt_prefix_passes_unchanged_worker_nondeterminism_checks() {
    use basal_proto::{ActivationResult, Failure, Nondeterminism};
    let fixture = Fixture::new("worker-replay");
    let registry = Arc::new(Registry::new());
    let config = || run_log::Config {
        directory: fixture.dir.join("run-log"),
        identity: run_log::Identity {
            store_id: [9; 16],
            log_epoch: 1,
        },
        lease_epoch: 1,
        target_size: run_log::Config::TARGET_SIZE,
        hooks: Arc::new(NoHooks),
    };
    let log = Arc::new(RunLog::<Record>::create(config(), registry.clone()).unwrap());
    let mut records = Records::new("run".into(), 0, log.clone());
    let mut request = intent(0, StoredClass::Query);
    request.call = CallIdentity::new(
        HostCall {
            position: 0,
            kind: CallKind::Op {
                module: "mock".into(),
                op: "echo".into(),
            },
            args: json("1"),
        },
        StoredClass::Query,
    );
    append(&mut records, Body::Intent(request));
    append(&mut records, Body::Arrived(arrival(0, 0)));
    records.release_next(1, &[0]).unwrap().unwrap();
    records
        .reply_sync(
            1,
            HostCall {
                position: 1,
                kind: CallKind::Primitive(Primitive::Now),
                args: JsonText::null(),
            },
            json("123"),
            Some(123),
        )
        .unwrap();
    records
        .reply_sync(
            1,
            HostCall {
                position: 2,
                kind: CallKind::Primitive(Primitive::Random),
                args: JsonText::null(),
            },
            json("0.125"),
            None,
        )
        .unwrap();
    records.sync_through(records.last_lsn()).unwrap();
    drop(records);
    drop(log);
    let (log, recovered) = RunLog::<Record>::open(config(), registry).unwrap();
    let records = Records::recover("run".into(), 0, Arc::new(log), &recovered.records).unwrap();
    let prefix = records.state().prefix();
    let mut worker = ReplayWorker::start();
    assert_eq!(
        worker.replay(
            "const x = await ops.call('mock', 'echo', 1); return [x, Date.now(), Math.random()]",
            prefix.clone()
        ),
        ActivationResult::Completed {
            value: json("[{\"answer\":42},123,0.125]")
        }
    );
    let result = worker.replay("return await ops.call('mock', 'echo', 2)", prefix.clone());
    assert!(
        matches!(
            result,
            ActivationResult::Failed(Failure::Nondeterminism(Nondeterminism::Divergence {
                position: 0,
                ..
            }))
        ),
        "{result:?}"
    );
    let result = worker.replay("return await facts('x')", prefix.clone());
    assert!(
        matches!(
            result,
            ActivationResult::Failed(Failure::Nondeterminism(Nondeterminism::Divergence {
                position: 0,
                ..
            }))
        ),
        "{result:?}"
    );
    let result = worker.replay("return 1", prefix.clone());
    assert!(
        matches!(
            result,
            ActivationResult::Failed(Failure::Nondeterminism(Nondeterminism::UnreleasedOutcome {
                position: 0,
                delivery_order: 0
            }))
        ),
        "{result:?}"
    );
    let pending = Records::recover(
        "run".into(),
        0,
        records.log.clone(),
        &recovered.records[..1],
    )
    .unwrap();
    let result = worker.replay("return 1", pending.state().prefix());
    assert!(
        matches!(
            result,
            ActivationResult::Failed(Failure::Nondeterminism(Nondeterminism::UnconsumedCall {
                position: 0
            }))
        ),
        "{result:?}"
    );
}

#[test]
fn sync_failure_never_publishes_a_delivery_or_advances_snapshot_start() {
    let (mut records, log) = journal();
    append(&mut records, Body::Issued(identity(0, StoredClass::Query)));
    log.fail_sync.store(true, Ordering::SeqCst);
    let refusal = Record {
        run_id: "run".into(),
        generation: 1,
        body: Body::Arrived(arrival(0, 0)),
    };
    assert_eq!(
        records.append_refusal(refusal),
        Err(CoreError::Cut),
        "the refusal adapter must force a sync"
    );
    assert_eq!(records.release_next(1, &[0]), Err(CoreError::Cut));
    assert!(log.records(true).is_empty());
    let fixture = Fixture::new("sync-failure");
    let (mut records, log) = journal();
    append(&mut records, Body::Activated);
    let start = records.required_start();
    let (_store, run) = fixture.register(records);
    log.fail_sync.store(true, Ordering::SeqCst);
    assert_eq!(
        run.snapshot_if_due(600_001, &[0], &NoHooks),
        Err(CoreError::Cut)
    );
    run.activate(1, |a| {
        assert_eq!(a.records().required_start(), start);
        Ok(())
    })
    .unwrap();
}

#[test]
fn invalid_transitions_refuse_duplicate_identity_outcome_release_and_backwards_counters() {
    let (mut records, _) = journal();
    append(
        &mut records,
        Body::Claimed {
            first_claim_ms: 10,
            deadline_at: 100,
        },
    );
    assert!(
        records
            .append(
                2,
                Body::Claimed {
                    first_claim_ms: 20,
                    deadline_at: 200
                }
            )
            .is_err()
    );
    append(&mut records, Body::Requeued { broken: 2 });
    assert!(records.append(1, Body::Requeued { broken: 1 }).is_err());
    append(&mut records, Body::Issued(identity(0, StoredClass::Local)));
    assert!(
        records
            .append(1, Body::Intent(intent(0, StoredClass::Query)))
            .is_err()
    );
    assert!(records.append(1, Body::Sync(sync(0, 0))).is_err());
    assert!(
        records
            .append(
                1,
                Body::Released {
                    position: 0,
                    delivery_order: 0
                }
            )
            .is_err()
    );
    append(&mut records, Body::Arrived(arrival(0, 0)));
    assert!(records.append(1, Body::Arrived(arrival(0, 1))).is_err());
    append(
        &mut records,
        Body::Released {
            position: 0,
            delivery_order: 0,
        },
    );
    assert!(
        records
            .append(
                1,
                Body::Released {
                    position: 0,
                    delivery_order: 1
                }
            )
            .is_err()
    );
    assert_eq!(records.state().next_delivery_order, 1);
    assert_eq!(records.state().next_arrival_seq, 1);
}

#[test]
fn keyed_retry_cannot_change_its_idempotency_key() {
    let (mut records, log) = journal();
    let mut request = intent(0, StoredClass::KeyedMutation);
    append(&mut records, Body::Intent(request.clone()));
    request.attempts = 2;
    request.idempotency_key = Some("different-key".into());
    assert!(matches!(
        records.append(1, Body::Intent(request.clone())),
        Err(CoreError::Corrupt(_))
    ));
    RecordLog::append(
        log.as_ref(),
        Record {
            run_id: "run".into(),
            generation: 1,
            body: Body::Intent(request),
        },
    )
    .unwrap();
    assert!(matches!(
        Records::recover("run".into(), 0, log.clone(), &log.records(false)),
        Err(CoreError::Corrupt(_))
    ));
}
