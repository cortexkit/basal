mod common;

use basal_host::CardDecision;
use basal_module::caller::{self, Caller};
use common::{Options, agent, events_manifest, fixture, install, install_approved};
use serde_json::{Value, json};
use subc_protocol::Principal;

const OP: &str = "flow.private_content_providers";

fn reserved(module_id: &str) -> Caller {
    caller::from_route(
        Some(&Principal::Reserved {
            module_id: module_id.into(),
        }),
        None,
    )
}

#[test]
fn providers_op_requires_verified_unscoped_broca() {
    let f = fixture("providers-caller", Options::default());
    install_approved(
        &f,
        &Caller::Operator,
        "return 1;",
        &events_manifest("private"),
    );
    assert_eq!(
        f.module
            .handle(&reserved("broca"), OP, json!({"flow_id":"private"}))
            .unwrap()["providers"],
        json!([])
    );
    let scope = subc_protocol::scope::ScopeStamp::new(
        Principal::Reserved {
            module_id: "prefrontal-core".into(),
        },
        "s",
        1,
        subc_protocol::scope::ScopeKind::Head,
    )
    .with_attributes(
        subc_protocol::scope::ScopeAttributes::new().with_agent_id(Some("SYNAPSE".into())),
    )
    .with_owner_authorized(true);
    let scoped_broca = caller::from_route(
        Some(&Principal::Reserved {
            module_id: "broca".into(),
        }),
        Some(&scope),
    );
    for caller in [
        Caller::Core,
        Caller::Operator,
        Caller::Local,
        agent("SYNAPSE"),
        reserved("aft"),
        reserved("Broca"),
        Caller::Other("reserved:broca".into()),
        caller::from_route(Some(&Principal::Unverified), None),
        caller::from_route(None, None),
        scoped_broca,
    ] {
        for flow_id in ["private", "unknown"] {
            assert_eq!(
                f.module
                    .handle(&caller, OP, json!({"flow_id":flow_id}))
                    .unwrap_err()
                    .code,
                "not_permitted",
                "{caller:?}"
            );
        }
    }
}

#[test]
fn providers_op_reads_current_approval_not_pending_version() {
    let f = fixture("providers-version", Options::default());
    let mut m = events_manifest("versions");
    m["private_content_providers"] = json!(["typesafe", "openai"]);
    let first = install(&f, &Caller::Operator, "return 1;", &m);
    assert!(f.consent.decide(
        first["card_id"].as_str().unwrap(),
        CardDecision::Approve,
        "operator"
    ));
    m["version"] = json!(2);
    m["private_content_providers"] = json!(["cloudflare-workers-ai"]);
    let second = install(&f, &Caller::Operator, "return 2;", &m);
    assert_eq!(
        f.module
            .handle(&reserved("broca"), OP, json!({"flow_id":"versions"}))
            .unwrap(),
        json!({"flow_id":"versions","version":1,"code_hash":first["code_hash"],"providers":["typesafe","openai"]})
    );
    assert!(f.consent.decide(
        second["card_id"].as_str().unwrap(),
        CardDecision::Approve,
        "operator"
    ));
    assert_eq!(
        f.module
            .handle(&reserved("broca"), OP, json!({"flow_id":"versions"}))
            .unwrap(),
        json!({"flow_id":"versions","version":2,"code_hash":second["code_hash"],"providers":["cloudflare-workers-ai"]})
    );
}

#[test]
fn providers_op_absent_and_empty_lists_return_base_set_only() {
    let f = fixture("providers-base", Options::default());
    for (id, providers) in [("absent", None), ("empty", Some(json!([])))] {
        let mut m = events_manifest(id);
        if let Some(providers) = providers {
            m["private_content_providers"] = providers;
        }
        let installed = install(&f, &Caller::Operator, "return 1;", &m);
        assert!(f.consent.decide(
            installed["card_id"].as_str().unwrap(),
            CardDecision::Approve,
            "operator"
        ));
        assert_eq!(
            f.module
                .handle(&reserved("broca"), OP, json!({"flow_id":id}))
                .unwrap(),
            json!({"flow_id":id,"version":1,"code_hash":installed["code_hash"],"providers":[]})
        );
    }
}

#[test]
fn providers_op_unknown_and_unapproved_have_same_refusal() {
    let f = fixture("providers-hidden", Options::default());
    install(
        &f,
        &Caller::Operator,
        "return 1;",
        &events_manifest("pending"),
    );
    let pending = f
        .module
        .handle(&reserved("broca"), OP, json!({"flow_id":"pending"}))
        .unwrap_err();
    let unknown = f
        .module
        .handle(&reserved("broca"), OP, json!({"flow_id":"unknown"}))
        .unwrap_err();
    assert_eq!(unknown, pending);
    assert_eq!(unknown.code, "flow_not_approved");
}

#[test]
fn providers_op_reads_template_version_not_instance_params() {
    let f = fixture("providers-template", Options::default());
    let mut m = events_manifest("provider-template");
    m["sinks"][0]["agent"] = json!("$self");
    m["params"] = json!({"provider":{"type":"enum","values":["openai","typesafe"]}});
    m["template"] = json!({"max_per_agent":2,"max_lifetime":"1d"});
    m["private_content_providers"] = json!(["typesafe"]);
    let first = f
        .module
        .rt
        .register_package("return 1;", &m.to_string())
        .unwrap();
    let instance = f
        .module
        .rt
        .instantiate(
            "provider-template",
            1,
            "SYNAPSE",
            &json!({"provider":"openai"}),
            "1h",
        )
        .unwrap();
    m["version"] = json!(2);
    m["private_content_providers"] = json!(["openai"]);
    f.module
        .rt
        .register_package("return 2;", &m.to_string())
        .unwrap();
    let flow_id = &instance["flow_id"];
    assert_eq!(
        f.module
            .handle(&reserved("broca"), OP, json!({"flow_id":flow_id}))
            .unwrap(),
        json!({"flow_id":flow_id,"version":1,"code_hash":first["code_hash"],"providers":["typesafe"]})
    );
}

#[test]
fn providers_op_is_a_management_query() {
    let op = basal_module::manifest::OPERATIONS
        .iter()
        .find(|op| op.0 == OP)
        .unwrap();
    assert_eq!(
        op.1,
        subc_protocol::manifest::ManagementOperationKind::Query
    );
    let docs = include_str!("../../../docs/ops.md");
    let section = docs
        .split("## flow.private_content_providers\n")
        .nth(1)
        .unwrap()
        .split("## flow.audience")
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
            json!({"flow_id":"string","version":"integer","code_hash":"string","providers":["string"]})
        ]
    );
}
