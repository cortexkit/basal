use super::*;

const CAPABILITY: &str = "flow-notice/v1";

fn path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/flow-notice-v1")
        .join(name)
}

fn samples() -> Vec<Value> {
    serde_json::from_slice::<Value>(&std::fs::read(path("flow-notice.json")).unwrap()).unwrap()
        ["samples"].as_array().unwrap().clone()
}

fn sample(name: &str) -> Value {
    samples().into_iter().find(|s| s["name"] == name).unwrap()
}

fn manifest() -> Value {
    json!({"id":"host-flow","version":1,"explanation":"Exercises the declared trigger and grants for a test, without additional effects.","purpose":"Report build health",
        "audience":{"kind":"agent","id":"ag_synapse"},
        "trigger":{"schedule":{"interval":"1h"}},
        "sinks":[{"kind":"notice","severity_max":"attention"}]})
}

fn fixture(tag: &str) -> (Arc<Fake>, common::Fixture) {
    let fake = Fake::new();
    fake.catalog.lock().unwrap()["agents"] = json!([
        {"agent_id":"ag_synapse","name":"SYNAPSE"},
        {"agent_id":"ag_other","name":"OTHER"}
    ]);
    set_core_capability(&fake, CAPABILITY, true);
    fake.scopes.lock().unwrap().insert(
        "scope-of-ag_synapse".into(),
        wire::CoreScope {
            agent: Some("ag_synapse".into()),
            live: true,
        },
    );
    let f = worker_fixture(fake.clone(), Arc::new(CoreConsent::new(fake.clone())), tag);
    (fake, f)
}

fn install(f: &common::Fixture, script: &str, m: &Value) {
    common::install(f, &common::agent("ag_synapse"), script, m);
}

fn run_once(f: &common::Fixture, tag: &str) -> (String, Value) {
    let run = common::admit(f, "host-flow", tag);
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, RunState::Succeeded, "{:?}", done.error_detail);
    (
        run,
        serde_json::from_str(done.result.as_deref().unwrap()).unwrap(),
    )
}

#[test]
fn notice_vectors_are_byte_pinned() {
    use sha2::{Digest, Sha256};
    let sums = std::fs::read_to_string(path("SHA256SUMS")).unwrap();
    let mut names = Vec::new();
    for line in sums.lines() {
        let (hash, name) = line.split_once("  ").unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(std::fs::read(path(name)).unwrap())),
            hash,
            "{name}"
        );
        names.push(name);
    }
    assert_eq!(
        names,
        ["README.md", "flow-notice.json", "notice-watch.json"]
    );
    assert_eq!(CAPABILITY, basal_host::core_host::FLOW_NOTICE_V1);
}

