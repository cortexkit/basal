//! Classifier op grants reserve dollars in the same transaction as the journal.

mod common;
use basal_core::{Clock, Durability, InstallError, InstallRequest, NoHooks, Runtime, Store};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, HostOutcome, TransportError,
};
use basal_proto::{CallKind, JsonText};
use basal_testkit::harness::{World, test_manifest};
use common::{config, finish, result};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Provider {
    store: Arc<Store>,
    calls: Mutex<Vec<CallRequest>>,
    outcome: Mutex<HostOutcome>,
    lose_reply: AtomicBool,
}
impl Host for Provider {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, request: &CallRequest) -> Result<Dispatched, TransportError> {
        assert_eq!(request.flow_id, "flow-test");
        assert!(request.flow_version.is_some());
        self.store
            .read(|c| {
                let saved: String = c.query_row(
                    "SELECT request FROM journal WHERE run_id=?1 AND position=?2",
                    rusqlite::params![request.run_id, request.position],
                    |r| r.get(0),
                )?;
                assert_eq!(
                    saved,
                    request.args.as_str(),
                    "clamped request commits before sending"
                );
                Ok(())
            })
            .unwrap();
        self.calls.lock().unwrap().push(request.clone());
        if self.lose_reply.load(Ordering::SeqCst) {
            return Err(TransportError::maybe_sent(
                basal_host::UnknownReason::ConnectionLost,
                "lost classifier reply",
            ));
        }
        Ok(Dispatched::Completed(self.outcome.lock().unwrap().clone()))
    }
    fn now_ms(&self) -> f64 {
        100.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}

fn fixture(cap: f64) -> (World, Runtime, Arc<Provider>, Value, Clock) {
    let world = World::new("classify-dollars");
    world.catalog.set_tool_contract(
        "mock",
        "echo",
        json!({"modes":["flow"],"capabilities":[cortexkit_llm_tools::classify::TAG],
        "input_schema":cortexkit_llm_tools::classify::input_schema()}),
    );
    let store = Arc::new(Store::open(world.store_path(), Durability::throwaway()).unwrap());
    let provider = Arc::new(Provider {
        store: store.clone(),
        calls: Mutex::new(vec![]),
        lose_reply: AtomicBool::new(false),
        outcome: Mutex::new(HostOutcome::fulfilled(
            JsonText::new(json!({"answers":{},"usage":{},"cost_usd":0}).to_string()).unwrap(),
        )),
    });
    let clock = Clock::manual(100);
    let rt = Runtime::new(
        store,
        provider.clone(),
        Arc::new(world.catalog.clone()),
        Arc::new(NoHooks),
        Some(world.source.clone()),
        basal_core::Config {
            clock: clock.clone(),
            ..config()
        },
    );
    rt.recover().unwrap();
    let mut m = test_manifest();
    m["model_cost_usd"] = json!({"per_run":cap,"usd":cap,"window":"1h"});
    (world, rt, provider, m, clock)
}

fn run(world: &World, rt: &Runtime, m: &Value, ceiling: Option<f64>, trigger: &str) -> String {
    let vectors: Value = serde_json::from_str(cortexkit_llm_tools::vectors::CLASSIFY).unwrap();
    let mut args = vectors["valid_requests"][0]["value"].clone();
    if let Some(cap) = ceiling {
        args["max_cost_usd"] = json!(cap);
    } else {
        args.as_object_mut().unwrap().remove("max_cost_usd");
    }
    let script = format!(
        "try {{ return await ops.call('mock','echo',{args}); }} catch(e) {{ return e.data; }}"
    );
    let mut spec = world.spec_with(rt, &script, m).unwrap();
    spec.trigger_id = trigger.into();
    let id = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    finish(rt, world, &id);
    id
}

fn spent(rt: &Runtime, id: &str) -> f64 {
    rt.store()
        .read(|c| basal_core::classify_cost::spent(c, "flow", id))
        .unwrap()
}

#[test]
fn flow_classify_clamps_and_supplies_remaining_dollars() {
    let (world, rt, provider, m, _) = fixture(0.30);
    for (i, ceiling) in [Some(1.0), None].into_iter().enumerate() {
        let id = run(&world, &rt, &m, ceiling, &format!("t{i}"));
        let calls = provider.calls.lock().unwrap();
        let input: Value = serde_json::from_str(calls[i].args.as_str()).unwrap();
        assert_eq!(input["max_cost_usd"], 0.30);
        assert_eq!(spent(&rt, &id), 0.0);
        let row = &rt.calls(&id).unwrap()[0];
        assert_eq!(row.class, basal_core::StoredClass::Mutation);
    }
}

