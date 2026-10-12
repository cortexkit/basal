use super::*;

fn audience_flow(id: &str, audience: Value) -> Flow {
    Flow {
        id: id.into(),
        version: 1,
        manifest: json!({"format":1,"id":id,"version":1,
            "purpose":"Exercise install policy without dispatching any host calls.",
            "trigger":{"schedule":{"cron":"0 0 1 1 *"}},"audience":audience})
        .to_string(),
        script: "return 42;".into(),
    }
}

fn policy_approved(reply: &Value, persisted: &Value, agent: &str) -> bool {
    reply["state"] == "approved"
        && reply["approval"] == json!({"kind":"policy","rule":"self_install"})
        && reply["card_id"].is_null()
        && persisted["approved_by"] == "policy:self_install"
        && persisted["author_agent_id"] == agent
        && reply["code_hash"]
            .as_str()
            .is_some_and(|hash| hash.len() == 64)
        && persisted["code_hash"] == reply["code_hash"]
        && persisted["card_intents"] == 0
        && persisted["card_hints"] == 0
}

fn pending_cards_empty(reply: &Value) -> bool {
    reply["records"].as_array().is_some_and(Vec::is_empty)
        && reply.get("continuation").is_none_or(Value::is_null)
}

pub(super) async fn run(rig: &Rig, agent: &Agent, tag: &str) -> Result<Vec<Case>, String> {
    let mut policy =
        Case::new("install ladder: agent self-install is approved by policy without a card");
    let config = json!({"module_config":{"flows":{"install":{
        "self":"allow","workspace":"ask","llm_token_cap_ceiling":4096,"net_hosts":[]}}}});
    let configured = rig
        .client
        .call(
            CORE,
            &agent.identity,
            "manager.set_substrate_config",
            config.clone(),
        )
        .await;
    policy.record("policy_config_request", config);
    if !policy.check(
        "the registered host configures core's install policy in the isolated rig",
        configured.as_ref().is_ok_and(|v| v["ok"] == true),
        evidence(&configured),
    ) {
        return Err(format!(
            "cannot configure core install policy: {}",
            evidence(&configured)
        ));
    }
    let flow = audience_flow(
        &format!("rig-policy-self-{tag}"),
        json!({"kind":"agent","id":agent.id}),
    );
    let installed = rig.relay(agent, "install", install_params(&flow)).await;
    policy.record("install_request", install_params(&flow));
    policy.record("install_reply", installed.evidence());
    let persisted = rig.core.policy_install(&flow.id, flow.version);
    policy.check("core relays a policy-approved self install and persists matching policy provenance, without a card intent or decision hint",
        installed.reply().zip(persisted.as_ref().ok().and_then(Option::as_ref)).is_some_and(|(r,p)|policy_approved(r,p,&agent.id)),
        json!({"reply":installed.evidence(),"core":persisted}));
    let pending = rig.client.operator("consent.list_pending", json!({})).await;
    policy.check(
        "cingulate successfully lists a complete empty pending-card page after the policy install",
        pending.as_ref().is_ok_and(pending_cards_empty),
        evidence(&pending),
    );
    let enabled = rig.wait_enabled(&flow).await;
    policy.check(
        "basal enables the self flow with recorded self_install policy provenance",
        enabled
            .as_ref()
            .is_some_and(|v| v["approval"] == json!({"kind":"policy","rule":"self_install"})),
        json!(enabled),
    );
    policy.record("status_read_proof",json!({"live":"outcome and provenance only; no live request counter",
        "offline":"basal-module/tests/install_policy.rs::matching_policy_reply_and_active_status_approve_on_both_consent_paths proves exactly one flow.install_status read"}));
    let mut cases = vec![policy];
    for (kind, audience) in [
        ("workspace", json!({"kind":"workspace","id":"basal-rig"})),
        ("global", json!({"kind":"global"})),
    ] {
        let flow = audience_flow(&format!("rig-policy-{kind}-{tag}"), audience);
        let mut card = Case::new(&format!(
            "install ladder: {kind} install still raises a cingulate card"
        ));
        if install_with_route(
            rig,
            &mut card,
            &flow,
            InstallRoute::Operator,
            "approve",
            false,
        )
        .await
        .is_some()
        {
            let enabled = rig.wait_enabled(&flow).await;
            card.check(
                "basal enables the carded audience flow with card provenance",
                enabled
                    .as_ref()
                    .is_some_and(|v| v["approval"] == json!({"kind":"card"})),
                json!(enabled),
            );
        }
        cases.push(card);
    }
    Ok(cases)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_card_evidence_requires_a_complete_successful_list() {
        assert!(pending_cards_empty(&json!({"records":[]})));
        assert!(pending_cards_empty(
            &json!({"records":[],"continuation":null})
        ));
        for bad in [
            json!({}),
            json!({"records":null}),
            json!({"records":[],"continuation":"next"}),
            json!({"records":[{"elicitationId":"card"}]}),
        ] {
            assert!(!pending_cards_empty(&bad));
        }
    }

    #[test]
    fn policy_evidence_requires_matching_hash_provenance_and_no_card() {
        let hash = "a".repeat(64);
        let reply = json!({"state":"approved","code_hash":hash,"approval":{"kind":"policy","rule":"self_install"},"card_id":null});
        let core = json!({"code_hash":hash,"approved_by":"policy:self_install","author_agent_id":"agent","card_intents":0,"card_hints":0});
        assert!(policy_approved(&reply, &core, "agent"));
        for (field, value) in [
            ("approved_by", json!("card")),
            ("author_agent_id", json!("other")),
            ("code_hash", json!("b".repeat(64))),
            ("card_intents", json!(1)),
            ("card_hints", json!(1)),
        ] {
            let mut bad = core.clone();
            bad[field] = value;
            assert!(!policy_approved(&reply, &bad, "agent"), "{field}");
        }
        for (field, value) in [
            ("state", json!("pending")),
            ("approval", json!({"kind":"card"})),
            ("card_id", json!("el_1")),
            ("code_hash", json!("")),
        ] {
            let mut bad = reply.clone();
            bad[field] = value;
            assert!(!policy_approved(&bad, &core, "agent"), "{field}");
        }
    }
}
