use super::*;
use basal_host::{EventBody, EventDecl, EventOrigin, MockCatalog, OpDecl};
use serde_json::json;

#[test]
fn prefrontal_loop_rule_still_applies() {
    let catalog = MockCatalog::standard();
    catalog.set_flow_capable("prefrontal", true);
    catalog.set_event(
        "prefrontal",
        "changed",
        1,
        EventDecl {
            origin: EventOrigin::Internal,
            body: EventBody::Inline,
            headers: vec![],
        },
    );
    catalog.set_op(
        "prefrontal",
        "mutate",
        OpDecl {
            kind: Some(OpKind::Mutate),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let manifest = Manifest::parse(
        &json!({"id":"loop","version":1,
        "explanation":"Checks loop protection for prefrontal.","purpose":"Check loops",
        "trigger":{"events":[{"module":"prefrontal","name":"changed","version":1}]},
        "ops":[{"module":"prefrontal","op":"mutate"}]})
        .to_string(),
    )
    .unwrap();
    assert!(matches!(
        validate(
            &manifest,
            "operator",
            &catalog,
            &ShellDenylist::default(),
            false
        ),
        Err(InstallError::LoopRule { .. })
    ));
}