#[test]
fn flow_classify_exhausted_dollars_never_send() {
    let (world, rt, provider, m, _) = fixture(0.0);
    let id = run(&world, &rt, &m, None, "t1");
    let run = rt.run(&id).unwrap();
    assert_eq!(result(&run)["code"], "budget_exhausted");
    assert_eq!(result(&run)["budget"], "model_cost_usd");
    assert!(provider.calls.lock().unwrap().is_empty());
}

#[test]
fn flow_classify_charges_success_overrun_and_rolling_window() {
    let (world, rt, provider, m, clock) = fixture(0.30);
    *provider.outcome.lock().unwrap() = HostOutcome::fulfilled(
        JsonText::new(json!({"answers":{},"usage":{},"cost_usd":0.42}).to_string()).unwrap(),
    );
    let id = run(&world, &rt, &m, Some(0.001), "t1");
    assert_eq!(spent(&rt, &id), 0.42);
    let id2 = run(&world, &rt, &m, None, "t2");
    assert_eq!(result(&rt.run(&id2).unwrap())["code"], "budget_exhausted");
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
    clock.advance(3_600_001);
    run(&world, &rt, &m, None, "t3");
    assert_eq!(provider.calls.lock().unwrap().len(), 2);
}

#[test]
fn flow_classify_unmeasured_charges_max_and_premodel_refusals_are_free() {
    let (world, rt, provider, m, _) = fixture(0.30);
    for (i, code) in [
        "invalid_params",
        "scope_required",
        "flow_scope_required",
        "cost_unpriceable",
        "cost_exceeded",
        "provider_not_allowed",
        "not_started",
        "cost_unmeasured",
    ]
    .into_iter()
    .enumerate()
    {
        *provider.outcome.lock().unwrap() = HostOutcome::rejected(
            JsonText::new(json!({"code":code,"message":code,"detail":{"consent":{}}}).to_string())
                .unwrap(),
        );
        let id = run(&world, &rt, &m, Some(0.10), &format!("t{i}"));
        assert_eq!(
            spent(&rt, &id),
            if code == "cost_unmeasured" { 0.10 } else { 0.0 }
        );
    }
}

#[test]
fn flow_classify_tool_result_cost_is_not_the_reservation() {
    let (world, rt, provider, m, _) = fixture(0.30);
    *provider.outcome.lock().unwrap() = HostOutcome::fulfilled(
        JsonText::new(
            json!({"content":[],"structuredContent":{"answers":{},"usage":{},"cost_usd":0.042}})
                .to_string(),
        )
        .unwrap(),
    );
    let id = run(&world, &rt, &m, Some(0.001), "t1");
    assert_eq!(spent(&rt, &id), 0.042);
}

#[test]
fn flow_classify_unknown_send_is_never_automatically_resent() {
    let (world, rt, provider, m, _) = fixture(0.30);
    provider.lose_reply.store(true, Ordering::SeqCst);
    let id = run(&world, &rt, &m, None, "t1");
    assert_eq!(
        rt.run(&id).unwrap().state,
        basal_core::RunState::NeedsReconcile
    );
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
    assert_eq!(spent(&rt, &id), 0.30);
    rt.recover().unwrap();
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
}

#[test]
fn flow_classify_per_run_cap_is_not_reset_between_calls() {
    let (world, rt, provider, mut m, _) = fixture(0.30);
    m["model_cost_usd"]["per_run"] = json!(0.10);
    *provider.outcome.lock().unwrap() = HostOutcome::fulfilled(
        JsonText::new(json!({"answers":{},"usage":{},"cost_usd":0.10}).to_string()).unwrap(),
    );
    let args = json!({"model":"example/classifier","questions":{"urgent":{"type":"noul","instructions":"Is this urgent?"}},"item":{"state":"A kettle"}});
    let script = format!(
        "await ops.call('mock','echo',{args}); try {{ await ops.call('mock','echo',{args}); return 'sent'; }} catch(e) {{ return e.data.code; }}"
    );
    let spec = world.spec_with(&rt, &script, &m).unwrap();
    let id = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    let run = finish(&rt, &world, &id);
    assert_eq!(result(&run), "budget_exhausted");
    assert_eq!(provider.calls.lock().unwrap().len(), 1);
    assert_eq!(spent(&rt, &id), 0.10);
}

#[test]
fn flow_classify_install_requires_dollar_cap() {
    let (world, rt, _, mut m, _) = fixture(0.30);
    m.as_object_mut().unwrap().remove("model_cost_usd");
    let error = rt
        .install(&InstallRequest {
            manifest: m.to_string(),
            script: "return 1".into(),
            author: "alice".into(),
            loop_override: false,
        })
        .unwrap_err();
    assert!(
        matches!(error, InstallError::ModelCostCapRequired { .. }),
        "{error:?}"
    );
    assert!(world.spec_with(&rt, "return 1", &m).is_err());
}

