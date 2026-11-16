use super::*;

fn findings(catalog: &MockCatalog, module: &str) {
    catalog.set_event(
        module,
        "findings_changed",
        1,
        basal_host::EventDecl {
            headers: vec![],
            origin: basal_host::EventOrigin::Internal,
            body: basal_host::EventBody::Resolved {
                resolve_op: "events_get".into(),
            },
        },
    );
    catalog.set_op(
        module,
        "events_get",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
}

fn template(f: &Fixture) -> InstallRequest {
    let mut request = f.request("diagnosis");
    let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
    manifest["trigger"] = json!({
        "events":[{"module":"$params.source","name":"findings_changed","version":1}],
        "schedule":{"interval":"6h"}
    });
    manifest["ops"] = json!([{"module":"$params.source","op":"events_get"}]);
    manifest["params"] = json!({"source":{"type":"enum","values":["synapse","other"]}});
    manifest["template"] = json!({"max_per_agent":10,"max_lifetime":"7d"});
    request.manifest = manifest.to_string();
    request
}

fn placeholder_error(error: crate::packages::PackageError, field: &str, value: &str) {
    let crate::packages::PackageError::Install(crate::InstallError::Manifest(
        crate::manifest::ManifestError::TemplatePlaceholderInvalid { field: actual },
    )) = error
    else {
        panic!("expected placeholder refusal, got {error:?}")
    };
    assert!(actual.contains(field), "{actual}");
    assert!(actual.contains(value), "{actual}");
}

#[test]
fn enum_module_registration_checks_every_event_and_grant_choice() {
    for invalid in [
        "missing_event",
        "wrong_version",
        "not_query",
        "not_served",
        "no_scope",
        "unknown_grant",
    ] {
        let f = Fixture::new(60);
        findings(&f.catalog, "synapse");
        if invalid != "missing_event" {
            findings(&f.catalog, "other");
        }
        let mut request = template(&f);
        if invalid == "wrong_version" {
            let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
            manifest["trigger"]["events"][0]["version"] = json!(2);
            f.catalog.set_event(
                "synapse",
                "findings_changed",
                2,
                f.catalog.event("synapse", "findings_changed", 1).unwrap(),
            );
            request.manifest = manifest.to_string();
        }
        if invalid == "not_query" {
            f.catalog.set_op(
                "other",
                "events_get",
                OpDecl {
                    kind: Some(OpKind::Mutate),
                    cause_echo: false,
                    shell_capable: false,
                },
            );
        }
        if invalid == "not_served" {
            f.catalog
                .set_tool_contract("other", "events_get", json!({"modes":["classic"]}));
        }
        if invalid == "no_scope" {
            f.catalog.set_flow_capable("other", false);
        }
        if invalid == "unknown_grant" {
            f.catalog.set_op(
                "synapse",
                "extra",
                OpDecl {
                    kind: Some(OpKind::Query),
                    cause_echo: false,
                    shell_capable: false,
                },
            );
            let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
            manifest["ops"]
                .as_array_mut()
                .unwrap()
                .push(json!({"module":"$params.source","op":"extra"}));
            request.manifest = manifest.to_string();
        }
        let field = match invalid {
            "not_served" | "no_scope" => "ops[0].module",
            "unknown_grant" => "ops[1].module",
            _ => "trigger.events[0].module",
        };
        placeholder_error(
            f.rt.register_package(&request.script, &request.manifest)
                .unwrap_err(),
            field,
            "other",
        );
    }
}

#[test]
fn enum_module_registration_requires_correlated_resolver_grant() {
    let f = Fixture::new(60);
    for module in ["synapse", "other"] {
        findings(&f.catalog, module);
    }
    let request = template(&f);
    let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
    manifest["params"]["reader"] = manifest["params"]["source"].clone();
    manifest["ops"][0]["module"] = json!("$params.reader");
    placeholder_error(
        f.rt.register_package(&request.script, &manifest.to_string())
            .unwrap_err(),
        "trigger.events[0].module",
        "synapse",
    );

    manifest["ops"] = json!([{"module":"synapse","op":"events_get"}]);
    placeholder_error(
        f.rt.register_package(&request.script, &manifest.to_string())
            .unwrap_err(),
        "trigger.events[0].module",
        "other",
    );
    manifest["ops"]
        .as_array_mut()
        .unwrap()
        .push(json!({"module":"other","op":"events_get"}));
    f.rt.register_package(&request.script, &manifest.to_string())
        .unwrap();
}

#[test]
fn enum_module_instance_binds_event_and_authorizes_only_chosen_module() {
    let f = Fixture::new(60);
    let owner = "agent_47120287c700722b";
    f.catalog.add_named_agent(owner, "Owner");
    for module in ["synapse", "other"] {
        findings(&f.catalog, module);
    }
    let request = template(&f);
    f.rt.register_package(&request.script, &request.manifest)
        .unwrap();
    let instance =
        f.rt.instantiate("diagnosis", 1, owner, &json!({"source":"synapse"}), "1d")
            .unwrap();
    let id = instance["flow_id"].as_str().unwrap();
    let manifest =
        f.rt.store()
            .read(|c| effective_manifest(c, &request.manifest, id))
            .unwrap();
    assert_eq!(
        manifest.trigger.events.as_ref().unwrap()[0].module,
        "synapse"
    );
    assert_eq!(manifest.ops[0].module, "synapse");
    for module in ["synapse", "other"] {
        let result = crate::authorize::check(
            &manifest,
            &Default::default(),
            &f.catalog,
            &CallKind::Op {
                module: module.into(),
                op: "events_get".into(),
            },
            &json!({}),
        );
        assert_eq!(result.is_ok(), module == "synapse", "{module}: {result:?}");
    }
    let mut event = notice(10);
    event.subject = "ck.account.event.other.findings_changed.v1".into();
    assert_eq!(f.rt.admit_events(&[event.clone()]).unwrap().matched, 0);
    event.subject = "ck.account.event.synapse.findings_changed.v1".into();
    assert_eq!(f.rt.admit_events(&[event]).unwrap().admitted, 1);
}

struct UnusedWorker;
impl crate::WorkerChannel for UnusedWorker {
    fn welcome(&self) -> &basal_proto::Welcome {
        panic!("queued run reached worker")
    }
    fn send(
        &mut self,
        _: &basal_proto::ParentMessage,
    ) -> std::result::Result<(), crate::ChannelError> {
        panic!("queued run reached worker")
    }
    fn recv(
        &mut self,
        _: std::time::Duration,
    ) -> std::result::Result<basal_proto::WorkerMessage, crate::ChannelError> {
        panic!("queued run reached worker")
    }
    fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
        panic!("queued run reached worker")
    }
    fn kill(&mut self) {
        panic!("queued run reached worker")
    }
}

