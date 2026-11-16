//! Fixed synthetic identities; facts come from the production write paths.
use super::*;
use basal_core::codemode::{admission::Limits, models, store};
use basal_proto::{ActivationResult, CallKind, HostCall, JsonText, Primitive, WorkerMessage};
use std::collections::BTreeMap;

pub fn setup(tag: &str) -> Fixture {
    let f = fixture(
        tag,
        Options {
            dry_run_max_fires: Some(0),
            ..Options::default()
        },
    );
    f.catalog
        .add_named_agent("agent_attribution", "Attribution");
    let mut m = events_manifest("attribution-flow");
    m["sinks"][0]["agent"] = json!("Attribution");
    m["llm"] = json!({"iq":50,"eq":0,"max_output":10,"token_cap":{"tokens":10000,"window":"1h"}});
    let installed = f
        .module
        .rt
        .install(&basal_core::InstallRequest {
            script: "return 1;".into(),
            manifest: m.to_string(),
            author: "agent_attribution".into(),
            loop_override: false,
        })
        .unwrap();
    f.module
        .rt
        .approve(
            &installed.flow_id,
            1,
            &installed.code_hash,
            "synthetic approval",
        )
        .unwrap();
    let mut template = events_manifest("attribution-template");
    template["params"] = json!({"note":{"type":"string","max_bytes":64}});
    template["template"] = json!({"max_per_agent":4,"max_lifetime":"1d"});
    template["sinks"][0]["agent"] = json!("$self");
    f.module
        .rt
        .register_package("return 1;", &template.to_string())
        .unwrap();
    f.module
        .rt
        .instantiate(
            "attribution-template",
            1,
            "agent_attribution",
            &json!({"note":"synthetic"}),
            "1h",
        )
        .unwrap();
    f.module
        .rt
        .fixture_run(
            "attribution-run",
            "attribution-flow",
            1,
            "return 1;",
            &m.to_string(),
            basal_core::run_records::State::new(T0),
        )
        .unwrap();
    let mut worker = basal_core::test_support::ScriptedWorker::new(vec![
        WorkerMessage::HostCall(HostCall {
            position: 0,
            kind: CallKind::Primitive(Primitive::Llm),
            args: JsonText::new(
                json!({"prompt":"synthetic private prompt","complete_after_ms":0}).to_string(),
            )
            .unwrap(),
        }),
        WorkerMessage::Finished {
            activation_id: 0,
            result: ActivationResult::Completed {
                value: JsonText::null(),
            },
        },
    ]);
    f.module
        .rt
        .activate("attribution-run", &mut worker)
        .unwrap();
    let limits = serde_json::to_string(&Limits::default()).unwrap();
    f.module
        .rt
        .store()
        .write(|tx| {
            store::insert_run(
                tx,
                &store::NewRun {
                    run_id: "attribution-program",
                    agent_id: "agent_attribution",
                    program: "",
                    catalog: "[]",
                    catalog_digest: &"0".repeat(64),
                    description: None,
                    limits: &limits,
                    scope: "{}",
                    deadline_ms: T0 + 10000,
                    admitted_at: T0,
                },
                None,
            )?;
            tx.execute(
                "UPDATE codemode_runs SET bind_identity=?1 WHERE run_id='attribution-program'",
                [serde_json::to_string(&subc_protocol::BindIdentity::new(
                    "broca",
                    "parent-session",
                ))
                .unwrap()],
            )?;
            store::insert_call(
                tx,
                "attribution-program",
                0,
                "model",
                2,
                store::CallStart::Intent { at: T0 },
            )?;
            Ok(())
        })
        .unwrap();
    let store::Lookup::Found(run) = f
        .module
        .rt
        .store()
        .read(|c| store::lookup(c, "attribution-program"))
        .unwrap()
    else {
        panic!()
    };
    models::prepare(
        f.module.rt.store(),
        &basal_host::selector::FakeSelector::default(),
        &run,
        &HostCall {
            position: 0,
            kind: CallKind::Tool {
                name: "model".into(),
            },
            args: JsonText::new(
                json!({"prompt":"synthetic private program prompt","max_output":1}).to_string(),
            )
            .unwrap(),
        },
    )
    .unwrap();
    f
}

pub fn reader() -> Caller {
    basal_module::caller::from_route(
        Some(&subc_protocol::Principal::Reserved {
            module_id: "astrocyte".into(),
        }),
        None,
    )
}

pub fn vectors(f: &Fixture) -> BTreeMap<String, Value> {
    let page = f
        .module
        .handle(&reader(), "attribution.read", json!({}))
        .unwrap();
    let facts = page["facts"].as_array().unwrap();
    assert_eq!(facts.len(), 4);
    assert!(!page.to_string().contains("synthetic private"));
    let mut files = BTreeMap::new();
    for (name, kind, workload) in [
        ("flow.json", "flow", "flow"),
        ("instance.json", "flow", "instance"),
        ("flow-session.json", "session", "flow"),
        ("program-session.json", "session", "codemode"),
    ] {
        let fact = facts
            .iter()
            .find(|fact| {
                fact["subject"]["kind"] == kind
                    && if workload == "instance" {
                        fact["task"] == "basal.attribution-template"
                    } else {
                        fact["workload"] == workload && fact["task"] != "basal.attribution-template"
                    }
            })
            .unwrap();
        files.insert(name.into(), fact.clone());
    }
    files.insert(
        "read-page.json".into(),
        f.module
            .handle(&reader(), "attribution.read", json!({"after":0,"limit":2}))
            .unwrap(),
    );
    files
}
