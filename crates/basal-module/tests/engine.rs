//! The engine's loops in-process: bounded activations across flows, a
//! storage error that stops everything, and the production binary's
//! unconfigured host refusing every dispatch as never sent.

mod common;

use std::sync::Arc;
use std::time::Duration;

use basal_core::{Boundary, Clock, Config, Durability, Hooks, NoHooks, RunState, Step};
use basal_host::{MockCatalog, MockConsent};
use basal_module::caller::Caller;
use basal_module::dryrun::DryRunConfig;
use basal_module::engine::EngineConfig;
use basal_module::module::{Hosts, Module, ModuleConfig};
use basal_module::pool::ProcessSpawner;
use basal_module::unconfigured::UnconfiguredHost;
use common::{
    Options, T0, admit, agent, events_manifest, fixture, install_approved, pool_config, scratch,
};
use serde_json::json;

#[test]
fn activations_are_bounded_across_flows() {
    let f = fixture(
        "engine-bound",
        Options {
            max_concurrent: 1,
            ..Options::with_worker()
        },
    );
    let held = install_approved(
        &f,
        &agent("SYNAPSE"),
        "await ops.call('mock', 'echo', { gate: 'hold' }); return 1;",
        &events_manifest("flow-held"),
    );
    let other = install_approved(
        &f,
        &agent("SYNAPSE"),
        "return 2;",
        &events_manifest("flow-other"),
    );
    let first = admit(&f, &held, "h-1");
    let second = admit(&f, &other, "o-1");
    let report = f.module.engine.pass().expect("pass");
    assert_eq!(
        report.started,
        vec![first.clone()],
        "one activation at a time"
    );
    assert!(f.module.engine.wait_active(&first, Duration::from_secs(60)));
    // The first run waits at the gate, inside the host; another pass starts
    // nothing, although the other flow's run could start.
    let report = f.module.engine.pass().expect("pass");
    assert!(report.started.is_empty(), "{report:?}");
    assert_eq!(
        f.module.rt.run(&second).expect("run").state,
        RunState::Pending
    );
    f.mock.open_gate("hold");
    f.module.engine.run_until_idle(50).expect("idle");
    for run in [&first, &second] {
        assert_eq!(
            f.module.rt.run(run).expect("run").state,
            RunState::Succeeded
        );
    }
}

/// Cuts the store (a storage failure) when a run's second call commits.
struct CutAtSecondCall;

impl Hooks for CutAtSecondCall {
    fn at(&self, _run_id: &str, boundary: &Boundary) -> Step {
        if *boundary == (Boundary::CallCommitted { position: 1 }) {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

#[test]
fn a_storage_error_stops_the_engine_and_raises_the_fatal_latch() {
    let f = fixture(
        "engine-fatal",
        Options {
            hooks: Arc::new(CutAtSecondCall),
            ..Options::with_worker()
        },
    );
    let flow = install_approved(
        &f,
        &agent("SYNAPSE"),
        "await ops.call('mock', 'echo', { n: 1 }); await ops.call('mock', 'echo', { n: 2 }); return 1;",
        &events_manifest("flow-cut"),
    );
    admit(&f, &flow, "c-1");
    let err = f
        .module
        .engine
        .run_until_idle(50)
        .expect_err("the engine stops");
    assert!(f.module.fatal.get().is_some(), "{err}");
    // The latch stays raised: no further pass runs.
    assert!(f.module.engine.pass().is_err());
}

#[test]
fn the_unconfigured_host_refuses_every_dispatch_as_never_sent() {
    let dir = scratch("engine-unconfigured");
    let clock = Clock::manual(T0);
    let o = Options::with_worker();
    let pool = pool_config(&o);
    let consent = MockConsent::new();
    let module = Module::start(
        ModuleConfig {
            store_path: dir.join("basal.db"),
            durability: Durability { fullfsync: false },
            runtime: Config {
                clock,
                install_gate: basal_core::InstallGate::Off,
                ..Config::default()
            },
            pool: pool.clone(),
            engine: EngineConfig::default(),
            dry_run: DryRunConfig::new(dir.join("dry-run")),
        },
        Hosts {
            host: Arc::new(UnconfiguredHost::new()),
            // The mock catalog, so the flow can be installed at all.
            catalog: Arc::new(MockCatalog::standard()),
            consent: Arc::new(consent.clone()),
            hooks: Arc::new(NoHooks),
        },
        Arc::new(ProcessSpawner::new(&pool)),
    )
    .expect("start");
    let reply = module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({
                "script": "return await ops.call('mock', 'send', { n: 1 }).catch((e) => e.data.code);",
                "manifest": events_manifest("flow-nowhere").to_string(),
                "author": "SYNAPSE",
            }),
        )
        .expect("install");
    let card = reply["card_id"].as_str().expect("card");
    assert!(consent.decide(card, basal_host::CardDecision::Approve, "operator"));
    let run = module
        .rt
        .admit_trigger("flow-nowhere", "n-1", basal_proto::JsonText::null())
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();
    module.engine.run_until_idle(50).expect("idle");
    let done = module.rt.run(&run).expect("run");
    // A mutation that cannot be proven unsent would stop in
    // needs_reconcile; this one is proven unsent, so the script sees a
    // journaled rejection and the run completes.
    assert_eq!(done.state, RunState::Succeeded, "{done:#?}");
    assert_eq!(done.result.as_deref(), Some("\"unavailable\""));
    let calls = module.rt.calls(&run).expect("calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].dispatch, basal_core::DispatchState::Sent);
    module.pool.stop();
    let _ = std::fs::remove_dir_all(&dir);
}
