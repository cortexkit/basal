use super::*;

#[test]
fn card_settlement_requires_core_row_delivered_hint_and_basal_decision() {
    let core = json!({"state":"executed","code_hash":"hash","installed_hash":"hash","approved_at_ms":10,
        "hint_state":"executed","hint_delivered_at_ms":11});
    let basal =
        json!({"state":"approved","decided_at_ms":11,"approved_version":1,"flow_state":"enabled"});
    assert!(card_decision_settled(
        "approve", "hash", 1, false, &core, &basal
    ));
    for (field, value) in [
        ("state", json!("sent")),
        ("code_hash", json!("other")),
        ("installed_hash", Value::Null),
        ("approved_at_ms", Value::Null),
        ("hint_state", json!("declined")),
        ("hint_delivered_at_ms", Value::Null),
    ] {
        let mut bad = core.clone();
        bad[field] = value;
        assert!(
            !card_decision_settled("approve", "hash", 1, false, &bad, &basal),
            "core {field}"
        );
    }
    for (field, value) in [
        ("state", json!("pending")),
        ("decided_at_ms", Value::Null),
        ("approved_version", json!(2)),
        ("flow_state", json!("unapproved")),
    ] {
        let mut bad = basal.clone();
        bad[field] = value;
        assert!(
            !card_decision_settled("approve", "hash", 1, false, &core, &bad),
            "basal {field}"
        );
    }
    let mut paused = basal.clone();
    paused["flow_state"] = json!("disabled");
    assert!(card_decision_settled(
        "approve", "hash", 1, true, &core, &paused
    ));
    assert!(!card_decision_settled(
        "approve", "hash", 1, false, &core, &paused
    ));
    let declined_core = json!({"state":"declined","code_hash":"hash","installed_hash":null,"hint_state":"declined","hint_delivered_at_ms":12});
    let declined_basal = json!({"state":"declined","decided_at_ms":12,"approved_version":null,"flow_state":"enabled"});
    assert!(card_decision_settled(
        "decline",
        "hash",
        1,
        false,
        &declined_core,
        &declined_basal
    ));
    let mut rejected = declined_basal.clone();
    rejected["state"] = json!("rejected");
    assert!(!card_decision_settled(
        "decline",
        "hash",
        1,
        false,
        &declined_core,
        &rejected
    ));
}

#[test]
fn install_card_matching_uses_the_exact_intent_not_the_latest_pending_card() {
    let intent = json!({"flow_id":"flow","version":1,"attempt":2,"code_hash":"hash","elicitation_id":"el_exact"});
    let card = json!({"elicitationId":"el_exact","kind":"flow_install","flowInstall":{"flow_id":"flow","version":1,"code_hash":"hash"}});
    assert!(install_card_matches(&card, &intent));
    // The surface omits attempt. A different ID can be another attempt with
    // otherwise identical visible fields, so matching only the body is unsafe.
    let mut other_attempt = card.clone();
    other_attempt["elicitationId"] = json!("el_newer_attempt");
    assert!(!install_card_matches(&other_attempt, &intent));
    for (field, wrong) in [
        ("flow_id", json!("other")),
        ("version", json!(2)),
        ("code_hash", json!("changed")),
    ] {
        let mut wrong_card = card.clone();
        wrong_card["flowInstall"][field] = wrong;
        assert!(!install_card_matches(&wrong_card, &intent), "{field}");
    }
    let mut wrong_kind = card.clone();
    wrong_kind["kind"] = json!("package");
    assert!(!install_card_matches(&wrong_kind, &intent));
    for field in [
        "flow_id",
        "version",
        "attempt",
        "code_hash",
        "elicitation_id",
    ] {
        let mut missing = intent.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(!install_card_matches(&card, &missing), "{field}");
    }
}