#[test]
fn notice_script_and_receipt_build_the_sample_wire_bytes() {
    for name in ["send", "replay", "resolve", "resolve-replay"] {
        let s = sample(name);
        let op = s["method"].as_str().unwrap();
        let mut args = s["params"].clone();
        args.as_object_mut().unwrap().remove("clientId");
        args.as_object_mut().unwrap().remove("stamp");
        let (fake, f) = fixture("notice-wire");
        install(&f, &format!("return await {op}({args});"), &manifest());
        approve(&fake, &f.module);
        fake.enqueue(op, vec![Ok(s["result"].clone())]);
        let (run, result) = run_once(&f, "notice-wire");
        assert_eq!(result, s["result"]);
        let rows = f.module.rt.calls(&run).unwrap();
        let row = &rows[0];
        assert_eq!(row.class, basal_core::StoredClass::KeyedMutation);
        let key = &row.idempotency_key;
        assert_eq!(key.len(), 44);
        assert!(key.starts_with("bk1-"));
        assert!(key.bytes().all(|b| (0x20..=0x7e).contains(&b)));
        let mut expected = s["params"].clone();
        expected["stamp"]["flow_id"] = json!("host-flow");
        expected["stamp"]["code_hash"] =
            fake.calls("flow.install_request")[0]["params"]["flow_install"]["code_hash"].clone();
        expected["clientId"] = json!(key);
        assert_eq!(
            fake.calls(op)[0]["params"].to_string().as_bytes(),
            expected.to_string().as_bytes()
        );
        let mut frozen: Value =
            serde_json::from_str(row.request.as_ref().unwrap().as_str()).unwrap();
        expected.as_object_mut().unwrap().remove("clientId");
        assert_eq!(frozen, expected);

        // Substitute the published install identity and receipt, not the
        // script's fields, to compare the complete carrier payload verbatim.
        frozen["stamp"] = s["params"]["stamp"].clone();
        let p = if op.ends_with("send") {
            Primitive::SinkNoticeSend
        } else {
            Primitive::SinkNoticeResolve
        };
        let mut request = call(p, frozen);
        request.idempotency_key = s["params"]["clientId"].as_str().unwrap().into();
        fake.enqueue(op, vec![Ok(s["result"].clone())]);
        let host = CoreHost::new(fake.clone());
        host.dispatch(&request).unwrap();
        assert_eq!(
            fake.calls(op).last().unwrap()["params"]
                .to_string()
                .as_bytes(),
            s["params"].to_string().as_bytes(),
            "{name}"
        );
        assert_eq!(
            host.classify(&request.kind),
            basal_host::CallClass::Mutation {
                honours_idempotency_keys: true
            }
        );
        assert_eq!(
            basal_host::op_label(&request.kind),
            format!("prefrontal-core.{op}")
        );
    }
}

#[test]
fn notice_omitted_optional_fields_stay_omitted_and_call_keys_are_distinct() {
    let (fake, f) = fixture("notice-optional");
    install(
        &f,
        "await sink.notice.send({title:'Build failed',severity:'info',key:'build'}); return await sink.notice.resolve({key:'build'});",
        &manifest(),
    );
    approve(&fake, &f.module);
    fake.enqueue(
        "sink.notice.send",
        vec![Ok(sample("send")["result"].clone())],
    );
    fake.enqueue(
        "sink.notice.resolve",
        vec![Ok(sample("resolve")["result"].clone())],
    );
    let (run, _) = run_once(&f, "notice-optional");
    let send = &fake.calls("sink.notice.send")[0]["params"];
    let resolve = &fake.calls("sink.notice.resolve")[0]["params"];
    assert!(send.get("body").is_none());
    assert!(resolve.get("resolution").is_none());
    assert_ne!(send["clientId"], resolve["clientId"]);
    let rows = f.module.rt.calls(&run).unwrap();
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].idempotency_key, rows[1].idempotency_key);
}

#[test]
fn notice_core_refusals_preserve_every_sample_code_at_the_script() {
    let mut codes: Vec<String> = samples()
        .iter()
        .filter_map(|s| s["error"]["code"].as_str().map(str::to_owned))
        .collect();
    codes.extend(
        [
            "sink_principal_refused",
            "sink_flow_not_installed",
            "notice_requires_agent_flow",
            "notice_grant_invalid",
            "notice.invalid_target",
            "notice.future_code",
        ]
        .map(str::to_owned),
    );
    for op in ["sink.notice.send", "sink.notice.resolve"] {
        for code in &codes {
            let (fake, f) = fixture("notice-refusal");
            let args = if op.ends_with("send") {
                "{title:'Build failed',severity:'info',key:'build'}"
            } else {
                "{key:'build'}"
            };
            install(
                &f,
                &format!("return await {op}({args}).catch(e => [e.code, e.data.code]);"),
                &manifest(),
            );
            approve(&fake, &f.module);
            fake.enqueue(
                op,
                vec![Err(WireError::Refused {
                    code: code.clone(),
                    message: "core's wording is not the contract".into(),
                })],
            );
            let (run, result) = run_once(&f, "notice-refusal");
            assert_eq!(result, json!([code, code]));
            let rows = f.module.rt.calls(&run).unwrap();
            assert_eq!(rows[0].dispatch, basal_core::DispatchState::Sent, "{code}");
            assert_eq!(rows[0].attempts, 1, "{code}");
            assert_eq!(fake.calls(op).len(), 1);
        }
    }
}

