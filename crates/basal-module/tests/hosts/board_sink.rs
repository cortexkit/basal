use super::*;

fn board_manifest() -> Value {
    json!({"id":"host-flow","version":1,"purpose":"Track work",
        "trigger":{"schedule":{"interval":"1h"}},
        "board":[{"agent":"SYNAPSE","title":"Work"}]})
}

#[test]
fn board_operations_pin_stamped_wire_bytes_and_receipts() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent, "board-wire");
    common::install(&f, &Caller::Operator,
        "const added = await sink.board.append('SYNAPSE','work');
         await sink.board.setState('ag_synapse',added.item_id,'blocked',{on:'ci',ref:'work:wi_example'});
         await sink.board.retire('SYNAPSE'); return added;", &board_manifest());
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "board-wire");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded);
    assert_eq!(
        serde_json::from_str::<Value>(done.result.as_ref().unwrap()).unwrap(),
        json!({"lane_id":"virtual:flow:host-flow","item_id":"item_1"})
    );
    let hash = fake.calls("flow.install_request")[0]["params"]["flow_install"]["code_hash"].clone();
    let sent = fake.calls("sink.board");
    assert_eq!(sent.len(), 3);
    let journal = f.module.rt.calls(&run).unwrap();
    for (position, operation) in [
        json!({"op":"append","text":"work"}),
        // Compare the serialized wait unchanged in the set_state request;
        // core, not basal's transport, validates board wait references.
        json!({"op":"set_state","item_id":"item_1","state":"blocked","wait":{"on":"ci","ref":"work:wi_example"}}),
        json!({"op":"retire"}),
    ].into_iter().enumerate() {
        let key = basal_core::ids::idempotency_key("host-flow", &run, position as u64);
        assert!(key.starts_with("bk1-"));
        assert_eq!(key, journal[position].idempotency_key);
        let expected = json!({"method":"sink.board","params":{
            "stamp":{"flow_id":"host-flow","flow_version":1,"code_hash":hash},
            "agent":"ag_synapse","receipt_key":key,"operation":operation}});
        assert_eq!(sent[position].to_string().as_bytes(), expected.to_string().as_bytes());
        let frozen: Value = serde_json::from_str(journal[position].request.as_ref().unwrap().as_str()).unwrap();
        assert_eq!(frozen["stamp"], expected["params"]["stamp"]);
        assert_eq!(frozen["operation"], expected["params"]["operation"]);
        assert_eq!(journal[position].class, basal_core::model::StoredClass::KeyedMutation);
    }
}

#[test]
fn board_target_refusal_is_unsent_and_final_refusals_reach_script() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent, "board-refused");
    common::install(
        &f,
        &Caller::Operator,
        "return await sink.board.append('SYNAPSE','work').catch(e => e.data.code);",
        &json!({"id":"host-flow","version":1,"purpose":"No board grant","trigger":{"schedule":{"interval":"1h"}}}),
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "board-refused");
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(
        f.module.rt.run(&run).unwrap().result.as_deref(),
        Some("\"sink_board_target_not_granted\"")
    );
    assert!(fake.calls("sink.board").is_empty());
    let row = &f.module.rt.calls(&run).unwrap()[0];
    assert_eq!(row.attempts, 0);
    assert_eq!(row.dispatch, basal_core::model::DispatchState::None);

    let host = CoreHost::new(fake.clone());
    for code in [
        "sink_board_target_not_granted",
        "sink_board_item_not_owned",
        "sink_board_wait_invalid",
        "sink_board_text_cap",
        "sink_board_item_cap",
        "sink_board_lane_cap",
        "sink_board_lane_missing",
        "sink_item_invalid",
        "sink_input_conflict",
        "sink_flow_not_installed",
        "sink_flow_hash_mismatch",
        "sink_principal_refused",
        "sink_target_not_in_audience",
        "sink_audience_check_required",
    ] {
        fake.enqueue(
            "sink.board",
            vec![Err(WireError::Refused {
                code: code.into(),
                message: "no".into(),
            })],
        );
        let Dispatched::Completed(outcome) = host
            .dispatch(&call(
                Primitive::SinkBoard,
                json!({"operation":{"op":"append"}}),
            ))
            .unwrap()
        else {
            panic!("completed")
        };
        assert_eq!(outcome.settlement, Settlement::Rejected);
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            code
        );
    }
}

