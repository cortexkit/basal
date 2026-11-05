//! Real-worker scripts exercise log-backed runtime execution and recovery after
//! removing records beyond the last synced log position.
use basal_core::{
    Admission, Clock, Config, Hooks, NoHooks, RunState, Runtime, Store,
    run_driver::Budgets,
    run_host::RecoveredHosts,
    run_log::{self, Lsn, RunLog, SqliteRegistry},
    run_records::{Body, Record, Records, State},
    run_store::lock_order,
    run_store_sql::{self as sql, EpochLsn, GcFloor, Meta},
};
use basal_host::{CallClass, CallRequest, Dispatched, Host, HostOutcome, TransportError};
use basal_proto::CallKind;
use basal_testkit::{
    fuzz::Rng,
    harness::{Probe, World, drive, wait_until},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Observed {
    sent: Vec<CallRequest>,
    outcomes: BTreeMap<u64, HostOutcome>,
    acknowledged: BTreeMap<u64, HostOutcome>,
}
struct HostOracle {
    mock: basal_host::mock::MockHost,
    observed: Mutex<Observed>,
}
impl Host for HostOracle {
    fn attach(&self, _: Arc<dyn basal_host::CompletionSink>) {
        lock_order::assert_lock_free();
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        self.mock.classify(kind)
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        lock_order::assert_lock_free();
        self.observed.lock().unwrap().sent.push(request.clone());
        let reply = self.mock.dispatch(request)?;
        if let Dispatched::Completed(outcome) = &reply {
            self.observed
                .lock()
                .unwrap()
                .outcomes
                .insert(request.position, outcome.clone());
        }
        Ok(reply)
    }
    fn dispatch_committed(&self, request: &CallRequest) {
        lock_order::assert_lock_free();
        let mut observed = self.observed.lock().unwrap();
        let outcome = observed.outcomes[&request.position].clone();
        observed.acknowledged.insert(request.position, outcome);
    }
    fn now_ms(&self) -> f64 {
        self.mock.now_ms()
    }
    fn random(&self) -> f64 {
        self.mock.random()
    }
}

fn budgets() -> Budgets {
    Budgets {
        dispatch_window_ms: 60_000,
        token_window_ms: 86_400_000,
        dispatches: 1000,
        tokens: u64::MAX,
        cost_micros: u64::MAX,
        cost_window: None,
    }
}

fn open(
    world: &World,
    host: Arc<HostOracle>,
    hooks: Arc<dyn Hooks>,
    first: bool,
) -> (
    Runtime,
    Arc<RunLog<Record>>,
    Vec<run_log::RecoveredRecord<Record>>,
) {
    let store = Arc::new(Store::open(world.store_path(), world.durability).unwrap());
    if first {
        let epoch = EpochLsn {
            log_epoch: 1,
            lsn: Lsn::default(),
        };
        store
            .write(|tx| {
                sql::create_test_schema(
                    tx,
                    &Meta {
                        store_id: store.store_id().into(),
                        log_epoch: 1,
                        log_hwm: epoch,
                        commit_seq: 0,
                        gc_floor: GcFloor {
                            log_epoch: 1,
                            first_retained: 0,
                            predecessor: None,
                            synced_lsn: Lsn::default(),
                            live_starts: BTreeMap::new(),
                        },
                        migration_phase: "test".into(),
                    },
                )
            })
            .unwrap();
    }
    let config = run_log::Config {
        directory: world.dir.join("run-log"),
        identity: run_log::Identity {
            store_id: store.store_id().as_bytes().try_into().unwrap(),
            log_epoch: 1,
        },
        lease_epoch: store.lease_epoch(),
        target_size: run_log::Config::TARGET_SIZE,
        hooks: hooks.clone(),
    };
    let registry = Arc::new(SqliteRegistry::new(store.clone(), 1));
    let (log, frames) = if first {
        (
            RunLog::<Record>::create(config, registry).unwrap(),
            Vec::new(),
        )
    } else {
        let (log, recovered) = RunLog::<Record>::open(config, registry).unwrap();
        (log, recovered.records)
    };
    let log = Arc::new(log);
    let clock = Clock::manual(3_700_000);
    let hosts = Arc::new(
        RecoveredHosts::recover(
            store.clone(),
            log.clone(),
            Arc::new(log.watermark()),
            &frames,
            budgets(),
            hooks.clone(),
            clock.clone(),
        )
        .unwrap(),
    );
    let runtime = Runtime::new_staged(
        hosts,
        host,
        Arc::new(world.catalog.clone()),
        hooks,
        Some(world.source.clone()),
        Config {
            clock,
            install_gate: basal_core::InstallGate::Off,
            selector: world.selector.clone(),
            activation_deadline: Duration::from_secs(10),
            ..Default::default()
        },
    )
    .unwrap();
    (runtime, log, frames)
}

fn admit(world: &World, rt: &Runtime, script: &str) -> String {
    let spec = world.spec(rt, script).unwrap();
    match rt.admit(&spec).unwrap() {
        Admission::Admitted { run_id } => run_id,
        other => panic!("{other:?}"),
    }
}

fn stop(rt: Runtime, log: &Arc<RunLog<Record>>) {
    rt.quiesce();
    drop(rt);
    // A logical crash returns before every in-flight send thread unwinds. A
    // real process exit would end those threads; wait for their log references
    // to disappear before acquiring the next process's storage lease.
    wait_until(
        Instant::now() + Duration::from_secs(5),
        "old run-log users to stop",
        || Arc::strong_count(log) == 1,
    )
    .unwrap();
}

fn script(rng: &mut Rng, seed: u64) -> String {
    let width = 3 + rng.below(4);
    let mut script = format!(
        "await ops.call('mock','post',{{seed:{seed}}});\nawait ops.call('mock','send',{{seed:{seed}}});\nconst calls = [\n"
    );
    for n in 0..width {
        // A delayed call remains outstanding while faster continuations make
        // synchronous reads. The random widths and delays change arrival order.
        let delay = if n == 0 { 80 } else { rng.below(8) };
        script.push_str(&format!("ops.call('mock','echo',{{n:{n},delay_ms:{delay}}}).then(async v => {{ const t=Date.now(); const r=Math.random(); await ops.call('mock','echo',{{seen:v.n,t,r}}); return {{n:v.n}}; }}),\n"));
    }
    script.push_str("] ; const values = await Promise.all(calls); return values.map(v=>v.n);");
    script
}

fn assert_c(state: &State, observed: &Observed, original: Option<&State>, label: &str) {
    let mut sends: BTreeMap<u64, Vec<&CallRequest>> = BTreeMap::new();
    for request in &observed.sent {
        sends.entry(request.position).or_default().push(request);
    }
    for (position, requests) in sends {
        let intent = state.calls[&position]
            .intent
            .as_ref()
            .expect("dispatch needs durable Intent");
        if matches!(&requests[0].kind, CallKind::Op { op, .. } if op == "post") {
            assert_eq!(requests.len(), 1, "{label}: unkeyed resend");
        }
        for request in requests {
            if let Some(key) = &intent.idempotency_key {
                assert_eq!(&request.idempotency_key, key, "{label}: retry key changed");
            }
        }
    }
    for (position, outcome) in &observed.acknowledged {
        let arrival = state.calls[position]
            .arrival
            .as_ref()
            .expect("acknowledged outcome lost");
        assert_eq!(arrival.value, outcome.value, "{label}");
        assert_eq!(arrival.settlement, outcome.settlement, "{label}");
    }
    if let Some(original) = original {
        if let Some(claim) = &original.claim {
            assert_eq!(
                state.claim.as_ref().unwrap().deadline_at,
                claim.deadline_at,
                "{label}: durable deadline rearmed"
            );
        }
        assert!(
            state.broken >= original.broken,
            "{label}: synced broken count reset"
        );
    }
    let counted: u64 = state
        .calls
        .values()
        .flat_map(|c| c.prior_intents.iter().chain(c.intent.iter()))
        .filter_map(|i| i.dispatch_count_window.as_ref())
        .map(|c| c.count)
        .sum();
    assert!(
        counted >= observed.sent.len() as u64,
        "{label}: budget undercount {counted} < {}",
        observed.sent.len()
    );
}

#[test]
fn random_scripts_with_concurrent_calls_sync_interleavings_and_crashes_preserve_c() {
    const {
        assert!(
            cfg!(debug_assertions),
            "the property suite requires lock-order checks"
        )
    };
    let mut rng = Rng::new(0x5eed_cafe);
    for case in 0..12 {
        let script = script(&mut rng, case);
        let baseline = World::new("log-property-baseline");
        let host = Arc::new(HostOracle {
            mock: baseline.mock.clone(),
            observed: Mutex::new(Observed::default()),
        });
        let trace = Arc::new(Probe::recording());
        let (rt, log, _) = open(&baseline, host.clone(), trace.clone(), true);
        let id = admit(&baseline, &rt, &script);
        let result = drive(&rt, &baseline.mock, &id, Duration::from_secs(15)).unwrap();
        assert_eq!(
            result.state,
            RunState::Succeeded,
            "baseline case {case}: {result:?}"
        );
        let expected_result = result.result;
        stop(rt, &log);
        drop(log);
        let (rt, log, frames) = open(&baseline, host.clone(), Arc::new(NoHooks), false);
        let state = Records::recover(id, 3_700_000, log.clone(), &frames).unwrap();
        assert_c(state.state(), &host.observed.lock().unwrap(), None, "uncut");
        let bodies: Vec<_> = frames.iter().map(|f| &f.record.body).collect();
        assert!(
            bodies
                .iter()
                .enumerate()
                .any(|(i, body)| matches!(body, Body::Sync(_))
                    && bodies[..i]
                        .iter()
                        .any(|b| matches!(b, Body::Arrived(a) if a.position >= 2))
                    && bodies[i + 1..]
                        .iter()
                        .any(|b| matches!(b, Body::Arrived(a) if a.position >= 2))),
            "case {case}: sync calls did not interleave arrivals"
        );
        drop(state);
        stop(rt, &log);
        drop(log);

        let points: Vec<_> = trace
            .points()
            .into_iter()
            .filter(|p| {
                matches!(
                    p.boundary.as_str(),
                    "DriverAppended" | "DriverSynced" | "DriverDispatched" | "DriverReleased"
                )
            })
            .collect();
        let point = points[rng.below(points.len() as u64) as usize].clone();
        let label = format!("seed=0x5eed_cafe case={case} cut={}", point.render());
        let world = World::new("log-property-cut");
        let host = Arc::new(HostOracle {
            mock: world.mock.clone(),
            observed: Mutex::new(Observed::default()),
        });
        let probe = Arc::new(Probe::crash_at(point));
        let (rt, log, _) = open(&world, host.clone(), probe.clone(), true);
        let id = admit(&world, &rt, &script);
        let cut_result = drive(&rt, &world.mock, &id, Duration::from_secs(15));
        assert!(probe.fired(), "{label}: cut not reached: {cut_result:?}");
        stop(rt, &log);
        let durable = log.synced_lsn();
        drop(log);
        // Dropping a writer does not evict the kernel page cache. Explicitly
        // remove its lazy suffix to model a power loss, rather than a clean exit.
        std::fs::OpenOptions::new()
            .write(true)
            .open(
                world
                    .dir
                    .join(format!("run-log/e1/{:016x}.seg", durable.sequence)),
            )
            .unwrap()
            .set_len(durable.offset)
            .unwrap();
        let (rt, log, frames) = open(&world, host.clone(), Arc::new(NoHooks), false);
        let before = Records::recover(id.clone(), 3_700_000, log.clone(), &frames).unwrap();
        assert_c(before.state(), &host.observed.lock().unwrap(), None, &label);
        rt.recover().unwrap();
        let result = drive(&rt, &world.mock, &id, Duration::from_secs(15)).unwrap();
        assert!(
            matches!(result.state, RunState::Succeeded | RunState::NeedsReconcile),
            "{label}: {result:?}"
        );
        if result.state == RunState::Succeeded {
            assert_eq!(result.result, expected_result, "{label}");
        }
        let original = before.state().clone();
        drop(before);
        stop(rt, &log);
        drop(log);
        let (rt, log, frames) = open(&world, host.clone(), Arc::new(NoHooks), false);
        let recovered = Records::recover(id, 3_700_000, log.clone(), &frames).unwrap();
        assert_c(
            recovered.state(),
            &host.observed.lock().unwrap(),
            Some(&original),
            &label,
        );
        drop(recovered);
        stop(rt, &log);
        drop(log);
    }
}
