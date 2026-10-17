use super::*;
use crate::authorize::{self, ShellDenylist};
use basal_host::MockCatalog;
use basal_proto::{CallKind, Primitive};
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({"id":"board-flow","version":1,"purpose":"Track work",
        "trigger":{"schedule":{"interval":"1h"}},
        "board":[{"agent":"SYNAPSE","title":"Work"}]})
}

#[test]
fn board_manifest_validates_titles_and_targets() {
    for grant in [
        json!({"agent":"SYNAPSE"}),
        json!({"agent":"SYNAPSE","title":""}),
        json!({"agent":"SYNAPSE","title":"  "}),
        json!({"agent":"SYNAPSE","title":"a\nb"}),
        json!({"agent":"SYNAPSE","title":"x".repeat(57)}),
        json!({"agent":"$audience","title":"Work"}),
        json!({"agent":"","title":"Work"}),
    ] {
        let mut m = manifest();
        m["board"] = json!([grant]);
        let error = Manifest::parse(&m.to_string()).unwrap_err();
        if grant.get("title").is_none() || grant["title"].as_str().unwrap().trim().is_empty() {
            assert!(
                error.to_string().contains("missing board lane title"),
                "{error}"
            );
        }
    }
    let mut m = manifest();
    m["board"][0]["title"] = json!("é".repeat(56));
    assert!(Manifest::parse(&m.to_string()).is_ok());
    m["board"] = json!([m["board"][0], m["board"][0]]);
    assert!(
        matches!(Manifest::parse(&m.to_string()), Err(ManifestError::Duplicate {field,..}) if field == "board")
    );
    for audience in [
        json!({"kind":"global"}),
        json!({"kind":"agent","id":"SYNAPSE"}),
    ] {
        let mut m = manifest();
        m["audience"] = audience;
        assert!(matches!(
            Manifest::parse(&m.to_string()),
            Err(ManifestError::AudienceSinkNamed)
        ));
        m["board"][0]["agent"] = json!("$audience");
        assert!(Manifest::parse(&m.to_string()).is_ok());
    }
}

#[test]
fn board_grants_resolve_self_and_enforce_author_ownership() {
    let catalog = MockCatalog::standard();
    catalog.add_named_agent("agent_owner", "SYNAPSE");
    for agent in ["$self", "SYNAPSE", "agent_owner"] {
        let mut value = manifest();
        value["board"][0]["agent"] = json!(agent);
        let mut parsed = Manifest::parse(&value.to_string()).unwrap();
        authorize::resolve_self(&mut parsed, "agent_owner");
        assert_eq!(
            parsed.agents(),
            [(
                "board",
                if agent == "$self" {
                    "agent_owner"
                } else {
                    agent
                }
            )]
        );
        crate::install::validate(
            &parsed,
            "agent_owner",
            &catalog,
            &ShellDenylist::default(),
            false,
        )
        .unwrap();
        assert!(matches!(
            crate::install::validate(&parsed, "ALF", &catalog, &ShellDenylist::default(), false),
            Err(crate::install::InstallError::ForeignAgentTarget { field: "board", .. })
        ));
    }
}

#[test]
fn board_authorization_checks_grants_before_dispatch() {
    let catalog = MockCatalog::standard();
    catalog.add_named_agent("agent_owner", "SYNAPSE");
    let kind = CallKind::Primitive(Primitive::SinkBoard);
    let mut value = manifest();
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    for agent in ["SYNAPSE", "agent_owner"] {
        authorize::check(
            &parsed,
            &ShellDenylist::default(),
            &catalog,
            &kind,
            &json!({"agent":agent}),
        )
        .unwrap();
    }
    let refusal = authorize::check(
        &parsed,
        &ShellDenylist::default(),
        &catalog,
        &kind,
        &json!({"agent":"ALF"}),
    )
    .unwrap_err();
    assert_eq!(refusal.code, "sink_board_target_not_granted");
    value["audience"] = json!({"kind":"global"});
    value["board"][0]["agent"] = json!("$audience");
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    authorize::check(
        &parsed,
        &ShellDenylist::default(),
        &catalog,
        &kind,
        &json!({"agent":"ALF"}),
    )
    .unwrap();
    value["board"] = json!([]);
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    assert_eq!(
        authorize::check(
            &parsed,
            &ShellDenylist::default(),
            &catalog,
            &kind,
            &json!({"agent":"ALF"})
        )
        .unwrap_err()
        .code,
        "sink_board_target_not_granted"
    );
}