#[test]
fn notice_store_refusal_alone_defers_and_reuses_the_frozen_client_id() {
    for op in ["sink.notice.send", "sink.notice.resolve"] {
        for detailed in [false, true] {
            let (fake, f) = fixture("notice-retry");
            let args = if op.ends_with("send") {
                "{title:'Build failed',severity:'info',key:'build'}"
            } else {
                "{key:'build'}"
            };
            install(&f, &format!("return await {op}({args});"), &manifest());
            approve(&fake, &f.module);
            let error = if detailed {
                WireError::RefusedDetails {
                    code: "sink_store_failed".into(),
                    message: "temporary".into(),
                    detail: json!({"retryable":true}),
                }
            } else {
                WireError::Refused {
                    code: "sink_store_failed".into(),
                    message: "temporary".into(),
                }
            };
            let reply = sample(if op.ends_with("send") {
                "replay"
            } else {
                "resolve-replay"
            })["result"]
                .clone();
            fake.enqueue(op, vec![Err(error), Ok(reply.clone())]);
            let run = common::admit(&f, "host-flow", "notice-retry");
            f.module.engine.run_until_idle(50).unwrap();
            assert_eq!(
                f.module.rt.calls(&run).unwrap()[0].dispatch,
                basal_core::DispatchState::Deferred
            );
            let retry = f
                .module
                .rt
                .fixture_deferral(&run, 0)
                .unwrap()
                .retry_not_before;
            f.clock.set(retry);
            f.module.engine.run_until_idle(50).unwrap();
            let done = f.module.rt.run(&run).unwrap();
            assert_eq!(done.state, RunState::Succeeded);
            assert_eq!(done.result.as_deref(), Some(reply.to_string().as_str()));
            let sent = fake.calls(op);
            assert_eq!(sent.len(), 2);
            assert_eq!(sent[0].to_string(), sent[1].to_string());
        }
    }
}

#[test]
fn notice_script_validation_refuses_unsent() {
    let cases = [
        (
            "send",
            json!({"title":"Build failed","severity":"info","key":""}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"title":"Build failed","severity":"info","key":"x".repeat(129)}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"title":"Build failed","severity":"info","key":"with space"}),
            "sink_item_invalid",
        ),
        ("resolve", json!({"key":"é"}), "sink_item_invalid"),
        ("resolve", json!({"key":42}), "sink_item_invalid"),
        ("resolve", json!({}), "sink_item_invalid"),
        (
            "send",
            json!({"title":"Build failed","severity":"attention","key":"build"}),
            "notice_severity_above_grant",
        ),
        (
            "send",
            json!({"title":"Build failed","severity":"urgent","key":"build"}),
            "notice_urgent_refused",
        ),
        (
            "send",
            json!({"title":"Build failed","severity":"unknown","key":"build"}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"title":"Build failed","key":"build"}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"title":"","severity":"info","key":"build"}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"title":4,"severity":"info","key":"build"}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"severity":"info","key":"build"}),
            "sink_item_invalid",
        ),
        (
            "send",
            json!({"title":"Build failed","severity":"info","key":"build","clientId":"mine"}),
            "sink_item_invalid",
        ),
        (
            "resolve",
            json!({"key":"build","stamp":{}}),
            "sink_item_invalid",
        ),
        (
            "resolve",
            json!({"key":"build","title":"mine"}),
            "sink_item_invalid",
        ),
        ("send", json!(null), "sink_item_invalid"),
    ];
    for (op, args, code) in cases {
        let (fake, f) = fixture("notice-unsent");
        let mut m = manifest();
        m["sinks"][0]["severity_max"] = json!("info");
        install(
            &f,
            &format!("return await sink.notice.{op}({args}).catch(e => e.code);"),
            &m,
        );
        approve(&fake, &f.module);
        let (run, result) = run_once(&f, "notice-unsent");
        assert_eq!(result, code, "{op} {args}");
        assert!(fake.calls(&format!("sink.notice.{op}")).is_empty());
        let rows = f.module.rt.calls(&run).unwrap();
        assert_eq!(rows[0].attempts, 0);
        assert_eq!(rows[0].dispatch, basal_core::DispatchState::None);
    }
}

