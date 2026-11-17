use super::*;
use basal_host::{EventBody, EventDecl, EventOrigin, MockCatalog, OpDecl};
use serde_json::json;

#[test]
fn prefrontal_rename_loop_rule_matches_aliases() {
    let catalog = MockCatalog::standard();
    for module in ["prefrontal-core", "prefrontal"] {
        catalog.set_flow_capable(module, true);
        catalog.set_event(
            module,
            "changed",
            1,
            EventDecl {
                origin: EventOrigin::Internal,
                body: EventBody::Inline,
                headers: vec![],
            },
        );
        catalog.set_op(
            module,
            "mutate",
            OpDecl {
                kind: Some(OpKind::Mutate),
                cause_echo: false,
                shell_capable: false,
            },
        );
    }
    for trigger in ["prefrontal-core", "prefrontal"] {
        for operation in ["prefrontal-core", "prefrontal"] {
            let manifest = Manifest::parse(&json!({"id":"loop","version":1,
                "explanation":"Checks loop protection across renamed module ids.","purpose":"Check loops",
                "trigger":{"events":[{"module":trigger,"name":"changed","version":1}]},
                "ops":[{"module":operation,"op":"mutate"}]}).to_string()).unwrap();
            assert!(
                matches!(
                    validate(
                        &manifest,
                        "operator",
                        &catalog,
                        &ShellDenylist::default(),
                        false
                    ),
                    Err(InstallError::LoopRule { .. })
                ),
                "{trigger} -> {operation}"
            );
        }
    }
}
