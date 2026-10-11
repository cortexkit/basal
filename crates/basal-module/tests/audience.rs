mod common;
mod wire;

use basal_core::{InstallRequest, Manifest};
use basal_module::caller::{self, Caller};
use common::{Options, T0, agent, events_manifest, fixture, install, install_approved};
use serde_json::{Value, json};
use subc_protocol::Principal;

fn audience_manifest(id: &str, audience: Value) -> Value {
    let mut m = events_manifest(id);
    m["audience"] = audience;
    m["sinks"][0]["agent"] = json!("$audience");
    m["status"] = json!(["$audience"]);
    m
}

fn reserved(module_id: &str) -> Caller {
    caller::from_route(
        Some(&Principal::Reserved {
            module_id: module_id.into(),
        }),
        None,
    )
}

#[test]
fn audience_op_requires_verified_unscoped_module_route() {
    let f = fixture("audience-caller", Options::default());
    install_approved(
        &f,
        &Caller::Operator,
        "return 1;",
        &audience_manifest("reach", json!({"kind":"global"})),
    );
    for caller in [
        reserved("prefrontal-core"),
        reserved("plexus"),
        reserved("callosum"),
        reserved("aft"),
    ] {
        assert_eq!(
            f.module
                .handle(&caller, "flow.audience", json!({"flow_id":"reach"}))
                .unwrap(),
            json!({"audience":{"kind":"global"}})
        );
    }
    for caller in [
        Caller::Local,
        agent("SYNAPSE"),
        Caller::Other("reserved:plexus".into()),
        caller::from_route(Some(&Principal::Unverified), None),
        caller::from_route(None, None),
    ] {
        for flow in ["reach", "unknown"] {
            assert_eq!(
                f.module
                    .handle(&caller, "flow.audience", json!({"flow_id":flow}))
                    .unwrap_err()
                    .code,
                "not_permitted"
            );
        }
    }
    let scope = subc_protocol::scope::ScopeStamp {
        owner: Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        scope_ref: "s".into(),
        scope_epoch: 1,
        kind: subc_protocol::scope::ScopeKind::Head,
        parent: None,
        parent_state: None,
        attributes: subc_protocol::scope::ScopeAttributes::new()
            .with_agent_id(Some("SYNAPSE".into())),
        owner_authorized: true,
    };
    let scoped = caller::from_route(
        Some(&Principal::Reserved {
            module_id: "plexus".into(),
        }),
        Some(&scope),
    );
    assert_eq!(
        f.module
            .handle(&scoped, "flow.audience", json!({"flow_id":"reach"}))
            .unwrap_err()
            .code,
        "not_permitted"
    );
}

#[test]
fn audience_op_unknown_and_unapproved_have_same_refusal() {
    let f = fixture("audience-hidden", Options::default());
    install(
        &f,
        &Caller::Operator,
        "return 1;",
        &audience_manifest("pending", json!({"kind":"global"})),
    );
    let unknown = f
        .module
        .handle(&Caller::Core, "flow.audience", json!({"flow_id":"unknown"}))
        .unwrap_err();
    let pending = f
        .module
        .handle(&Caller::Core, "flow.audience", json!({"flow_id":"pending"}))
        .unwrap_err();
    assert_eq!(unknown, pending);
    assert_eq!(unknown.code, "flow_not_approved");
}

#[test]
fn audience_op_reads_approved_version_not_pending() {
    let f = fixture("audience-version", Options::default());
    let m = audience_manifest("versions", json!({"kind":"agent","id":"SYNAPSE"}));
    install_approved(&f, &Caller::Operator, "return 1;", &m);
    let mut v2 = audience_manifest("versions", json!({"kind":"global"}));
    v2["version"] = json!(2);
    let pending = install(&f, &Caller::Operator, "return 2;", &v2);
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.audience",
                json!({"flow_id":"versions"})
            )
            .unwrap(),
        json!({"audience":{"kind":"agent","id":"SYNAPSE"}})
    );
    assert!(f.consent.decide(
        pending["card_id"].as_str().unwrap(),
        basal_host::CardDecision::Approve,
        "operator"
    ));
    assert_eq!(
        f.module
            .handle(
                &Caller::Core,
                "flow.audience",
                json!({"flow_id":"versions"})
            )
            .unwrap(),
        json!({"audience":{"kind":"global"}})
    );
}