#[test]
fn flow_classify_untagged_cost_schema_is_not_installable() {
    let (world, rt, _, m, _) = fixture(0.30);
    world.catalog.set_tool_contract(
        "mock",
        "echo",
        json!({"modes":["flow"],"input_schema":cortexkit_llm_tools::classify::input_schema()}),
    );
    let error = rt
        .install(&InstallRequest {
            manifest: m.to_string(),
            script: "return 1".into(),
            author: "alice".into(),
            loop_override: false,
        })
        .unwrap_err();
    assert!(
        matches!(error, InstallError::ModelToolUntagged { .. }),
        "{error:?}"
    );
}

#[test]
fn ordinary_tool_call_makes_zero_tool_catalog_reads_at_runtime() {
    let world = World::new("ordinary-tool-policy");
    let rt = common::runtime(&world);
    let spec = world
        .spec(&rt, "return await ops.call('mock','echo',{n:7});")
        .unwrap();
    let reads = world.catalog.tool_contract_reads().len();
    assert!(reads > 0, "installation must capture provider metadata");
    let id = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    let run = finish(&rt, &world, &id);
    assert_eq!(result(&run), json!({"n":7}));
    assert_eq!(world.mock.total_sends(), 1);
    assert_eq!(
        world.catalog.tool_contract_reads().len() - reads,
        0,
        "ordinary calls must use the approved snapshot, not tool.catalog"
    );
}

#[test]
fn classifier_policy_is_bound_to_the_installed_version() {
    let (world, rt, provider, m, _) = fixture(0.30);
    let spec = world.spec_with(&rt,"return await ops.call('mock','echo',{model:'example/classifier',questions:{urgent:{type:'noul',instructions:'Is it urgent?'}},item:{state:'A kettle'},max_cost_usd:1});",&m).unwrap();
    world.catalog.set_tool_contract("mock", "echo", json!({}));
    let reads = world.catalog.tool_contract_reads().len();
    let id = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    finish(&rt, &world, &id);
    let input: Value =
        serde_json::from_str(provider.calls.lock().unwrap()[0].args.as_str()).unwrap();
    assert_eq!(input["max_cost_usd"], 0.30);
    assert_eq!(world.catalog.tool_contract_reads().len(), reads);
}

#[test]
fn migration_42_preserves_existing_versions_as_legacy_ordinary() {
    let world = World::new("legacy-tool-policy");
    let rt = common::runtime(&world);
    let spec = world
        .spec(&rt, "return await ops.call('mock','echo',{n:42});")
        .unwrap();
    // Removing only the new tables reproduces the pre-migration schema around
    // the recorded version; migration 42 must preserve its existing approval.
    rt.store()
        .write(|tx| {
            tx.execute_batch(
                "DROP TABLE install_tool_policies; DROP TABLE package_tool_policies;",
            )?;
            tx.execute_batch(
                basal_core::schema::MIGRATIONS
                    .iter()
                    .find(|m| m.version == 42)
                    .unwrap()
                    .statements,
            )?;
            let policies = basal_core::classify_cost::load(tx, "flow-test", 1)?;
            assert_eq!(
                policies[&(String::from("mock"), String::from("echo"))],
                basal_core::classify_cost::ToolPolicy::LegacyOrdinary
            );
            Ok(())
        })
        .unwrap();
    world.catalog.set_tool_contract(
        "mock",
        "echo",
        json!({"input_schema":{"required":["max_cost_usd"]}}),
    );
    let reads = world.catalog.tool_contract_reads().len();
    let id = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    let run = finish(&rt, &world, &id);
    assert_eq!(result(&run), json!({"n":42}));
    assert_eq!(world.mock.total_sends(), 1);
    assert_eq!(world.catalog.tool_contract_reads().len(), reads);
}

#[test]
fn absent_tool_policy_is_a_typed_refusal_without_peer_reads() {
    let world = World::new("missing-tool-policy");
    let rt = common::runtime(&world);
    let spec = world
        .spec(
            &rt,
            "try { return await ops.call('mock','echo',{}); } catch(e) { return e.data.code; }",
        )
        .unwrap();
    rt.store()
        .write(|tx| {
            tx.execute(
                "DELETE FROM install_tool_policies WHERE flow_id='flow-test'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    let reads = world.catalog.tool_contract_reads().len();
    let id = rt.admit(&spec).unwrap().run_id().unwrap().to_owned();
    assert_eq!(result(&finish(&rt, &world, &id)), "tool_policy_missing");
    assert_eq!(world.mock.total_sends(), 0);
    assert_eq!(world.catalog.tool_contract_reads().len(), reads);
}
