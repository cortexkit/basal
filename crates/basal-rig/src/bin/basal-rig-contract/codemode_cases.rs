//! Live codemode calls use the same core-owned selector as an OpenCode host.
//! The provider probe is real daemon traffic, not a fabricated tool context.

use super::*;

const PROBE: &str = "ckdev-runscope-probe";

fn request(code: &str, key: &str) -> Value {
    json!({"name":"codemode","call_key":key,"arguments":{"code":code}})
}

async fn closed(rig: &Rig, agent: &Agent, key: &str, case: &mut Case) -> Option<Value> {
    let admitted = rig.basal.codemode_call(key);
    case.check(
        "basal persisted the actual keyed run",
        admitted.as_ref().is_ok_and(|r| r.is_some()),
        json!(&admitted),
    );
    let admitted = admitted.ok()??;
    case.check(
        "basal's admitted agent is the invoking agent",
        admitted["agent_id"] == agent.id,
        admitted.clone(),
    );
    let run_id = admitted["run_id"].as_str()?;
    let scope = poll(Duration::from_secs(10), || async {
        rig.core
            .codemode_scope(run_id)
            .ok()?
            .filter(|s| s["lifecycle"] == "terminal" && s["end_settled"] == true)
    })
    .await;
    case.check(
        "core terminalized the run and settled its daemon scope end",
        scope.is_some(),
        json!({"scope":scope,"current":rig.core.codemode_scope(run_id)}),
    );
    let scope = scope?;
    case.record("owner_scope", scope.clone());
    case.check(
        "core's scope belongs to the same invoking agent",
        scope["agent_id"] == agent.id,
        scope.clone(),
    );
    let later = rig.client.tool_scoped(PROBE, &agent.identity, scope["selector"].clone(), json!({"name":"probe_echo","arguments":{"text":"must not arrive"},"call_key":format!("{key}-closed")})).await;
    case.check(
        "the daemon refuses a later call on the closed scope before probe dispatch",
        matches!(code(&later), Some("scope_ended" | "scope_not_live")),
        evidence(&later),
    );
    Some(json!({"run_id":run_id,"scope":scope}))
}

pub(super) async fn run(rig: &Rig, agent: &Agent, tag: &str) -> Vec<Case> {
    let mut success = Case::new("codemode: live probe, console and return value");
    let identity = rig
        .client
        .call(
            CORE,
            &agent.identity,
            "session.scope_identity",
            json!({"harness":"opencode","session":agent.session}),
        )
        .await;
    success.record("host_scope_identity", evidence(&identity));
    let selector = identity.as_ref().ok().and_then(|s| {
        Some(json!({"owner":s.get("owner")?,"ref":s.get("ref")?,"epoch":s.get("scope_epoch")?}))
    });
    if !success.check(
        "core returns a real agent selector for this registered host session",
        selector.is_some(),
        evidence(&identity),
    ) {
        return vec![success];
    }
    let selector = selector.unwrap();
    let missing = rig.client.tool_scoped(PROBE, &agent.identity, selector.clone(), json!({"name":"probe_echo","arguments":{"text":"no run"},"call_key":format!("rig-no-run-{tag}")})).await;
    success.check(
        "the probe refuses an agent scope without run_id",
        code(&missing) == Some("run_scope_required"),
        evidence(&missing),
    );
    let key = format!("rig-codemode-{tag}");
    let program = "const echoed = await tools.probe_echo({text: 'rig echo'}); console.log('rig console'); return {answer: 42, echoed};";
    let reply = rig
        .client
        .tool_scoped(
            "basal",
            &agent.identity,
            selector.clone(),
            request(program, &key),
        )
        .await;
    success.record("tool_reply", evidence(&reply));
    success.check(
        "the program completed and returned 42",
        reply
            .as_ref()
            .is_ok_and(|v| v["status"] == "completed" && v["value"]["answer"] == 42),
        evidence(&reply),
    );
    success.check(
        "console.log output is captured",
        reply.as_ref().is_ok_and(|v| {
            v["output"]
                .as_str()
                .is_some_and(|s| s.contains("rig console"))
        }),
        evidence(&reply),
    );
    if let Some(ended) = closed(rig, agent, &key, &mut success).await {
        let catalog = &ended["scope"]["catalog"];
        success.check(
            "core's open catalog contains the probe and excludes codemode",
            catalog.as_array().is_some_and(|entries| {
                entries
                    .iter()
                    .any(|e| e == &json!({"tool":"probe_echo","module":PROBE,"op":"probe_echo"}))
                    && entries.iter().all(|e| {
                        e["tool"] != "codemode"
                            && !(e["module"] == "basal" && e["op"] == "codemode")
                    })
            }),
            catalog.clone(),
        );
        success.check(
            "the probe received the actual agent_id and run_id in its daemon stamp",
            reply.as_ref().is_ok_and(|v| {
                v["value"]["echoed"]
                    == json!({"text":"rig echo","agent_id":agent.id,"run_id":ended["run_id"]})
            }),
            evidence(&reply),
        );
    }

    let mut cpu = Case::new("codemode: infinite loop exhausts CPU and closes scope");
    let key = format!("rig-codemode-cpu-{tag}");
    let reply = rig
        .client
        .tool_scoped(
            "basal",
            &agent.identity,
            selector,
            request("while (true) {}", &key),
        )
        .await;
    cpu.record("tool_reply", evidence(&reply));
    cpu.check(
        "an infinite loop ends with the JavaScript CPU budget refusal",
        reply
            .as_ref()
            .is_ok_and(|v| v["status"] == "budget_exhausted:js_cpu"),
        evidence(&reply),
    );
    closed(rig, agent, &key, &mut cpu).await;

    let ungranted = Case::skipped(
        "codemode: an ungranted worker is refused not_granted",
        "Prefrontal 16cd2fa48 grants codemode to every registered head; only real worker scopes lack it. Plugin worker registration requires an existing live task, which this rig does not launch. Core covers the refusal in codemode_scope::tests::workers_do_not_hold_the_codemode_grant; basal preserves it in refused_open_preserves_core_code_and_detail_without_worker. No worker scope is fabricated.",
    );
    vec![success, cpu, ungranted]
}