#[test]
fn board_retryable_refusals_defer_with_same_receipt_until_success_or_deadline() {
    for (code, expire) in [
        ("sink_store_failed", false),
        ("sink_audience_unavailable", false),
        ("sink_store_failed", true),
    ] {
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let f = worker_fixture(fake.clone(), consent, "board-deferred");
        let mut manifest = board_manifest();
        manifest["deadline"] = json!("1m");
        common::install(
            &f,
            &Caller::Operator,
            "return await sink.board.append('SYNAPSE','work');",
            &manifest,
        );
        approve(&fake, &f.module);
        fake.enqueue(
            "sink.board",
            vec![Err(WireError::Refused {
                code: code.into(),
                message: "temporary".into(),
            })],
        );
        let run = common::admit(&f, "host-flow", "board-deferred");
        f.module.engine.run_until_idle(50).unwrap();
        let row = &f.module.rt.calls(&run).unwrap()[0];
        assert_eq!(row.dispatch, basal_core::model::DispatchState::Deferred);
        assert_eq!(fake.calls("sink.board").len(), 1);
        let (reason, retry): (String, i64) = f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT refusal,retry_not_before FROM journal WHERE run_id=?1",
                    [&run],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(reason, code);
        assert!(retry > common::T0 && retry <= common::T0 + 30_000);
        f.clock
            .set(if expire { common::T0 + 60_000 } else { retry });
        f.module.engine.run_until_idle(50).unwrap();
        if expire {
            assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Failed);
            assert_eq!(fake.calls("sink.board").len(), 1);
            let (settlement, value): (String, String) = f
                .module
                .rt
                .store()
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT settlement,value FROM mailbox WHERE run_id=?1 AND position=0",
                        [&run],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                })
                .unwrap();
            assert_eq!(settlement, "rejected");
            assert_eq!(serde_json::from_str::<Value>(&value).unwrap()["code"], code);
        } else {
            assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
            let sent = fake.calls("sink.board");
            assert_eq!(sent.len(), 2);
            assert_eq!(
                sent[0].to_string().as_bytes(),
                sent[1].to_string().as_bytes()
            );
        }
    }
}

#[test]
fn board_registry_failure_defers_before_send_and_resolves_once_on_retry() {
    for failure in [
        WireError::NeverSent("core unreachable".into()),
        WireError::TimedOut("registry timed out".into()),
        WireError::Unreadable("invalid registry reply".into()),
    ] {
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let f = worker_fixture(fake.clone(), consent, "board-registry-retry");
        common::install(
            &f,
            &Caller::Operator,
            "return await sink.board.append('ag_synapse','work');",
            &board_manifest(),
        );
        approve(&fake, &f.module);
        let run = common::admit(&f, "host-flow", "board-registry-retry");
        let reads = fake.calls("agent.list").len();
        fake.enqueue("agent.list", vec![Err(failure)]);
        f.module.engine.run_until_idle(50).unwrap();
        let row = f.module.rt.calls(&run).unwrap().remove(0);
        assert_eq!(row.dispatch, basal_core::DispatchState::Deferred);
        assert!(row.request.is_none());
        assert!(fake.calls("sink.board").is_empty());
        assert_eq!(fake.calls("agent.list").len(), reads + 1);
        let (reason, retry): (String, i64) = f
            .module
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT refusal,retry_not_before FROM journal WHERE run_id=?1",
                    [&run],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!(reason, "sink_board_target_unavailable");
        assert!(retry > common::T0 && retry <= common::T0 + 30_000);
        f.clock.set(retry);
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Succeeded);
        assert_eq!(fake.calls("agent.list").len(), reads + 2);
        let sent = fake.calls("sink.board");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["params"]["agent"], "ag_synapse");
        assert_eq!(sent[0]["params"]["receipt_key"], row.idempotency_key);
        let completed = f.module.rt.calls(&run).unwrap().remove(0);
        assert_eq!(completed.attempts, 2);
        let frozen: Value =
            serde_json::from_str(completed.request.as_ref().unwrap().as_str()).unwrap();
        assert_eq!(frozen["agent"], "ag_synapse");
        assert_eq!(frozen["stamp"], sent[0]["params"]["stamp"]);
        let bytes = completed.args.len()
            + completed.request.as_ref().unwrap().len()
            + completed.outcome.as_ref().unwrap().value.len();
        assert_eq!(
            f.module
                .rt
                .store()
                .read(|c| basal_core::journal::run_size(c, &run))
                .unwrap(),
            (1, bytes as u64)
        );
    }
}

