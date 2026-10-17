use super::*;
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({"id":"boards","version":1,"purpose":"Read boards",
        "trigger":{"schedule":{"cron":"* * * * *"}},
        "ops":[{"module":"prefrontal-core","op":"board.read"}],
        "board_read":["SYNAPSE"]})
}

#[test]
fn board_read_parses_and_preserves_string_references() {
    let value = manifest();
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    assert_eq!(parsed.board_read, ["SYNAPSE"]);
    assert_eq!(parsed.agents(), [("board_read", "SYNAPSE")]);
    assert_eq!(
        serde_json::to_value(&parsed).unwrap()["board_read"],
        value["board_read"]
    );
    let mut absent = value;
    absent.as_object_mut().unwrap().remove("ops");
    absent.as_object_mut().unwrap().remove("board_read");
    assert!(
        Manifest::parse(&absent.to_string())
            .unwrap()
            .board_read
            .is_empty()
    );
}

#[test]
fn board_read_requires_nonempty_string_references_and_rejects_duplicates() {
    for refs in [
        json!(null),
        json!("SYNAPSE"),
        json!([{}]),
        json!([1]),
        json!([""]),
        json!([" "]),
        json!(["$other"]),
        json!(["SYNAPSE", "SYNAPSE"]),
        json!([]),
    ] {
        let mut value = manifest();
        value["board_read"] = refs;
        assert!(Manifest::parse(&value.to_string()).is_err(), "{value}");
    }
    let mut value = manifest();
    value["board_read"] = json!(["SYNAPSE", "ALF"]);
    assert!(Manifest::parse(&value.to_string()).is_ok());
    value["board_write"] = json!([]);
    assert!(matches!(
        Manifest::parse(&value.to_string()),
        Err(ManifestError::Decode(_))
    ));
    value.as_object_mut().unwrap().remove("board_write");
    value["board_read"] = json!(["SYNAPSE", "SYNAPSE"]);
    assert!(
        matches!(Manifest::parse(&value.to_string()), Err(ManifestError::Duplicate { field, .. }) if field == "board_read")
    );
    value["board_read"] = json!((0..65).map(|i| format!("Agent{i}")).collect::<Vec<_>>());
    assert!(
        matches!(Manifest::parse(&value.to_string()), Err(ManifestError::Invalid { field, .. }) if field == "board_read")
    );
}

#[test]
fn board_read_audience_references_must_match_audience_presence() {
    for audience in [
        json!({"kind":"global"}),
        json!({"kind":"workspace","id":"work"}),
        json!({"kind":"agent","id":"SYNAPSE"}),
    ] {
        for target in ["SYNAPSE", "$self"] {
            let mut value = manifest();
            value["audience"] = audience.clone();
            value["board_read"] = json!([target]);
            assert!(
                matches!(
                    Manifest::parse(&value.to_string()),
                    Err(ManifestError::AudienceSinkNamed)
                ),
                "{value}"
            );
        }
        let mut value = manifest();
        value["audience"] = audience;
        value["board_read"] = json!(["$audience"]);
        assert!(Manifest::parse(&value.to_string()).is_ok());
    }
    let mut value = manifest();
    value["board_read"] = json!(["$audience"]);
    assert!(matches!(
        Manifest::parse(&value.to_string()),
        Err(ManifestError::AudiencePlaceholderWithoutAudience)
    ));
}

#[test]
fn board_read_op_requires_a_nonempty_grant() {
    for refs in [None, Some(json!([]))] {
        let mut value = manifest();
        value.as_object_mut().unwrap().remove("board_read");
        if let Some(refs) = refs {
            value["board_read"] = refs;
        }
        let error = Manifest::parse(&value.to_string()).unwrap_err();
        assert!(
            matches!(&error, ManifestError::Invalid { field, .. } if field == "board_read"),
            "{error}"
        );
    }
}

#[test]
fn board_read_grants_require_the_exact_core_op() {
    for ops in [
        json!([]),
        json!([{"module":"other","op":"board.read"}]),
        json!([{"module":"prefrontal-core","op":"board.write"}]),
    ] {
        let mut value = manifest();
        value["ops"] = ops;
        let error = Manifest::parse(&value.to_string()).unwrap_err();
        assert!(
            matches!(&error, ManifestError::Invalid { field, .. } if field == "board_read"),
            "{error}"
        );
    }
}

#[test]
fn stored_manifests_skip_only_intake_coupling() {
    let mut legacy = manifest();
    legacy.as_object_mut().unwrap().remove("board_read");
    assert!(
        Manifest::parse_stored(&legacy.to_string())
            .unwrap()
            .board_read
            .is_empty()
    );
    let mut without_op = manifest();
    without_op.as_object_mut().unwrap().remove("ops");
    assert_eq!(
        Manifest::parse_stored(&without_op.to_string())
            .unwrap()
            .board_read,
        ["SYNAPSE"]
    );
    for bad in [
        json!(null),
        json!([{}]),
        json!([""]),
        json!(["SYNAPSE", "SYNAPSE"]),
        json!(["$audience"]),
    ] {
        let mut value = legacy.clone();
        value["board_read"] = bad;
        assert!(
            Manifest::parse_stored(&value.to_string()).is_err(),
            "{value}"
        );
    }
    legacy["unknown"] = json!(true);
    assert!(Manifest::parse_stored(&legacy.to_string()).is_err());
    legacy.as_object_mut().unwrap().remove("unknown");
    legacy["version"] = json!(0);
    assert!(Manifest::parse_stored(&legacy.to_string()).is_err());
}

#[test]
fn board_read_self_is_enumerated_for_package_resolution() {
    let mut value = manifest();
    value["board_read"] = json!(["$self"]);
    let mut parsed = Manifest::parse(&value.to_string()).unwrap();
    assert_eq!(parsed.agents(), [("board_read", "$self")]);
    crate::authorize::resolve_self(&mut parsed, "agent_owner");
    assert_eq!(parsed.board_read, ["agent_owner"]);
    assert_eq!(value["board_read"], json!(["$self"]));
}

#[test]
fn board_read_agents_resolve_names_and_ids_and_refuse_unknown_targets() {
    use basal_host::{MockCatalog, OpDecl, OpKind};
    let catalog = MockCatalog::standard();
    catalog.add_named_agent("agent_owner", "SYNAPSE");
    catalog.set_op(
        "prefrontal-core",
        "board.read",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    for target in ["SYNAPSE", "agent_owner", "NoSuchAgent"] {
        let mut value = manifest();
        value["board_read"] = json!([target]);
        let parsed = Manifest::parse(&value.to_string()).unwrap();
        let result = crate::install::validate(
            &parsed,
            "operator",
            &catalog,
            &crate::authorize::ShellDenylist::default(),
            false,
        );
        if target == "NoSuchAgent" {
            assert!(
                matches!(result, Err(crate::InstallError::UnknownAgent { field, .. }) if field == "board_read")
            );
        } else {
            result.unwrap();
            crate::install::validate(
                &parsed,
                "agent_owner",
                &catalog,
                &crate::authorize::ShellDenylist::default(),
                false,
            )
            .unwrap();
        }
    }
}