#[test]
fn install_card_checks_match_cingulate_request_fixture() {
    let request: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/flow-install-card-request.json"
    ))
    .unwrap();
    // Cingulate records the request in snake_case but serves the live card in camelCase.
    let mut card = request.clone();
    let install_payload = card
        .as_object_mut()
        .unwrap()
        .remove("flow_install")
        .unwrap();
    card["flowInstall"] = install_payload;
    let install = &request["flow_install"];
    let flow = Flow {
        id: install["flow_id"].as_str().unwrap().into(),
        version: install["version"].as_i64().unwrap(),
        manifest: install["manifest_json"].as_str().unwrap().into(),
        script: install["script"].as_str().unwrap().into(),
    };
    let calculated_hash =
        basal_core::ids::hex(&basal_core::ids::code_hash(&flow.script, &flow.manifest));
    assert_eq!(install["code_hash"], calculated_hash);
    assert_eq!(
        request["facts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|fact| fact["label"] == "Code hash")
            .unwrap()["value"],
        calculated_hash
    );

    let mut case = Case::new("install card request fixture");
    check_rendered(&mut case, &card, &flow, &request);
    assert!(case.passed(), "{:#?}", case.to_json());

    let failed_check = |case: &Case, name: &str| {
        case.checks
            .iter()
            .any(|check| check["check"] == name && check["passed"] == false)
    };
    let mut wrong_flow = card.clone();
    wrong_flow["facts"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|fact| fact["label"] == "Flow")
        .unwrap()["value"] = json!("different-flow");
    let mut wrong_flow_case = Case::new("wrong flow fact");
    check_rendered(&mut wrong_flow_case, &wrong_flow, &flow, &request);
    assert!(failed_check(
        &wrong_flow_case,
        "card fact 'Flow' matches the manifest as plain text"
    ));

    let mut changed_source = card.clone();
    changed_source["source"]["text"] = json!("return 42; ");
    let mut changed_source_case = Case::new("changed source text");
    check_rendered(&mut changed_source_case, &changed_source, &flow, &request);
    assert!(failed_check(
        &changed_source_case,
        "source.text is byte-equal to the script basal sent"
    ));

    let mut placeholder = card.clone();
    placeholder["facts"].as_array_mut().unwrap().push(json!({
        "label": "Optional fact",
        "value": "not stated"
    }));
    let mut placeholder_case = Case::new("fact without a value");
    check_rendered(&mut placeholder_case, &placeholder, &flow, &request);
    assert!(failed_check(
        &placeholder_case,
        "facts without a value are omitted instead of shown as a placeholder"
    ));

    let mut changed_dry_run_text = card.clone();
    changed_dry_run_text["dry_run_text"]["summary"] = json!("different by one byte ");
    let mut changed_dry_run_case = Case::new("changed dry-run text");
    check_rendered(
        &mut changed_dry_run_case,
        &changed_dry_run_text,
        &flow,
        &request,
    );
    assert!(failed_check(
        &changed_dry_run_case,
        "dry_run_text is byte-equal at the request's top level"
    ));
}

#[test]
fn report_builder_fails_unmarked_zero_check_cases() {
    let mut empty = Case::new("empty");
    empty.finish();
    assert_eq!(empty.checks.len(), 1);
    assert_eq!(empty.to_json()["passed"], false);
    assert_eq!(empty.checks[0]["passed"], false);
    let mut skipped = Case::skipped("skipped", "no prerequisite result");
    skipped.finish();
    assert!(skipped.checks.is_empty());
    assert_eq!(skipped.to_json()["passed"], Value::Null);
    let mut checked = Case::new("checked");
    checked.check("real check", true, Value::Null);
    checked.finish();
    assert_eq!(checked.checks.len(), 1);
    assert_eq!(checked.to_json()["passed"], true);
}

#[test]
fn runtime_owner_evidence_requires_both_exact_core_refusals() {
    let refused = json!({"refused":{"data":{"code":"sink_target_not_owner"}}});
    let good = json!({"digest":refused,"status":refused});
    assert!(ownership::refused_by_core(&good));
    for sink in ["digest", "status"] {
        for code in [
            "denied",
            "sink_target_not_granted",
            "sink_flow_not_installed",
            "transport",
        ] {
            let mut bad = good.clone();
            bad[sink]["refused"]["data"]["code"] = json!(code);
            assert!(!ownership::refused_by_core(&bad), "{sink}: {code}");
        }
        let mut missing = good.clone();
        missing.as_object_mut().unwrap().remove(sink);
        assert!(!ownership::refused_by_core(&missing));
    }
}