#[test]
fn notice_parent_leaves_body_resolution_and_other_limits_to_core() {
    for (op, args) in [
        (
            "sink.notice.send",
            json!({"title":" ","severity":"info","key":"A0._:-","body":null}),
        ),
        (
            "sink.notice.resolve",
            json!({"key":"x".repeat(128),"resolution":{"not":"text"}}),
        ),
    ] {
        let (fake, f) = fixture("notice-core-validation");
        install(&f, &format!("return await {op}({args});"), &manifest());
        approve(&fake, &f.module);
        fake.enqueue(op, vec![Ok(json!({"core":"judges these fields"}))]);
        let (_, result) = run_once(&f, "notice-core-validation");
        assert_eq!(result, json!({"core":"judges these fields"}));
        assert_eq!(fake.calls(op).len(), 1);
    }
}

#[test]
fn notice_intake_refuses_bad_caps_and_non_author_audiences_before_approval() {
    for (patch, code) in [
        (
            json!({"sinks":[{"kind":"notice","severity_max":"urgent"}]}),
            "notice_urgent_refused",
        ),
        (
            json!({"sinks":[{"kind":"notice","severity_max":"unknown"}]}),
            "notice_grant_invalid",
        ),
        (
            json!({"sinks":[{"kind":"notice","severity_max":4}]}),
            "notice_grant_invalid",
        ),
        (
            json!({"sinks":[{"kind":"notice","severity_max":"info","extra":true}]}),
            "notice_grant_invalid",
        ),
        (json!({"sinks":[{"kind":"notice"}]}), "notice_grant_invalid"),
        (json!({"audience":null}), "notice_requires_agent_flow"),
        (
            json!({"audience":{"kind":"global"}}),
            "notice_requires_agent_flow",
        ),
        (
            json!({"audience":{"kind":"workspace","id":"ws-1"}}),
            "notice_requires_agent_flow",
        ),
        (
            json!({"audience":{"kind":"agent","id":"ag_other"}}),
            "notice_requires_agent_flow",
        ),
    ] {
        let (fake, f) = fixture("notice-intake");
        let mut m = manifest();
        for (key, value) in patch.as_object().unwrap() {
            m[key] = value.clone();
        }
        for op in ["flow.install", "flow.dry_run", "package.register"] {
            let caller = if op == "package.register" {
                Caller::Operator
            } else {
                common::agent("ag_synapse")
            };
            let actual = if op == "flow.dry_run" {
                let error = f
                    .module
                    .dry
                    .run(&basal_module::dryrun::DryRunRequest {
                        script: "return 1;".into(),
                        manifest: m.to_string(),
                        author: "ag_synapse".into(),
                        loop_override: false,
                        mode: basal_module::dryrun::Mode::Capture,
                        trigger: basal_module::dryrun::DryTrigger::Synthetic(json!({})),
                        now_ms: common::T0,
                    })
                    .unwrap_err();
                let basal_module::dryrun::DryRunError::InstallRefused(
                    basal_core::InstallError::Manifest(
                        basal_core::manifest::ManifestError::Notice { code, .. },
                    ),
                ) = error
                else {
                    panic!("expected typed notice intake refusal, got {error:?}");
                };
                code.to_owned()
            } else {
                f.module
                    .handle(
                        &caller,
                        op,
                        json!({"script":"return 1;", "manifest":m.to_string()}),
                    )
                    .unwrap_err()
                    .code
            };
            // Even a valid notice cap needs per-instance approval, which
            // packages cannot obtain. Invalid caps retain their own code.
            let expected = if op == "package.register" && code == "notice_requires_agent_flow" {
                "notice_grant_invalid"
            } else {
                code
            };
            assert_eq!(actual, expected, "{op}: {patch}");
        }
        assert!(fake.calls("flow.install_request").is_empty());
    }
    let (fake, f) = fixture("notice-operator");
    let error = f
        .module
        .handle(
            &Caller::Operator,
            "flow.install",
            json!({"script":"return 1;","manifest":manifest().to_string()}),
        )
        .unwrap_err();
    assert_eq!(error.code, "notice_requires_agent_flow");
    assert!(fake.calls("flow.install_request").is_empty());
}