#[test]
fn audience_reply_matches_documented_shape() {
    let docs = include_str!("../../../docs/ops.md");
    let section = docs
        .split("## flow.audience\n")
        .nth(1)
        .unwrap()
        .split("## flow.disable")
        .next()
        .unwrap();
    let shapes: Vec<Value> = section
        .split("```json\n")
        .skip(1)
        .map(|part| serde_json::from_str(part.split("```").next().unwrap()).unwrap())
        .collect();
    assert_eq!(
        shapes,
        [
            json!({"flow_id":"string"}),
            json!({"audience":"audience shape below|null"}),
            json!({"kind":"global"}),
            json!({"kind":"workspace","id":"string"}),
            json!({"kind":"agent","id":"string"})
        ]
    );
    let f = fixture("audience-shape", Options::default());
    f.catalog.add_workspace("team");
    let audiences = [
        None,
        Some(json!({"kind":"global"})),
        Some(json!({"kind":"workspace","id":"team"})),
        Some(json!({"kind":"agent","id":"SYNAPSE"})),
    ];
    for (n, audience) in audiences.into_iter().enumerate() {
        let id = format!("shape-{n}");
        let m = match &audience {
            None => events_manifest(&id),
            Some(a) => audience_manifest(&id, a.clone()),
        };
        install_approved(&f, &Caller::Operator, "return 1;", &m);
        assert_eq!(
            f.module
                .handle(&Caller::Core, "flow.audience", json!({"flow_id":id}))
                .unwrap(),
            json!({"audience":audience})
        );
    }
    for params in [
        json!({}),
        json!({"flow_id":42}),
        json!({"flow_id":"shape-0","extra":true}),
    ] {
        assert_eq!(
            f.module
                .handle(&Caller::Core, "flow.audience", params)
                .unwrap_err()
                .code,
            "invalid_params"
        );
    }
    let decl = basal_module::manifest::OPERATIONS
        .iter()
        .find(|op| op.0 == "flow.audience")
        .unwrap();
    assert_eq!(
        decl.1,
        subc_protocol::manifest::ManagementOperationKind::Query
    );
}

#[test]
fn install_and_dry_run_apply_audience_authorship_and_resolution() {
    let f = fixture("audience-validation", Options::default());
    f.catalog.add_named_agent("agent_owner", "SYNAPSE");
    f.catalog.add_workspace("team");
    for (n, audience, author, expected) in [
        (
            0,
            json!({"kind":"global"}),
            "agent_owner",
            "audience_requires_operator",
        ),
        (
            1,
            json!({"kind":"workspace","id":"team"}),
            "agent_owner",
            "audience_requires_operator",
        ),
        (
            2,
            json!({"kind":"agent","id":"ALF"}),
            "agent_owner",
            "foreign_agent_target",
        ),
        (
            3,
            json!({"kind":"agent","id":"missing"}),
            "operator",
            "install_refused",
        ),
        (
            4,
            json!({"kind":"workspace","id":"missing"}),
            "operator",
            "unknown_workspace",
        ),
    ] {
        let id = format!("invalid-{n}");
        let m = audience_manifest(&id, audience);
        let err = f
            .module
            .handle(
                &Caller::Operator,
                "flow.install",
                json!({"script":"return 1;","manifest":m.to_string(),"author":author}),
            )
            .unwrap_err();
        assert_eq!(err.code, expected);
        assert!(f.module.rt.flow(&id).unwrap().is_none());
        assert!(f.consent.cards().is_empty());
        let request = InstallRequest {
            script: "return 1;".into(),
            manifest: m.to_string(),
            author: author.into(),
            loop_override: false,
        };
        f.module
            .rt
            .store()
            .write(|tx| {
                basal_core::install::record(
                    tx,
                    &Manifest::parse(&request.manifest).unwrap(),
                    &request,
                    Vec::new(),
                    T0,
                )
                .unwrap();
                Ok(())
            })
            .unwrap();
        let err = f
            .module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({"flow_id":id,"trigger":{}}),
            )
            .unwrap_err();
        assert_eq!(err.code, expected);
    }
    for (n, caller, audience) in [
        (
            0,
            agent("agent_owner"),
            json!({"kind":"agent","id":"SYNAPSE"}),
        ),
        (
            1,
            agent("agent_owner"),
            json!({"kind":"agent","id":"agent_owner"}),
        ),
        (2, Caller::Operator, json!({"kind":"global"})),
        (3, Caller::Operator, json!({"kind":"workspace","id":"team"})),
        (4, Caller::Local, json!({"kind":"global"})),
        (5, Caller::Local, json!({"kind":"workspace","id":"team"})),
        (6, Caller::Local, json!({"kind":"agent","id":"ALF"})),
    ] {
        let id = format!("allowed-{n}");
        let reply = install(&f, &caller, "return 1;", &audience_manifest(&id, audience));
        assert_eq!(reply["state"], "pending");
        assert!(reply["dry_run"].get("error").is_none(), "{reply}");
        f.module
            .handle(
                &Caller::Operator,
                "flow.dry_run",
                json!({"flow_id":id,"trigger":{}}),
            )
            .unwrap();
    }
}

