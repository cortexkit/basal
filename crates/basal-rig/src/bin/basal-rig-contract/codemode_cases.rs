//! Live codemode calls use the same core-owned selector as an OpenCode host.
//! The provider probe is real daemon traffic, not a fabricated tool context.

use super::*;

const PROBE: &str = "ckdev-runscope-probe";

fn request(code: &str, key: &str) -> Value {
    json!({"name":"codemode","call_key":key,"arguments":{"code":code}})
}

async fn onboard(rig: &Rig, agent: &Agent, args: &Args, case: &mut Case) -> bool {
    let project = args.project_root.to_string_lossy();
    let params = json!({"profileID":"default","projectKey":project});
    let phase = rig
        .client
        .call(CORE, &agent.identity, "init.resolve_phase", params.clone())
        .await;
    if !case.check(
        "core resolves the rig project's onboarding phase",
        phase
            .as_ref()
            .is_ok_and(|v| matches!(v.as_str(), Some("head" | "initiator_p1" | "initiator_p2"))),
        evidence(&phase),
    ) {
        return false;
    }
    if phase.as_ref().is_ok_and(|v| v == "initiator_p1") {
        let global = rig
            .client
            .call(
                CORE,
                &agent.identity,
                "init.save_global",
                json!({"profileID":"default","captured":{}}),
            )
            .await;
        if !case.check(
            "core completes global onboarding through its real API",
            global.is_ok(),
            evidence(&global),
        ) {
            return false;
        }
    }
    if !phase.as_ref().is_ok_and(|v| v == "head") {
        // The hire API stores a selected model in its handoff; it does not
        // call that model. No host consumes this isolated rig's handoff.
        let hired = rig.client.call(CORE, &agent.identity, "init.hire_head_with_handoff", json!({
            "profileID":"default","projectKey":project,"captured":{},
            "headName":agent.name,"headModel":"openai/gpt-6-luna",
            "headPersonaRef":"head@generic","headBrief":"Exercise the isolated basal codemode contract.",
            "sessionID":agent.session,"parkedRequest":""
        })).await;
        if !case.check(
            "core completes project onboarding and creates the real handoff",
            hired.as_ref().is_ok_and(|v| {
                v["state"]["state"] == "done" && v["handoff"]["handoffID"].is_string()
            }),
            evidence(&hired),
        ) {
            return false;
        }
        let handoff = &hired.as_ref().unwrap()["handoff"]["handoffID"];
        let delivered = rig
            .client
            .call(
                CORE,
                &agent.identity,
                "init.mark_handoff_delivered",
                json!({"handoffID":handoff}),
            )
            .await;
        if !case.check(
            "the rig settles its onboarding handoff without launching a host",
            delivered.as_ref().is_ok_and(|v| v == true),
            evidence(&delivered),
        ) {
            return false;
        }
    }
    let ready = rig
        .client
        .call(CORE, &agent.identity, "init.resolve_phase", params)
        .await;
    case.check(
        "core now reports Head, the prerequisite for codemode",
        ready.as_ref().is_ok_and(|v| v == "head"),
        evidence(&ready),
    )
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

pub(super) async fn run(rig: &Rig, agent: &Agent, args: &Args, tag: &str) -> Vec<Case> {
    let mut setup = Case::new("codemode: real onboarding with no model runs");
    let before = rig.broca.run_count();
    setup.check(
        "read Broca's real run index before onboarding",
        before.is_ok(),
        json!(&before),
    );
    if !onboard(rig, agent, args, &mut setup).await {
        return vec![setup];
    }
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
        let scope: subc_protocol::scope::ScopeSelector = serde_json::from_value(s.clone()).ok()?;
        scope.scope_epoch.map(|_| s.clone())
    });
    if !success.check(
        "core returns a real agent selector for this registered host session",
        selector.is_some(),
        evidence(&identity),
    ) {
        return vec![setup, success];
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
        code(&reply) == Some("budget_exhausted:js_cpu"),
        evidence(&reply),
    );
    closed(rig, agent, &key, &mut cpu).await;
    let after = rig.broca.run_count();
    setup.check(
        "onboarding and both codemode programs started no Broca model run",
        before
            .as_ref()
            .is_ok_and(|count| after.as_ref() == Ok(count)),
        json!({"before":before,"after":after}),
    );

    let ungranted = Case::skipped(
        "codemode: an ungranted worker is refused not_granted",
        "Prefrontal 16cd2fa48 grants codemode to every registered head; only real worker scopes lack it. Plugin worker registration requires an existing live task, which this rig does not launch. Core covers the refusal in codemode_scope::tests::workers_do_not_hold_the_codemode_grant; basal preserves it in refused_open_preserves_core_code_and_detail_without_worker. No worker scope is fabricated.",
    );
    vec![setup, success, cpu, ungranted]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_call_uses_the_protocol_tool_request_not_a_management_envelope() {
        let call: subc_protocol::tool_call::ToolCallRequest =
            serde_json::from_value(request("return 42;", "rig-key")).unwrap();
        assert_eq!(call.name, "codemode");
        assert_eq!(call.arguments, json!({"code":"return 42;"}));
        assert_eq!(call.call_key.as_deref(), Some("rig-key"));
    }
}