#[test]
fn combined_trigger_installs_and_serializes_event_and_schedule_runs() {
    for event_first in [true, false] {
        let f = Fixture::new(60);
        findings(&f.catalog, "synapse");
        let mut request = template(&f);
        let mut manifest: serde_json::Value = serde_json::from_str(&request.manifest).unwrap();
        manifest.as_object_mut().unwrap().remove("params");
        manifest.as_object_mut().unwrap().remove("template");
        manifest["trigger"]["events"][0]["module"] = json!("synapse");
        manifest["ops"][0]["module"] = json!("synapse");
        request.manifest = manifest.to_string();
        let installed = f.rt.install(&request).unwrap();
        f.rt.approve("diagnosis", 1, &installed.code_hash, "test-approved")
            .unwrap();
        let mut event = notice(11);
        event.subject = "ck.account.event.synapse.findings_changed.v1".into();
        if event_first {
            assert_eq!(f.rt.admit_events(&[event.clone()]).unwrap().admitted, 1);
        }
        f.rt.config.clock.set(1_000 + 6 * 60 * 60 * 1_000);
        let tick = f.rt.scheduler().unwrap().tick().unwrap();
        assert_eq!(
            tick.new_runs().len(),
            1,
            "events do not postpone the backstop"
        );
        if !event_first {
            assert_eq!(f.rt.admit_events(&[event]).unwrap().admitted, 1);
        }
        let runs: Vec<(String, String)> =
            f.rt.store()
                .read(|c| {
                    Ok(
                        c.prepare("SELECT run_id,trigger_kind FROM live_runs ORDER BY admit_seq")?
                            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                            .collect::<rusqlite::Result<_>>()?,
                    )
                })
                .unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].1, if event_first { "event" } else { "schedule" });
        assert_eq!(runs[1].1, if event_first { "schedule" } else { "event" });
        assert_eq!(
            f.rt.startable().unwrap(),
            vec![(runs[0].0.clone(), "diagnosis".into())]
        );
        assert_eq!(
            f.rt.activate(&runs[1].0, &mut UnusedWorker).unwrap(),
            crate::ActivationEnd::Waiting {
                holder: runs[0].0.clone()
            }
        );
        assert_eq!(f.rt.activation_count(&runs[1].0).unwrap(), 0);
        f.rt.cancel(&runs[0].0, "operator").unwrap();
        assert_eq!(
            f.rt.startable().unwrap(),
            vec![(runs[1].0.clone(), "diagnosis".into())]
        );
    }
}

