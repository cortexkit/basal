use super::*;

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
fn seeded_fixture_rejects_foreign_paths_and_symlinked_stores() {
    let output = std::process::Command::new("mktemp")
        .arg("-d")
        .arg(std::env::temp_dir().join("basal-rig-fixture.XXXXXXXX"))
        .output()
        .unwrap();
    assert!(output.status.success());
    let root = PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
        .canonicalize()
        .unwrap();
    let rig = root.join("ckdev-flows");
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
    std::fs::remove_file(&store).unwrap();
    std::os::unix::fs::symlink(&foreign, &store).unwrap();
    assert!(seeded::fixture_path_for(&home, &store, "basal").is_err());
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
    assert_eq!(
        seeded::module_cli_path(std::path::Path::new("/rig/ckdev-flows/home")).unwrap(),
        PathBuf::from("/rig/ckdev-flows/bin/ckdev-ck")
    );
    assert!(seeded::module_cli_path(std::path::Path::new("/rig/home")).is_err());
    assert!(seeded::module_cli_path(std::path::Path::new("ckdev-flows/home")).is_err());
}