#[test]
fn board_registry_answer_without_target_or_grant_is_final() {
    for target in ["MISSING", "ag_synapse"] {
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let f = worker_fixture(fake.clone(), consent, "board-registry-absent");
        common::install(
            &f,
            &Caller::Operator,
            &format!("return await sink.board.append('{target}','work').catch(e => e.data.code);"),
            &board_manifest(),
        );
        approve(&fake, &f.module);
        let run = common::admit(&f, "host-flow", "board-registry-absent");
        let reads = fake.calls("agent.list").len();
        if target == "ag_synapse" {
            fake.enqueue(
                "agent.list",
                vec![Ok(
                    json!({"agents":[{"agent_id":"ag_synapse","name":"RENAMED"}]}),
                )],
            );
        }
        f.module.engine.run_until_idle(50).unwrap();
        let done = f.module.rt.run(&run).unwrap();
        assert_eq!(done.state, RunState::Succeeded);
        assert_eq!(
            done.result.as_deref(),
            Some("\"sink_board_target_not_granted\"")
        );
        let row = f.module.rt.calls(&run).unwrap().remove(0);
        assert_eq!(row.dispatch, basal_core::DispatchState::None);
        assert_eq!(row.attempts, 0);
        assert_eq!(fake.calls("agent.list").len(), reads + 1);
        assert!(fake.calls("sink.board").is_empty());
    }
}

#[test]
fn board_registry_outage_backoff_stops_at_the_run_deadline() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent, "board-registry-deadline");
    let mut manifest = board_manifest();
    manifest["deadline"] = json!("1m");
    common::install(
        &f,
        &Caller::Operator,
        "return await sink.board.append('SYNAPSE','work');",
        &manifest,
    );
    approve(&fake, &f.module);
    let run = common::admit(&f, "host-flow", "board-registry-deadline");
    fake.enqueue(
        "agent.list",
        vec![
            Err(WireError::NeverSent("offline".into())),
            Err(WireError::NeverSent("offline".into())),
        ],
    );
    f.module.engine.run_until_idle(50).unwrap();
    let retry = || {
        f.module
            .rt
            .store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT retry_not_before FROM journal WHERE run_id=?1",
                    [&run],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap()
    };
    let first = retry();
    assert!(first > common::T0);
    f.clock.set(first);
    f.module.engine.run_until_idle(50).unwrap();
    let second = retry();
    assert_eq!(second - first, (first - common::T0) * 2);
    assert!(second <= common::T0 + 60_000);
    f.clock.set(common::T0 + 60_000);
    f.module.engine.run_until_idle(50).unwrap();
    assert_eq!(f.module.rt.run(&run).unwrap().state, RunState::Failed);
    assert!(fake.calls("sink.board").is_empty());
    let value: String = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT value FROM mailbox WHERE run_id=?1 AND position=0",
                [&run],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&value).unwrap()["code"],
        "sink_board_target_unavailable"
    );
}

#[test]
fn board_registry_batch_resolves_paged_names_and_exact_ids_together() {
    let fake = Fake::new();
    let catalog = SubcCatalog::new(fake.clone());
    fake.enqueue("agent.list", vec![
        Ok(json!({"agents":[{"agent_id":"agent_other","name":"agent_owner"}],"next_cursor":"next"})),
        Ok(json!({"agents":[{"agent_id":"agent_owner","name":"SYNAPSE"}]})),
    ]);
    assert_eq!(
        basal_host::Catalog::resolve_agents(&catalog, &["agent_owner", "SYNAPSE", "MISSING"])
            .unwrap(),
        [Some("agent_owner".into()), Some("agent_owner".into()), None]
    );
    let calls = fake.calls("agent.list");
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["params"], json!({}));
    assert_eq!(calls[1]["params"], json!({"cursor":"next"}));
}