#[test]
fn model_abort_records_every_named_case_without_losing_completed_evidence() {
    let mut completed = Case::new(MODEL_CASES[0]);
    completed.check("completed check", true, json!("saved"));
    let mut cases = vec![completed];
    report_model_abort(&mut cases, "unreadable checkpoint");
    assert_eq!(cases.len(), MODEL_CASES.len());
    assert!(cases[0].passed());
    for name in MODEL_CASES {
        assert_eq!(cases.iter().filter(|case| case.name == name).count(), 1);
    }
    assert!(cases[1..].iter().all(|case| {
        case.not_run
            .as_ref()
            .is_some_and(|why| why.contains("unreadable checkpoint"))
    }));
}

#[test]
fn failed_store_read_is_not_empty_evidence() {
    let mut case = Case::new("required store read");
    let records = rows::<Call>(&mut case, "read journal", Err("unreadable store".into()));
    case.check("no call was recorded", records.is_empty(), Value::Null);
    assert!(!case.passed(), "unreadable evidence was treated as absence");
}

#[test]
fn status_receipts_must_match_the_agent_and_revision() {
    let flow = flows::sinks("flow", "Agent", "Other");
    let call = Call {
        run_id: "run".into(),
        position: 0,
        kind_code: KIND_SINK_STATUS,
        dispatch: "sent".into(),
        attempts: 1,
        settlement: Some("fulfilled".into()),
        value: Some(json!({"accepted_revision": 17})),
        request: Some(json!({"agent":"Agent", "revision":17})),
    };
    for key in [
        json!(["sink.status", "flow", "Other", 17]),
        json!(["sink.status", "flow", "Agent", 99]),
    ] {
        let mut case = Case::new("status receipt identity");
        check_receipts(
            &mut case,
            &flow,
            std::slice::from_ref(&call),
            &[Receipt {
                key,
                reply: json!({"accepted_revision": 17}),
            }],
        );
        assert!(
            !case.passed(),
            "a different agent or requested revision was accepted"
        );
    }
    let mut case = Case::new("matching receipt");
    check_receipts(
        &mut case,
        &flow,
        &[call],
        &[Receipt {
            key: json!(["sink.status", "flow", "Agent", 17]),
            reply: json!({"accepted_revision":17}),
        }],
    );
    assert!(case.passed());
}

#[test]
fn blocked_package_is_never_reported_as_a_pass_or_a_skip() {
    let mut case = Case::new("blocked package");
    case.blocked = Some("missing provider protocol".into());
    case.record("refusal", json!({"code":"elicitation_invalid_request"}));
    let report = case.to_json();
    assert!(report["passed"].is_null());
    assert_eq!(report["blocked"], "missing provider protocol");
    assert!(report.get("not_run").is_none());
    assert_eq!(report["checks"], json!([]));
    assert_eq!(
        report["evidence"]["refusal"]["code"],
        "elicitation_invalid_request"
    );
}

#[test]
fn seeded_fixture_requires_a_terminal_disabled_module_and_no_pid() {
    let stopped = json!({"module":{"state":"disabled","enabled":false,"live":false}});
    assert!(seeded::fully_stopped(&stopped, None));
    assert!(!seeded::fully_stopped(&stopped, Some("123".into())));
    for (field, value) in [
        ("state", json!("draining")),
        ("enabled", json!(true)),
        ("live", json!(true)),
    ] {
        let mut changed = stopped.clone();
        changed["module"][field] = value;
        assert!(!seeded::fully_stopped(&changed, None));
        changed["module"].as_object_mut().unwrap().remove(field);
        assert!(!seeded::fully_stopped(&changed, None));
    }
}

#[test]
fn contract_validates_the_root_passed_by_the_rig_script() {
    let root = std::env::temp_dir().join("basal-rig-custom-root");
    assert!(validate_rig_root(&root, &root.join("home")).is_ok());
    assert!(validate_rig_root(&root, &root.join("not-home")).is_err());
    assert!(
        validate_rig_root(
            std::path::Path::new("relative-rig"),
            std::path::Path::new("relative-rig/home")
        )
        .is_err()
    );
}

