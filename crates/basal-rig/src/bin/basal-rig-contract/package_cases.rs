use super::*;

pub(super) async fn registration(rig: &Rig, tag: &str) -> Case {
    let mut case = Case::new("packages: registration is immutable and accepts only $self grants");
    let package = flows::package(&format!("rig-register-{tag}"));
    let registered = rig
        .client
        .basal_as_operator("package.register", install_params(&package))
        .await;
    case.check(
        "the operator registers an unapproved package version without a card",
        registered
            .as_ref()
            .is_ok_and(|r| r["package"] == package.id && r["version"] == 1 && r["new"] == true),
        evidence(&registered),
    );
    let again = rig
        .client
        .basal_as_operator("package.register", install_params(&package))
        .await;
    case.check(
        "registering the same exact bytes is idempotent",
        again.as_ref().is_ok_and(|r| {
            r["new"] == false
                && registered
                    .as_ref()
                    .is_ok_and(|first| r["code_hash"] == first["code_hash"])
        }),
        evidence(&again),
    );
    let mut conflict = package.clone();
    conflict.script.push_str("\n// different immutable bytes\n");
    let refused = rig
        .client
        .basal_as_operator("package.register", install_params(&conflict))
        .await;
    case.check(
        "different bytes cannot replace the registered version",
        code(&refused) == Some("package_version_conflict"),
        evidence(&refused),
    );
    let mut manifest = package.manifest_value();
    manifest["id"] = json!(format!("rig-foreign-package-{tag}"));
    manifest["sinks"][0]["agent"] = json!("NoSuchAgent");
    let refused = rig
        .client
        .basal_as_operator(
            "package.register",
            json!({"script":package.script,"manifest":manifest.to_string()}),
        )
        .await;
    case.check(
        "package registration refuses a literal foreign agent before approval",
        code(&refused) == Some("package_names_agent"),
        evidence(&refused),
    );
    case
}

fn persona_file(root: &std::path::Path, package: &str, pinned: bool) -> Result<(), String> {
    let rig_home = std::env::var_os("HOME").ok_or("HOME is absent")?;
    let projects = PathBuf::from(rig_home)
        .join("projects")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !root.starts_with(&projects) || root == projects {
        return Err("persona fixture is outside the isolated rig projects".into());
    }
    let bundle = root.join(".cortexkit/personas/alfonso");
    std::fs::create_dir_all(&bundle).map_err(|e| e.to_string())?;
    let refs = if pinned {
        format!("{{package: {package}, version: 1}}")
    } else {
        String::new()
    };
    std::fs::write(
        bundle.join("PERSONA.md"),
        format!("---\nname: alfonso\nflows: [{refs}]\n---\n## Isolated flow package\n"),
    )
    .map_err(|e| e.to_string())
}

pub(super) async fn view(rig: &Rig, agent: &Agent) -> Reply {
    rig.client
        .call(
            CORE,
            &agent.identity,
            "persona.check",
            json!({"selector":format!("agent:{}",agent.id)}),
        )
        .await
}

pub(super) async fn flush(rig: &Rig, agent: &Agent) -> Reply {
    rig.client.call(CORE, &agent.identity, "persona.flush", json!({"selector":format!("agent:{}",agent.id),"reason":"isolated flow package contract"})).await
}

