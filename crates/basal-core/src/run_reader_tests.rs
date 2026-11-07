use super::LOAD_CALLS;
use crate::{
    Clock, InstallGate, RunState, StoredClass,
    ops::{Health, LastRun, RunAges},
    run_records::{Body, CallIdentity, Claim, Deferred, Intent, State},
    runtime_authorization_regressions::TestRuntime,
};
use basal_proto::{CallKind, HostCall, JsonText};
use std::{collections::BTreeMap, sync::Arc};

fn intent(position: u64) -> Body {
    Body::Intent(Intent {
        call: CallIdentity::new(
            HostCall {
                position,
                kind: CallKind::Op {
                    module: "mock".into(),
                    op: "send".into(),
                },
                args: JsonText::null(),
            },
            StoredClass::KeyedMutation,
        ),
        idempotency_key: Some(format!("key-{position}")),
        frozen_request: JsonText::null(),
        attempts: 1,
        dispatch_count_window: None,
        audit_at: 0,
        reservation: None,
    })
}

fn fixture() -> TestRuntime {
    let mut f = TestRuntime::new();
    let config = Arc::make_mut(&mut f.rt.config);
    config.clock = Clock::manual(1000);
    config.install_gate = InstallGate::Off;
    config.limits.default_deadline = std::time::Duration::from_millis(60_000);
    *f.rt.shared.retention_next.lock().unwrap() = Some(3_600_000);
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO flows(flow_id,created_at) VALUES ('flow',0),('empty',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    // The old failure is separated from the current streak by 189 successes.
    for n in 1..=200 {
        let (state, kind, exempt) = match n {
            1 | 191 | 200 => (RunState::Failed, Some("script"), false),
            192 | 199 => (RunState::Cancelled, None, false),
            193 => (RunState::Failed, Some("readiness_rejection"), false),
            194 => (RunState::Failed, Some("agent_retired"), false),
            195 => (RunState::Failed, Some("version_revoked"), false),
            196 => (RunState::Failed, Some("deadline"), true),
            197 => (RunState::Failed, Some("deadline"), false),
            198 => (RunState::EngineMismatch, None, false),
            _ => (RunState::Succeeded, None, false),
        };
        let id = format!("finished-{n:03}");
        let mut snapshot = State::new(n);
        snapshot.deadline_exempt = exempt;
        f.rt.fixture_run(&id, "flow", 1, "return 1", "{}", snapshot)
            .unwrap();
        // Admission order, not end time, defines the last run. The newest
        // failure also has the earliest end time in the counted streak.
        f.rt.config.clock.set(if n == 200 { 999 } else { 3000 + n });
        f.rt.fixture_close(&id, state, None, kind.map(|k| (k, "test failure")))
            .unwrap();
    }
    f.rt.config.clock.set(1000);
    for (id, at) in [
        ("pending", 100),
        ("suspended", 200),
        ("reconcile", 300),
        ("running", 400),
    ] {
        let mut state = State::new(at);
        if matches!(id, "running" | "suspended") {
            state.claim = Some(Claim {
                first_claim_ms: 1000,
                deadline_at: 61_000,
            });
            state.parked = id == "suspended";
        }
        f.rt.fixture_run(id, "flow", 1, "return 1", "{}", state)
            .unwrap();
    }
    f.rt.fixture_append("suspended", intent(0)).unwrap();
    f.rt.fixture_append(
        "suspended",
        Body::Deferred(Deferred {
            position: 0,
            retry_not_before: 10_000,
            cause: "provider busy".into(),
            audit_at: 1000,
            proven_unsent: true,
            refund_dispatch: true,
            attempt: None,
        }),
    )
    .unwrap();
    f.rt.fixture_append("reconcile", intent(0)).unwrap();
    f.rt.fixture_append(
        "reconcile",
        Body::Unknown {
            position: 0,
            reason: "connection_lost".into(),
        },
    )
    .unwrap();
    f.rt.store().write(|tx| {
        tx.execute("INSERT INTO obligations(run_id,position,state,audit_at,detail) VALUES ('finished-001',7,'unknown',0,'{}'),('finished-002',8,'open',0,'{}')", [])?;
        Ok(())
    }).unwrap();
    f
}

fn measured<T>(reader: impl FnOnce() -> T) -> (T, usize) {
    LOAD_CALLS.with(|calls| calls.set(0));
    let value = reader();
    (value, LOAD_CALLS.with(|calls| calls.get()))
}

#[test]
fn health_counts_history_but_loads_only_live_runs() {
    let f = fixture();
    f.rt.store()
        .write(|tx| {
            tx.execute("INSERT INTO meta(key,value) VALUES('draining','1')", [])?;
            Ok(())
        })
        .unwrap();
    let (health, loads) = measured(|| f.rt.health().unwrap());
    assert_eq!(loads, 4, "health must not materialize 200 finished runs");
    assert_eq!(
        health,
        Health {
            runs: BTreeMap::from([
                ("succeeded".into(), 189),
                ("failed".into(), 8),
                ("cancelled".into(), 2),
                ("engine_mismatch".into(), 1),
                ("pending".into(), 1),
                ("running".into(), 1),
                ("suspended".into(), 1),
                ("needs_reconcile".into(), 1),
            ]),
            unfinished: BTreeMap::from([
                ("pending".into(), vec!["pending".into()]),
                ("running".into(), vec!["running".into()]),
                ("suspended".into(), vec!["suspended".into()]),
                ("needs_reconcile".into(), vec!["reconcile".into()]),
            ]),
            unknown_calls: vec![("finished-001".into(), 7), ("reconcile".into(), 0)],
            open_obligations: vec![
                ("finished-001".into(), 7),
                ("finished-002".into(), 8),
                ("reconcile".into(), 0),
                ("suspended".into(), 0)
            ],
            quarantined: 0,
            draining: true,
        }
    );
}

#[test]
fn flow_health_preserves_streak_and_loads_only_live_runs() {
    let f = fixture();
    let (flows, loads) = measured(|| f.rt.flow_health().unwrap());
    assert_eq!(loads, 4, "flow health must not materialize finished runs");
    assert_eq!(flows.len(), 2);
    let empty = &flows[0];
    assert_eq!(empty.flow_id, "empty");
    assert_eq!(empty.last_run, None);
    assert_eq!(empty.consecutive_failures, 0);
    assert_eq!(empty.failure_first_seen_ms, None);
    assert_eq!(empty.oldest_overdue_ms, None);
    assert!(empty.needs_reconcile.is_empty());
    let flow = &flows[1];
    assert_eq!(flow.flow_id, "flow");
    assert_eq!(
        flow.last_run,
        Some(LastRun {
            run_id: "finished-200".into(),
            state: "failed".into(),
            error_kind: Some("script".into()),
            ended_at: 999,
        })
    );
    assert_eq!(flow.consecutive_failures, 4);
    assert_eq!(flow.failure_first_seen_ms, Some(999));
    assert_eq!(flow.oldest_overdue_ms, Some(900));
    assert_eq!(flow.waiting_reason.as_deref(), Some("provider busy"));
    assert_eq!(flow.needs_reconcile, ["reconcile"]);
}

#[test]
fn engine_readers_load_only_live_runs_amid_history() {
    let f = fixture();
    let (wake, loads) = measured(|| f.rt.next_wake_at(None).unwrap());
    assert_eq!(loads, 4, "wake calculation must not materialize history");
    assert_eq!(wake, Some(10_000));
    let (ages, loads) = measured(|| f.rt.run_ages().unwrap());
    assert_eq!(loads, 4, "run ages must not materialize history");
    assert_eq!(
        ages,
        RunAges {
            oldest_pending_ms: Some(900),
            oldest_suspended_ms: Some(800),
            oldest_needs_reconcile_ms: Some(700),
        }
    );
    let (expired, loads) = measured(|| f.rt.enforce_deadlines().unwrap());
    assert_eq!(
        loads, 4,
        "deadline enforcement must not materialize history"
    );
    assert!(expired.is_empty());
    f.rt.config.clock.set(61_000);
    assert_eq!(f.rt.enforce_deadlines().unwrap(), ["suspended", "running"]);
    assert_eq!(
        f.rt.run("reconcile").unwrap().state,
        RunState::NeedsReconcile
    );
}

#[test]
fn recent_flow_runs_bound_loads_and_preserve_admission_order() {
    let f = fixture();
    f.rt.fixture_run("other-flow", "empty", 1, "return 1", "{}", State::new(0))
        .unwrap();
    let (runs, loads) = measured(|| f.rt.recent_runs_for_flow("flow", 20).unwrap());
    assert_eq!(loads, 20);
    let mut expected: Vec<_> = (185..=200).map(|n| format!("finished-{n:03}")).collect();
    expected.extend(["pending", "suspended", "reconcile", "running"].map(String::from));
    assert_eq!(
        runs.iter().map(|r| r.run_id.clone()).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(runs[0].state, RunState::Succeeded);
    assert_eq!(runs[6].state, RunState::Failed);
    assert_eq!(runs[7].state, RunState::Cancelled);
    assert_eq!(runs[16].state, RunState::Pending);
    assert_eq!(runs[18].state, RunState::NeedsReconcile);
    for (flow, limit) in [("flow", 0), ("missing", 20)] {
        let (runs, loads) = measured(|| f.rt.recent_runs_for_flow(flow, limit).unwrap());
        assert!(runs.is_empty());
        assert_eq!(loads, 0);
    }
    let all = f.rt.runs_for_flow("flow").unwrap();
    assert_eq!(all.len(), 204);
    assert_eq!(all.first().unwrap().run_id, "finished-001");
    assert_eq!(all.last().unwrap().run_id, "running");
    // A tie at the page boundary must use the same run-id ordering across
    // live and finished rows, rather than preferring either table.
    f.rt.store().write(|tx| {
        tx.execute("UPDATE run_history SET admit_seq=(SELECT admit_seq FROM live_runs WHERE run_id='running') WHERE run_id='finished-200'", [])?;
        Ok(())
    }).unwrap();
    let (runs, loads) = measured(|| f.rt.recent_runs_for_flow("flow", 2).unwrap());
    assert_eq!(loads, 2);
    assert_eq!(
        runs.iter().map(|r| r.run_id.as_str()).collect::<Vec<_>>(),
        ["finished-200", "running"]
    );
    assert_eq!(
        f.rt.recent_runs_for_flow("flow", 1).unwrap()[0].run_id,
        "running"
    );
}