#[test]
fn unreadable_registries_refuse_install_and_dry_run() {
    use basal_host::subc_catalog::SubcCatalog;
    use basal_host::transport::WireError;
    use basal_module::module::Hosts;
    use std::sync::Arc;
    for (n, audience, op, expected) in [
        (
            0,
            json!({"kind":"agent","id":"SYNAPSE"}),
            "agent.list",
            "install_refused",
        ),
        (
            1,
            json!({"kind":"workspace","id":"team"}),
            "enumerate",
            "workspace_registry_unavailable",
        ),
    ] {
        for unreadable in [false, true] {
            let fake = wire::Fake::new();
            fake.catalog.lock().unwrap()["modules"].as_array_mut().unwrap().push(json!({"module_id":"entorhinal","roles":[{"role":"management_surface","operations":[{"name":"enumerate","kind":"query"}],"config_schema":{},"observability":[],"identity_scope":[],"concurrency":"serial"}]}));
            let failure = || {
                if unreadable {
                    Ok(json!({}))
                } else {
                    Err(WireError::NeverSent("registry offline".into()))
                }
            };
            fake.enqueue(op, vec![failure(), failure()]);
            let consent = basal_host::MockConsent::new();
            let f = fixture(
                "audience-registry-failure",
                Options {
                    hosts: Some(Hosts {
                        transport: fake.clone(),
                        catalog: Arc::new(SubcCatalog::new(fake.clone())),
                        host: Arc::new(basal_host::mock::MockHost::new()),
                        consent: Arc::new(consent.clone()),
                        hooks: Arc::new(basal_core::NoHooks),
                    }),
                    ..Options::default()
                },
            );
            let id = format!("registry-{n}");
            let mut m = audience_manifest(&id, audience.clone());
            m["trigger"] = json!({"schedule":{"interval":"1h"}});
            m["ops"] = json!([]);
            let err = f
                .module
                .handle(
                    &Caller::Operator,
                    "flow.install",
                    json!({"script":"return 1;","manifest":m.to_string()}),
                )
                .unwrap_err();
            assert_eq!(err.code, expected, "{err}");
            assert!(f.module.rt.flow(&id).unwrap().is_none());
            assert!(consent.cards().is_empty());
            let request = InstallRequest {
                script: "return 1;".into(),
                manifest: m.to_string(),
                author: "operator".into(),
                loop_override: false,
            };
            f.module
                .rt
                .store()
                .write(|tx| {
                    basal_core::install::record(
                        tx,
                        &Manifest::parse(&request.manifest).unwrap(),
                        &request,
                        Vec::new(),
                        T0,
                    )
                    .unwrap();
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                f.module
                    .handle(
                        &Caller::Operator,
                        "flow.dry_run",
                        json!({"flow_id":id,"trigger":{}})
                    )
                    .unwrap_err()
                    .code,
                expected
            );
            assert_eq!(fake.calls(op).len(), 2);
        }
    }
}

#[test]
fn audience_recipient_and_core_refusal_reach_script_unchanged() {
    use basal_host::core_host::CoreHost;
    use basal_host::transport::WireError;
    use basal_module::module::Hosts;
    use std::sync::Arc;
    let fake = wire::Fake::new();
    let catalog = basal_host::MockCatalog::standard();
    let consent = basal_host::MockConsent::new();
    let f = fixture(
        "audience-script",
        Options {
            hosts: Some(Hosts {
                transport: fake.clone(),
                catalog: Arc::new(catalog),
                host: Arc::new(CoreHost::new(fake.clone())),
                consent: Arc::new(consent.clone()),
                hooks: Arc::new(basal_core::NoHooks),
            }),
            ..Options::with_worker()
        },
    );
    for op in ["sink.digest", "sink.status"] {
        fake.enqueue(
            op,
            vec![Err(WireError::Refused {
                code: "sink_target_not_in_audience".into(),
                message: "recipient moved".into(),
            })],
        );
    }
    let script = "const result=[]; for (const write of [()=>sink.digest('outside',{title:'hello',body:'world'}),()=>sink.status('outside','ready')]) { try { await write(); } catch(e) { result.push(e.data); } } return result;";
    let reply = f.module.handle(&Caller::Operator, "flow.install", json!({"script":script,"manifest":audience_manifest("script-reach", json!({"kind":"global"})).to_string()})).unwrap();
    assert!(consent.decide(
        reply["card_id"].as_str().unwrap(),
        basal_host::CardDecision::Approve,
        "operator"
    ));
    let run = common::admit(&f, "script-reach", "one");
    f.module.engine.run_until_idle(50).unwrap();
    let done = f.module.rt.run(&run).unwrap();
    assert_eq!(done.state, basal_core::RunState::Succeeded, "{done:?}");
    let result: Value = serde_json::from_str(done.result.as_ref().unwrap()).unwrap();
    assert_eq!(
        result,
        json!([{"code":"sink_target_not_in_audience","message":"recipient moved"},{"code":"sink_target_not_in_audience","message":"recipient moved"}])
    );
    assert_eq!(fake.calls("sink.digest")[0]["params"]["agent"], "outside");
    assert_eq!(
        fake.calls("sink.digest")[0]["params"]["action"],
        "piggyback"
    );
    assert_eq!(fake.calls("sink.status")[0]["params"]["agent"], "outside");
}