#[test]
fn event_script_trigger_includes_event_kind() {
    let mut event = notice(12);
    event.subject = "ck.account.event.synapse.findings_changed.v1".into();
    let trigger = script_trigger(&event, BODY.as_bytes()).unwrap();
    let trigger: serde_json::Value = serde_json::from_str(trigger.as_str()).unwrap();
    assert_eq!(trigger["kind"], "event");
    assert_eq!(trigger["event"]["module"], "synapse");
    assert_eq!(trigger["event"]["body"], json!({"changed":true}));
}

struct ManagementCatalog(MockCatalog);
impl Catalog for ManagementCatalog {
    fn supports_flow_calls(&self, module: &str) -> bool {
        self.0.supports_flow_calls(module)
    }
    fn flow_operations(
        &self,
        module: &str,
    ) -> std::result::Result<Option<Vec<String>>, basal_host::transport::WireError> {
        Ok(Some(if module == "synapse" {
            vec!["events_get".into()]
        } else {
            vec![]
        }))
    }
    fn event(&self, module: &str, name: &str, version: u32) -> Option<basal_host::EventDecl> {
        self.0.event(module, name, version)
    }
    fn op(&self, module: &str, op: &str) -> Option<OpDecl> {
        self.0.op(module, op)
    }
    fn agent_known(
        &self,
        agent: &str,
    ) -> std::result::Result<bool, basal_host::transport::WireError> {
        self.0.agent_known(agent)
    }
    fn agent_id(
        &self,
        agent: &str,
    ) -> std::result::Result<Option<String>, basal_host::transport::WireError> {
        self.0.agent_id(agent)
    }
    fn resolve_agents(
        &self,
        agents: &[&str],
    ) -> std::result::Result<Vec<Option<String>>, basal_host::transport::WireError> {
        self.0.resolve_agents(agents)
    }
}

#[test]
fn enum_module_management_choices_require_flow_operations() {
    let f = Fixture::new(60);
    for module in ["synapse", "other"] {
        findings(&f.catalog, module);
    }
    let request = template(&f);
    let manifest = Manifest::parse(&request.manifest).unwrap();
    let error = crate::install::validate_inner(
        &manifest,
        "operator",
        &ManagementCatalog(f.catalog.clone()),
        &Default::default(),
        false,
        true,
    )
    .unwrap_err();
    placeholder_error(
        crate::packages::PackageError::Install(error),
        "ops[0].module",
        "other",
    );
}