pub(super) async fn lifecycle(rig: &Rig, agent: &Agent, args: &Args, tag: &str) -> Vec<Case> {
    let package = flows::package(&format!("rig-package-{tag}"));
    let id = basal_rig::packages::instance_id(&package.id, &agent.id);
    let mut setup = Case::new("packages: register, approve the full consent card, pin and flush");
    let source = args
        .project_root
        .join(format!(".cortexkit/flows/{}/1", package.id));
    let written = persona_file(&args.project_root, &package.id, true).and_then(|()| {
        std::fs::create_dir_all(&source).map_err(|e| e.to_string())?;
        std::fs::write(source.join("script.js"), &package.script).map_err(|e| e.to_string())?;
        std::fs::write(source.join("manifest.json"), &package.manifest).map_err(|e| e.to_string())
    });
    if !setup.check(
        "write exact package bytes and a persona version reference",
        written.is_ok(),
        json!(written),
    ) {
        return vec![setup];
    }
    let registered = rig
        .client
        .basal_as_operator("package.register", install_params(&package))
        .await;
    if !setup.check(
        "register the exact unapproved package version in basal",
        registered.as_ref().is_ok_and(|r| {
            r["package"] == package.id
                && r["version"] == 1
                && r["new"] == true
                && r["code_hash"].is_string()
        }),
        evidence(&registered),
    ) {
        return vec![setup];
    }
    let primary=rig.client.call(CORE,&BindIdentity::new(args.project_root.clone(),"opencode",&agent.session),"board.register_primary",json!({"harness":"opencode","session":agent.session,"callerDirectory":args.project_root,"callerIsPrimary":true})).await;
    setup.check(
        "bind the registered head's persona source to its rig project",
        primary.as_ref().is_ok_and(|r| r["applied"] == true),
        evidence(&primary),
    );
    let requested = flush(rig, agent).await;
    setup.check(
        "the first persona reference queues core's package approval request",
        requested.as_ref().is_ok_and(|r| {
            r["flows"][&agent.id].as_array().is_some_and(|f| {
                f.iter()
                    .any(|q| q["package"] == package.id && q["state"] == "queued")
            })
        }),
        evidence(&requested),
    );
    let card = poll(Duration::from_secs(30), || package_card(rig, &package.id)).await;
    if !setup.check(
        "the real consent provider lists and serves the package card",
        card.is_some(),
        json!(&card),
    ) {
        setup.record("persona_check", evidence(&view(rig, agent).await));
        setup.record(
            "pending_cards",
            evidence(&rig.client.operator("consent.list_pending", json!({})).await),
        );
        return vec![setup];
    }
    let (summary, card, pages) = card.unwrap();
    setup.record("package_summary", summary.clone());
    setup.record("full_package_card", card.clone());
    setup.record("list_pages", json!(pages));
    setup.check(
        "pending list omits source bodies; consent.get returns the exact registered bytes",
        summary["package"].get("script").is_none()
            && summary["package"].get("manifest_json").is_none()
            && card["kind"] == "package"
            && card["package"]["package"] == package.id
            && card["package"]["version"] == 1
            && card["package"]["script"] == package.script
            && card["package"]["manifest_json"] == package.manifest
            && registered
                .as_ref()
                .is_ok_and(|r| card["package"]["code_hash"] == r["code_hash"]),
        json!({"summary":summary,"full_card":card}),
    );
    setup.check(
        "basal has no instance before package approval",
        rig.basal.instance(&id) == Ok(None),
        json!(rig.basal.instance(&id)),
    );
    let answer = rig
        .client
        .operator(
            "consent.answer",
            json!({"elicitationId":card_id(&card),"choiceId":"allow"}),
        )
        .await;
    setup.check(
        "the attested phone answerer approves the full package card through cingulate",
        answer
            .as_ref()
            .is_ok_and(|r| r["ok"] == true && r["elicitation"]["state"] == "answered"),
        evidence(&answer),
    );
    // Cingulate's answer and core's consent intake are separate transactions.
    // Observe committed approval before the pin operation checks that gate.
    let approved = poll(Duration::from_secs(30), || async {
        let checked = view(rig, agent).await.ok()?;
        let ready = checked["roles"].as_array()?.iter().any(|role| {
            role["flows"].as_array().is_some_and(|flows| {
                flows.iter().any(|f| {
                    f["package"] == package.id
                        && f["version"] == 1
                        && f["approval_state"] == "approved"
                })
            })
        });
        ready.then_some(checked)
    })
    .await;
    if !setup.check(
        "persona.check observes core's committed package approval before pinning",
        approved.is_some(),
        json!(approved),
    ) {
        return vec![setup];
    }
    let pinned = rig
        .client
        .call(
            CORE,
            &agent.identity,
            "persona.flows.override",
            json!({"agent":agent.id,"package":package.id,"pin_version":1}),
        )
        .await;
    setup.check(
        "pin the approved exact version for the rig persona's agent",
        pinned.is_ok(),
        evidence(&pinned),
    );
    let flushed = flush(rig, agent).await;
    setup.check(
        "flush the approved rig persona",
        flushed.as_ref().is_ok_and(|r| r.get("error").is_none()),
        evidence(&flushed),
    );
    let checked = wait_instance(rig, agent, &package.id, false).await;
    let core = checked
        .as_ref()
        .and_then(|v| instance_view(v, &package.id))
        .unwrap_or(Value::Null);
    let basal = rig.basal.instance(&id);
    setup.record("persona_check", json!(&checked));
    setup.record(
        "package_intent",
        json!(rig.core.package_intent(&package.id)),
    );
    setup.check(
        "core confirmed flow.instance.ensure; basal holds the independently derived ID and owner",
        core["flow_id"] == id
            && core["agent_id"] == agent.id
            && core["confirmed_version"] == 1
            && core["generation"].as_i64().is_some_and(|g| g > 0)
            && basal.as_ref().is_ok_and(|b| {
                b.as_ref().is_some_and(|b| {
                    b["flow_id"] == id
                        && b["agent_id"] == agent.id
                        && b["package"] == package.id
                        && b["version"] == 1
                        && b["generation"] == core["generation"]
                        && b["operation"] == "ensure"
                        && b["removed"] == false
                        && b["state"] == "enabled"
                })
            }),
        json!({"core":core,"basal":basal,"expected_id":id}),
    );
    let ready = setup.passed();
    let mut cases = vec![setup];
    if !ready {
        return cases;
    }
    let mut activation =
        Case::new("packages: scheduled activation resolves $self to the owning agent");
    let flow = Flow {
        id: id.clone(),
        ..package.clone()
    };
    let run = rig.first_run(&flow).await;
    let result = run
        .as_ref()
        .and_then(|r| r.result.clone())
        .unwrap_or(Value::Null);
    activation.check(
        "the package runs successfully with the stable activation identity",
        run.as_ref().is_some_and(|r| r.state == "succeeded")
            && result["identity"] == json!({"agent_id":agent.id}),
        result.clone(),
    );
    let calls = rows(
        &mut activation,
        "read package sink journal",
        rig.basal.calls(&id),
    );
    activation.check(
        "$self and self.agent_id sink calls both carry the stable owner ID",
        [KIND_SINK_DIGEST, KIND_SINK_STATUS].iter().all(|kind| {
            calls.iter().any(|c| {
                c.kind_code == *kind
                    && c.request
                        .as_ref()
                        .is_some_and(|r| r["agent"] == agent.id && r["flow_id"] == id)
                    && c.dispatch == "sent"
            })
        }),
        json!(calls.iter().map(|c| &c.request).collect::<Vec<_>>()),
    );
    let receipts = rows(
        &mut activation,
        "read package receipts",
        rig.core.receipts(&id),
    );
    check_receipts(&mut activation, &flow, &calls, &receipts);
    activation.check(
        "the package digest landed in its own agent's core store",
        rig.core
            .digest_rows(&agent.id, &id)
            .as_ref()
            .is_ok_and(|v| {
                v.as_array().is_some_and(|r| {
                    r.iter()
                        .any(|r| r["fire_id"] == result["digest"]["fire_id"])
                })
            }),
        json!(rig.core.digest_rows(&agent.id, &id)),
    );
    cases.push(activation);
    finish_lifecycle(rig, agent, args, &package, &id, &mut cases).await;
    cases
}

