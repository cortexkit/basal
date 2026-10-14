mod common;

use std::sync::{Arc, Mutex};

use basal_core::{Boundary, Hooks, RunState, Step};
use basal_host::InstallStatus;
use basal_module::caller::Caller;
use basal_proto::Settlement;
use serde_json::{Value, json};

type Action = Box<dyn FnOnce() + Send>;

struct RevokeAt {
    boundary: Boundary,
    action: Mutex<Option<Action>>,
}

impl Hooks for RevokeAt {
    fn at(&self, _: &str, boundary: &Boundary) -> Step {
        if *boundary == self.boundary
            && let Some(action) = self.action.lock().unwrap().take()
        {
            action();
        }
        Step::Continue
    }
}

fn fixture(tag: &str, boundary: Boundary) -> (common::Fixture, Arc<RevokeAt>) {
    let hook = Arc::new(RevokeAt {
        boundary,
        action: Mutex::new(None),
    });
    let f = common::fixture(
        tag,
        common::Options {
            hooks: hook.clone(),
            ..common::Options::with_worker()
        },
    );
    (f, hook)
}

fn arm(f: &common::Fixture, hook: &RevokeAt, flow: &str, hash: &str) {
    let rt = f.module.rt.clone();
    let host = f.mock.clone();
    let flow = flow.to_owned();
    let hash = hash.to_owned();
    *hook.action.lock().unwrap() = Some(Box::new(move || {
        host.set_install_status(
            &flow,
            2,
            Some(InstallStatus::Revoked {
                code_hash: hash.clone(),
            }),
        );
        rt.install_revoked(&flow, 2, &hash).unwrap();
    }));
}

fn manifest(id: &str) -> Value {
    json!({"id":id,"version":2,"purpose":"Verify live revocation","trigger":{"schedule":{"interval":"1h"}},
        "ops":[{"module":"mock","op":"echo"},{"module":"mock","op":"send"}]})
}

fn health(f: &common::Fixture, flow: &str) -> Value {
    let h = f
        .module
        .handle(&Caller::Operator, "flow.health", json!({}))
        .unwrap();
    h["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["flow_id"] == flow)
        .unwrap()
        .clone()
}

fn assert_revoked(f: &common::Fixture, run: &str) {
    let stored = f.module.rt.run(run).unwrap();
    assert_eq!(stored.state, RunState::Failed, "{stored:?}");
    assert_eq!(stored.error_kind.as_deref(), Some("version_revoked"));
    let calls = f.module.rt.calls(run).unwrap();
    assert_eq!(calls.len(), 2, "{calls:?}");
    let refused = calls[1].outcome.as_ref().expect("typed refusal");
    assert_eq!(refused.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<Value>(refused.value.as_str()).unwrap()["code"],
        "version_revoked"
    );
}

#[test]
fn an_unrevoked_run_dispatches_local_and_remote_calls() {
    let f = common::fixture("push-unrevoked", common::Options::with_worker());
    let script = "await kv.set('live',true); return await ops.call('mock','echo',{live:true});";
    let m = manifest("live-flow");
    common::install_approved(&f, &Caller::Operator, script, &m);
    let sends = f.mock.total_sends();
    let run = common::admit(&f, "live-flow", "one");
    f.module.engine.run_until_idle(20).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
    assert_eq!(f.mock.total_sends() - sends, 1);
    assert_eq!(f.module.rt.calls(&run).unwrap().len(), 2);
}

#[test]
fn push_mid_script_stops_the_next_call_and_health_does_not_count_a_failure() {
    let (f, hook) = fixture(
        "push-mid-script",
        Boundary::HostCallReceived { position: 1 },
    );
    let script = "await ops.call('mock','echo',{}); try { await ops.call('mock','echo',{}); } catch (_) {} return await ops.call('mock','echo',{});";
    let m = manifest("push-flow");
    common::install_approved(&f, &Caller::Operator, script, &m);
    let hash = basal_core::ids::hex(&basal_core::ids::code_hash(script, &m.to_string()));
    let run = common::admit(&f, "push-flow", "one");
    let sends = f.mock.total_sends();
    arm(&f, &hook, "push-flow", &hash);
    f.module.engine.run_until_idle(20).unwrap();
    assert_revoked(&f, &run);
    assert_eq!(f.mock.total_sends() - sends, 1);
    assert_eq!(f.mock.install_queries(), vec![("push-flow".into(), 2)]);
    let h = health(&f, "push-flow");
    assert_eq!(h["last_run"]["error_kind"], "version_revoked");
    assert_eq!(h["consecutive_failures"], 0);
    assert_eq!(h["auto_disabled"], false);
    assert!(f.module.rt.decision_cards().unwrap().is_empty());
}

#[test]
fn sent_call_settles_before_the_next_call_is_refused() {
    let (f, hook) = fixture("push-sent-call", Boundary::HostAnswered { position: 0 });
    let script =
        "await ops.call('mock','send',{value:7}); return await ops.call('mock','echo',{});";
    let m = manifest("sent-flow");
    common::install_approved(&f, &Caller::Operator, script, &m);
    let run = common::admit(&f, "sent-flow", "one");
    let effects = f.mock.effects().len();
    arm(
        &f,
        &hook,
        "sent-flow",
        &basal_core::ids::hex(&basal_core::ids::code_hash(script, &m.to_string())),
    );
    f.module.engine.run_until_idle(20).unwrap();
    assert_revoked(&f, &run);
    let first = &f.module.rt.calls(&run).unwrap()[0];
    assert_eq!(
        first.outcome.as_ref().unwrap().settlement,
        Settlement::Fulfilled
    );
    assert_eq!(first.outcome.as_ref().unwrap().delivery_order, 0);
    assert_eq!(f.mock.effects().len() - effects, 1);
}

#[test]
fn push_between_journal_and_send_refuses_the_dispatch_attempt() {
    let (f, hook) = fixture("push-before-send", Boundary::CallCommitted { position: 0 });
    let script = "return await ops.call('mock','echo',{});";
    let m = manifest("unsent-flow");
    common::install_approved(&f, &Caller::Operator, script, &m);
    let run = common::admit(&f, "unsent-flow", "one");
    let sends = f.mock.total_sends();
    arm(
        &f,
        &hook,
        "unsent-flow",
        &basal_core::ids::hex(&basal_core::ids::code_hash(script, &m.to_string())),
    );
    f.module.engine.run_until_idle(20).unwrap();
    assert_eq!(
        f.module.rt.run(&run).unwrap().error_kind.as_deref(),
        Some("version_revoked")
    );
    assert_eq!(f.mock.total_sends(), sends);
    let calls = f.module.rt.calls(&run).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(calls[0].outcome.as_ref().unwrap().value.as_str()).unwrap()["code"],
        "version_revoked"
    );
}