#[test]
fn board_replies_reject_malformed_append_and_nonobject_updates() {
    let fake = Fake::new();
    let host = CoreHost::new(fake.clone());
    for (op, reply, valid) in [
        ("append", json!({"lane_id":"l","item_id":"i"}), true),
        ("append", json!({"lane_id":"l"}), false),
        ("append", json!({"lane_id":1,"item_id":"i"}), false),
        (
            "append",
            json!({"lane_id":"l","item_id":"i","extra":true}),
            false,
        ),
        ("set_state", json!({"future":"field"}), true),
        ("retire", json!({}), true),
        ("set_state", json!(true), false),
        ("retire", json!(null), false),
    ] {
        fake.enqueue("sink.board", vec![Ok(reply)]);
        assert_eq!(
            host.dispatch(&call(Primitive::SinkBoard, json!({"operation":{"op":op}})))
                .is_ok(),
            valid,
            "{op}"
        );
    }
}

#[test]
fn board_self_and_audience_targets_use_stable_ids_and_preserve_final_codes() {
    for audience in [false, true] {
        let fake = Fake::new();
        let consent = Arc::new(CoreConsent::new(fake.clone()));
        let f = worker_fixture(fake.clone(), consent, "board-self-audience");
        let mut m = board_manifest();
        m["board"][0]["agent"] = json!(if audience { "$audience" } else { "$self" });
        if audience {
            m["audience"] = json!({"kind":"global"});
        }
        let flow_id = if audience {
            common::install(
                &f,
                &Caller::Operator,
                "return await sink.board.append('SYNAPSE','work').catch(e => e.data.code);",
                &m,
            );
            approve(&fake, &f.module);
            "host-flow".to_owned()
        } else {
            f.module.handle(&Caller::Core, "package.register", json!({
                "script":"return await sink.board.append('$self','work').catch(e => e.data.code);",
                "manifest":m.to_string()
            })).unwrap();
            f.module
                .handle(
                    &Caller::Core,
                    "flow.instance.ensure",
                    json!({
                        "package":"host-flow","version":1,"agent_id":"ag_synapse","generation":1
                    }),
                )
                .unwrap();
            basal_core::packages::instance_id("host-flow", "ag_synapse")
        };
        fake.enqueue(
            "sink.board",
            vec![Err(WireError::Refused {
                code: "sink_board_item_not_owned".into(),
                message: "no".into(),
            })],
        );
        let run = common::admit(&f, &flow_id, "board-self-audience");
        f.module.engine.run_until_idle(50).unwrap();
        assert_eq!(
            f.module.rt.run(&run).unwrap().result.as_deref(),
            Some("\"sink_board_item_not_owned\"")
        );
        let calls = fake.calls("sink.board");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["params"]["agent"], "ag_synapse");
    }
}

#[test]
fn board_dry_runs_capture_all_operations_and_render_lane_text() {
    let fake = Fake::new();
    let consent = Arc::new(CoreConsent::new(fake.clone()));
    let f = worker_fixture(fake.clone(), consent, "board-dry");
    let m = board_manifest();
    common::install(
        &f,
        &Caller::Operator,
        "await sink.board.append('SYNAPSE','work').catch(()=>null); await sink.board.setState('SYNAPSE','item_1','done').catch(()=>null); await sink.board.retire('SYNAPSE').catch(()=>null);",
        &m,
    );
    for mode in ["capture", "live"] {
        let result = f
            .module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({
                    "flow_id":"host-flow","mode":mode,"trigger":{"kind":"test"}
                }),
            )
            .unwrap();
        let summary = &result["summary"];
        let calls = summary["runs"][0]["calls"].as_array().unwrap();
        assert_eq!(calls.len(), 3, "{result}");
        for call in calls {
            assert_eq!(call["kind"], "sink.board");
            assert_eq!(call["action"], "captured");
            assert_eq!(call["outcome"]["rejected"], "captured");
        }
        let parsed = basal_core::manifest::Manifest::parse(&m.to_string()).unwrap();
        let text = basal_core::dry_run_text::render(&parsed, summary);
        assert_eq!(
            text.would,
            [
                "Add an item to its board lane for 'SYNAPSE'",
                "Set an item's state in its board lane for 'SYNAPSE'",
                "Retire its board lane for 'SYNAPSE'"
            ]
        );
        assert!(text.problems.is_empty());
        assert!(!text.summary.contains("stopped at"));
        assert!(fake.calls("sink.board").is_empty());
    }
}