#[test]
fn notice_stored_versions_keep_decoding_without_current_intake_policy() {
    use basal_core::manifest::Manifest;
    let mut m = manifest();
    let parsed = Manifest::parse(&m.to_string()).unwrap();
    assert_eq!(serde_json::to_value(&parsed).unwrap()["sinks"], m["sinks"]);
    assert_eq!(parsed.digest_sinks().count(), 0);
    assert_eq!(parsed.room_sinks().count(), 0);
    assert!(!parsed.wait_sink());
    for cap in ["urgent", "future-severity"] {
        m["sinks"][0]["severity_max"] = json!(cap);
        m["audience"] = json!({"kind":"global"});
        assert!(Manifest::parse(&m.to_string()).is_err());
        assert!(Manifest::parse_stored(&m.to_string()).is_ok());
    }
}

#[test]
fn notice_packages_and_templates_refuse_registration_and_instance_creation() {
    for template in [false, true] {
        let (_, f) = fixture("notice-package");
        let mut m = manifest();
        let id = if template {
            "notice-template"
        } else {
            "notice-package"
        };
        m["id"] = json!(id);
        if template {
            m["params"] = json!({"note":{"type":"string","max_bytes":32}});
            m["template"] = json!({"max_per_agent":1,"max_live":2,"max_lifetime":"1d"});
        }
        let text = m.to_string();
        let error = f
            .module
            .handle(
                &Caller::Operator,
                "package.register",
                json!({"script":"return 1;","manifest":text}),
            )
            .unwrap_err();
        assert_eq!(error.code, "notice_grant_invalid");
        let count: i64 = f
            .module
            .rt
            .store()
            .read(|c| Ok(c.query_row("SELECT count(*) FROM package_versions", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
        // A stored package may predate the current notice-grant checks.
        // Instance creation must still refuse an unapproved notice grant.
        f.module.rt.store().write(|tx| {
            tx.execute("INSERT INTO package_versions(package,version,script,manifest,code_hash,registered_at) VALUES (?1,1,?2,?3,?4,0)", rusqlite::params![id, "return 1;", text, basal_core::ids::code_hash("return 1;", &text).as_slice()])?;
            Ok(())
        }).unwrap();
        let error = if template {
            f.module.handle(&Caller::Operator, "flow.instantiate", json!({"template":id,"version":1,"agent_id":"ag_synapse","params":{"note":"test"},"expires_in":"1h"})).unwrap_err()
        } else {
            f.module
                .handle(
                    &Caller::Core,
                    "flow.instance.ensure",
                    json!({"package":id,"version":1,"agent_id":"ag_synapse","generation":1}),
                )
                .unwrap_err()
        };
        assert_eq!(error.code, "notice_grant_invalid");
        let count: i64 = f
            .module
            .rt
            .store()
            .read(|c| Ok(c.query_row("SELECT count(*) FROM flows", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn notice_runtime_capability_and_missing_grant_are_refused_unsent() {
    for op in ["send", "resolve"] {
        for (grant, capability, code) in [
            (true, false, "notice_unsupported"),
            (false, true, "notice_not_granted"),
        ] {
            let (fake, f) = fixture("notice-gate");
            set_core_capability(&fake, CAPABILITY, capability);
            let mut m = manifest();
            if !grant {
                m["sinks"] = json!([]);
            }
            let args = if op == "send" {
                "{title:'Build failed',severity:'info',key:'build'}"
            } else {
                "{key:'build'}"
            };
            install(
                &f,
                &format!("return await sink.notice.{op}({args}).catch(e => e.code);"),
                &m,
            );
            approve(&fake, &f.module);
            let (run, result) = run_once(&f, "notice-gate");
            assert_eq!(result, code);
            assert!(fake.calls(&format!("sink.notice.{op}")).is_empty());
            let rows = f.module.rt.calls(&run).unwrap();
            assert_eq!(rows[0].attempts, 0);
            assert_eq!(rows[0].dispatch, basal_core::DispatchState::None);
        }
    }
}

#[test]
fn notice_dispatch_rechecks_capability_and_client_id_before_sending() {
    let fake = Fake::new();
    let host = CoreHost::new(fake.clone());
    for (p, op) in [
        (Primitive::SinkNoticeSend, "sink.notice.send"),
        (Primitive::SinkNoticeResolve, "sink.notice.resolve"),
    ] {
        set_core_capability(&fake, CAPABILITY, false);
        let Dispatched::Completed(outcome) = host.dispatch(&call(p, json!({}))).unwrap() else {
            panic!("completed")
        };
        assert_eq!(outcome.settlement, Settlement::Rejected);
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            "notice_unsupported"
        );
        set_core_capability(&fake, CAPABILITY, true);
        for key in [
            "".to_owned(),
            "x".repeat(129),
            "\n".to_owned(),
            "é".to_owned(),
            "\u{7f}".to_owned(),
        ] {
            let mut request = call(p, json!({}));
            request.idempotency_key = key;
            let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
                panic!("completed")
            };
            assert_eq!(outcome.settlement, Settlement::Rejected);
            assert_eq!(
                serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
                "sink_item_invalid"
            );
        }
        assert!(fake.calls(op).is_empty());
        for key in [" ".to_owned(), "~".repeat(128)] {
            let mut request = call(p, json!({}));
            request.idempotency_key = key.clone();
            fake.enqueue(op, vec![Ok(json!({"unchanged":true}))]);
            let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
                panic!("completed")
            };
            assert_eq!(outcome.settlement, Settlement::Fulfilled);
            assert_eq!(fake.calls(op).last().unwrap()["params"]["clientId"], key);
        }
    }
    for (flow, run, position) in [("", "", 0), ("é\n", "\u{7f}", u64::MAX)] {
        let key = basal_core::ids::idempotency_key(flow, run, position);
        assert_eq!(key.len(), 44);
        assert!(key.bytes().all(|b| (0x20..=0x7e).contains(&b)));
    }
}

#[test]
fn notice_dry_run_captures_both_calls_and_follows_the_live_catalog() {
    let (fake, f) = fixture("notice-dry");
    install(
        &f,
        "await sink.notice.send({title:'Build failed',severity:'attention',key:'build'}).catch(()=>null); await sink.notice.resolve({key:'build'}).catch(()=>null);",
        &manifest(),
    );
    let parsed = basal_core::manifest::Manifest::parse(&manifest().to_string()).unwrap();
    for supported in [false, true, false] {
        set_core_capability(&fake, CAPABILITY, supported);
        for mode in ["capture", "live"] {
            let result = f
                .module
                .handle(
                    &Caller::Operator,
                    "flow.dry_run",
                    json!({"flow_id":"host-flow","mode":mode,"trigger":{"kind":"test"}}),
                )
                .unwrap();
            let summary = &result["summary"];
            let calls = summary["runs"][0]["calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            for (call, kind) in calls
                .iter()
                .zip(["sink.notice.send", "sink.notice.resolve"])
            {
                assert_eq!(call["kind"], kind);
                assert_eq!(
                    call["action"],
                    if supported { "captured" } else { "refused" }
                );
                assert_eq!(
                    call["outcome"]["rejected"],
                    if supported {
                        "captured"
                    } else {
                        "notice_unsupported"
                    }
                );
            }
            let text = basal_core::dry_run_text::render(&parsed, summary);
            assert_eq!(text.problems.is_empty(), supported, "{text:?}");
            if supported {
                assert_eq!(
                    text.would,
                    [
                        "Send the user a notice at 'attention': 'Build failed'",
                        "Resolve its notice 'build'"
                    ]
                );
            }
        }
    }
    assert!(fake.calls("sink.notice.send").is_empty());
    assert!(fake.calls("sink.notice.resolve").is_empty());
}