#[test]
fn template_and_package_instances_stop_every_call_family_without_changing_enablement() {
    const OWNER: &str = "agent_47120287c700722b";
    for (n, next) in [
        "ops.call('mock','echo',{})",
        "kv.set('after',true)",
        "random()",
        "now()",
        "fs.read('/tmp/revoked-file')",
        "facts('ALF',{})",
        "sink.digest('ALF',{title:'after'})",
        "llm({prompt:'after'})",
        "classify('after',['yes','no'])",
    ]
    .iter()
    .enumerate()
    {
        for template in [false, true] {
            let (f, hook) = fixture(
                &format!("push-instance-{n}-{template}"),
                Boundary::HostCallReceived { position: 1 },
            );
            f.catalog.add_named_agent(OWNER, "Owner");
            let script = format!(
                "await ops.call('mock','echo',{{}}); await {next}; return await ops.call('mock','echo',{{}});"
            );
            let mut m = manifest("push-package");
            if template {
                m["params"] = json!({"note":{"type":"string","max_bytes":64}});
                m["template"] = json!({"max_per_agent":2,"max_live":3,"max_lifetime":"1d"});
            }
            f.module
                .handle(
                    &Caller::Operator,
                    "package.register",
                    json!({"script":script,"manifest":m.to_string()}),
                )
                .unwrap();
            let reply = if template {
                f.module.handle(&common::agent(OWNER), "flow.instantiate", json!({"template":"push-package","version":2,"params":{"note":"test"},"expires_in":"1h"})).unwrap()
            } else {
                f.module.handle(&Caller::Core, "flow.instance.ensure", json!({"package":"push-package","version":2,"agent_id":OWNER,"generation":1})).unwrap()
            };
            let flow = reply["flow_id"].as_str().unwrap();
            let before = f.module.rt.flow(flow).unwrap().unwrap();
            let run = common::admit(&f, flow, "one");
            arm(
                &f,
                &hook,
                flow,
                &basal_core::ids::hex(&basal_core::ids::code_hash(&script, &m.to_string())),
            );
            f.module.engine.run_until_idle(20).unwrap();
            assert_revoked(&f, &run);
            assert_eq!(f.mock.total_sends(), 1, "{next}");
            assert!(f.mock.builtin_sends().is_empty());
            let after = f.module.rt.flow(flow).unwrap().unwrap();
            assert_eq!(after.enabled, before.enabled);
            assert_eq!(after.disabled_by, before.disabled_by);
            assert_eq!(after.disabled_reason, before.disabled_reason);
            assert_eq!(health(&f, flow)["consecutive_failures"], 0);
            let writes: i64 = f
                .module
                .rt
                .store()
                .read(|c| {
                    Ok(
                        c.query_row("SELECT count(*) FROM kv WHERE flow_id=?1", [flow], |r| {
                            r.get(0)
                        })?,
                    )
                })
                .unwrap();
            assert_eq!(writes, 0);
        }
    }
}