#[test]
fn seeded_fixture_rejects_foreign_paths_and_symlinked_stores() {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let root = loop {
        let root = std::env::temp_dir().join(format!(
            "basal-rig-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        match std::fs::create_dir(&root) {
            Ok(()) => break root.canonicalize().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("create isolated fixture: {error}"),
        }
    };
    let rig = root.join("custom-rig-root");
    let home = rig.join("home");
    let store = rig.join("data/cortexkit/basal/store.db");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    std::fs::write(&store, []).unwrap();
    assert_eq!(
        seeded::fixture_path_for(&home, &store, "basal").unwrap(),
        store
    );
    assert!(seeded::fixture_path_for(&root.join("home"), &store, "basal").is_err());
    let foreign = root.join("production.db");
    std::fs::write(&foreign, []).unwrap();
    assert!(seeded::fixture_path_for(&home, &foreign, "basal").is_err());
    #[cfg(unix)]
    {
        std::fs::remove_file(&store).unwrap();
        std::os::unix::fs::symlink(&foreign, &store).unwrap();
        assert!(seeded::fixture_path_for(&home, &store, "basal").is_err());
    }
    assert!(seeded::fixture_path_for(&home, &store, "other").is_err());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn authorship_fingerprint_allows_only_the_one_author_cell() {
    let c = rusqlite::Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE TABLE flow_install(flow_id TEXT,version INTEGER,author_agent_id TEXT,code_hash TEXT);
        CREATE TABLE flow_install_grant(agent_id TEXT);
        INSERT INTO flow_install VALUES ('flow',1,NULL,'hash'),('other',1,NULL,'hash');
        INSERT INTO flow_install_grant VALUES ('foreign');").unwrap();
    let before = ownership::other_values_fingerprint(&c, "flow").unwrap();
    c.execute_batch("UPDATE flow_install SET author_agent_id='owner' WHERE flow_id='flow';")
        .unwrap();
    assert_eq!(
        ownership::other_values_fingerprint(&c, "flow").unwrap(),
        before
    );
    c.execute_batch("UPDATE flow_install SET code_hash='wrong' WHERE flow_id='flow';")
        .unwrap();
    assert_ne!(
        ownership::other_values_fingerprint(&c, "flow").unwrap(),
        before
    );
    c.execute_batch(
        "UPDATE flow_install SET code_hash='hash'; UPDATE flow_install_grant SET agent_id='other';",
    )
    .unwrap();
    assert_ne!(
        ownership::other_values_fingerprint(&c, "flow").unwrap(),
        before
    );
    assert!(ownership::other_values_fingerprint(&c, "absent").is_err());
}

#[test]
fn fixture_module_control_uses_only_the_absolute_isolated_cli() {
    let root = std::env::temp_dir().join("rig").join("custom-rig-root");
    let home = root.join("home");
    assert_eq!(
        seeded::module_cli_path(&home).unwrap(),
        root.join("bin").join(if cfg!(windows) {
            "ckdev-ck.exe"
        } else {
            "ckdev-ck"
        })
    );
    assert!(seeded::module_cli_path(&std::env::temp_dir().join("rig").join("not-home")).is_err());
    assert!(seeded::module_cli_path(std::path::Path::new("relative-rig/home")).is_err());
}

#[cfg(windows)]
#[test]
fn store_handle_probe_refuses_an_open_database_or_sidecar() {
    let directory =
        std::env::temp_dir().join(format!("basal-rig-handle-probe-{}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let store = directory.join("store.db");
    std::fs::write(&store, []).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", store.display()));
        std::fs::write(&path, []).unwrap();
        let held = std::fs::File::open(&path).unwrap();
        assert!(
            seeded::no_store_handles(&store).is_err(),
            "open {suffix:?} was not refused"
        );
        drop(held);
        assert!(seeded::no_store_handles(&store).is_ok());
    }
    std::fs::remove_dir_all(directory).unwrap();
}