pub(super) fn instance_view(view: &Value, package: &str) -> Option<Value> {
    view["roles"]
        .as_array()?
        .iter()
        .flat_map(|role| role["flow_instances"].as_array().into_iter().flatten())
        .find(|i| i["package"] == package)
        .cloned()
}

async fn wait_instance(rig: &Rig, agent: &Agent, package: &str, removed: bool) -> Option<Value> {
    poll(Duration::from_secs(30), || async {
        let view = view(rig, agent).await.ok()?;
        let row = instance_view(&view, package)?;
        (row["removed"] == removed
            && row["marker"].is_null()
            && (removed || row["confirmed_version"] == 1))
            .then_some(view)
    })
    .await
}

/// Listing is bounded and excludes source bodies; only get supplies the full card.
async fn package_card(rig: &Rig, package: &str) -> Option<(Value, Value, usize)> {
    let mut params = json!({});
    let mut seen = std::collections::HashSet::new();
    for page in 1..=100 {
        let listed = rig
            .client
            .operator("consent.list_pending", params)
            .await
            .ok()?;
        if serde_json::to_vec(&listed).ok()?.len() > 3 * 1024 * 1024 {
            return None;
        }
        if let Some(summary) = listed["records"].as_array()?.iter().find(|c| {
            c["kind"] == "package"
                && c["package"]["package"] == package
                && c["package"]["version"] == 1
        }) {
            let full = rig
                .client
                .operator("consent.get", json!({"elicitationId":card_id(summary)}))
                .await
                .ok()?;
            return Some((summary.clone(), full, page));
        }
        let next = listed.get("continuation")?.clone();
        if !seen.insert(next.to_string()) {
            return None;
        }
        params = json!({"continuation":next});
    }
    None
}

async fn finish_lifecycle(
    rig: &Rig,
    agent: &Agent,
    args: &Args,
    package: &Flow,
    id: &str,
    cases: &mut Vec<Case>,
) {
    use basal_rig::packages::same_enable_state;
    let mut remove =
        Case::new("packages: unpin removes the instance and preserves the enable switch");
    let before = rig.basal.instance(id).ok().flatten().unwrap_or(Value::Null);
    let unpinned = persona_file(&args.project_root, &package.id, false);
    remove.check(
        "remove the exact-version reference from the rig persona",
        unpinned.is_ok(),
        json!(unpinned),
    );
    let flushed = flush(rig, agent).await;
    remove.check(
        "flush the unpinned persona",
        flushed.as_ref().is_ok_and(|r| r.get("error").is_none()),
        evidence(&flushed),
    );
    let checked = wait_instance(rig, agent, &package.id, true).await;
    let after = rig.basal.instance(id).ok().flatten().unwrap_or(Value::Null);
    remove.check(
        "persona.check and basal agree the instance is removed, not disabled",
        checked
            .as_ref()
            .and_then(|v| instance_view(v, &package.id))
            .is_some_and(|i| i["removed"] == true)
            && after["removed"] == true
            && same_enable_state(&before, &after),
        json!({"persona_check":checked,"before":before,"after":after}),
    );
    let removed = remove.passed();
    cases.push(remove);
    if !removed {
        return;
    }
    cases.push(
        super::seeded::stale(rig, agent, &package.id, id, || {
            persona_file(&args.project_root, &package.id, true)
        })
        .await,
    );

    let mut revoke =
        Case::new("packages: version revocation halts scheduled runs without changing enablement");
    let before_runs = rows(
        &mut revoke,
        "read runs before restoring the instance",
        rig.basal.runs(id),
    );
    let corrected = rig
        .client
        .call(
            CORE,
            &agent.identity,
            "persona.flows.override",
            json!({"agent":agent.id,"package":package.id,"clear":true}),
        )
        .await;
    revoke.check(
        "request a corrective ensure after core learned the generation floor",
        corrected.as_ref().is_ok_and(|r| r.get("error").is_none()),
        evidence(&corrected),
    );
    let checked = wait_instance(rig, agent, &package.id, false).await;
    revoke.check(
        "core confirms the same instance is present and approved again",
        checked.is_some(),
        json!(&checked),
    );
    let run = poll(RUN_WAIT, || async {
        rig.basal
            .runs(id)
            .ok()?
            .into_iter()
            .find(|r| r.ended() && !before_runs.iter().any(|old| old.run_id == r.run_id))
    })
    .await;
    revoke.check(
        "the restored instance actually runs before revocation",
        run.as_ref().is_some_and(|r| r.state == "succeeded"),
        json!(
            run.as_ref().map(
                |r| json!({"run_id":r.run_id,"state":r.state,"result":r.result,"error":r.error})
            )
        ),
    );
    let before = rig.basal.instance(id).ok().flatten().unwrap_or(Value::Null);
    // Core's package-administration API grants operator authority to the session
    // registered as the primary board. Callosum's reserved module identity can
    // answer consent cards, but does not satisfy this separate authorization rule.
    let revoked = rig
        .client
        .call(
            CORE,
            &agent.identity,
            "persona.flows.revoke_version",
            json!({"package":package.id,"version":1}),
        )
        .await;
    revoke.check(
        "the primary board session revokes the approved package version",
        revoked.as_ref().is_ok_and(|r| r.get("error").is_none()),
        evidence(&revoked),
    );
    let checked = wait_instance(rig, agent, &package.id, true).await;
    let after = rig.basal.instance(id).ok().flatten().unwrap_or(Value::Null);
    revoke.check(
        "revocation withdraws authority while preserving the enabled switch",
        checked.is_some()
            && before["state"] == "enabled"
            && after["removed"] == true
            && same_enable_state(&before, &after),
        json!({"persona_check":checked,"before":before,"after":after}),
    );
    let prior = rows(
        &mut revoke,
        "read the settled run set after revocation",
        rig.basal.runs(id),
    );
    tokio::time::sleep(Duration::from_secs(65)).await;
    let later = rig.basal.runs(id);
    revoke.check(
        "no new package run is admitted across the next schedule boundary",
        later.as_ref().is_ok_and(|r| r.len() == prior.len()),
        json!(later.map(|r| {
            r.iter()
                .map(|r| json!({"run_id":r.run_id,"state":r.state}))
                .collect::<Vec<_>>()
        })),
    );
    cases.push(revoke);
}
