use basal_core::authorize::{self, ShellDenylist};
use basal_core::install::{self, InstallError};
use basal_core::manifest::{Audience, Manifest, ManifestError};
use basal_host::MockCatalog;
use basal_proto::{CallKind, Primitive};
use serde_json::{Value, json};

fn manifest(audience: Option<Value>) -> Value {
    let mut m = json!({
        "id": "audience-test", "version": 1, "purpose": "Delivery reach",
        "trigger": {"schedule": {"interval": "1h"}},
        "sinks": [{"agent": "SYNAPSE", "digest_max": "piggyback"}],
        "status": ["SYNAPSE"]
    });
    if let Some(audience) = audience {
        m["audience"] = audience;
        m["sinks"][0]["agent"] = json!("$audience");
        m["status"] = json!(["$audience"]);
    }
    m
}

fn parse(m: &Value) -> Result<Manifest, ManifestError> {
    Manifest::parse(&m.to_string())
}

#[test]
fn audience_field_shape_is_closed() {
    for value in [
        json!({"kind":"global"}),
        json!({"kind":"workspace","id":"team space"}),
        json!({"kind":"workspace","id":"$audience"}),
        json!({"kind":"agent","id":"SYNAPSE"}),
    ] {
        let m = parse(&manifest(Some(value.clone()))).unwrap();
        assert_eq!(serde_json::to_value(m.audience).unwrap(), value);
    }
    for bad in [
        json!({}),
        json!({"kind":"unknown"}),
        json!({"kind":"agent"}),
        json!({"kind":"workspace"}),
        json!({"kind":"global","id":"SYNAPSE"}),
        json!({"kind":"global","id":null}),
        json!({"kind":"global","extra":true}),
        json!({"kind":"agent","id":"SYNAPSE","extra":true}),
        json!({"kind":"workspace","id":"w","extra":true}),
        json!({"kind":"agent","id":null}),
        json!({"kind":"agent","id":""}),
        json!({"kind":"workspace","id":"  "}),
        json!({"kind":"workspace","id":"w".repeat(513)}),
    ] {
        assert!(
            parse(&manifest(Some(bad.clone()))).is_err(),
            "accepted {bad}"
        );
    }
}

#[test]
fn audience_sink_targets_cannot_mix_with_named_targets() {
    for field in ["sinks", "status"] {
        let mut m = manifest(Some(json!({"kind":"global"})));
        if field == "sinks" {
            m[field][0]["agent"] = json!("SYNAPSE");
        } else {
            m[field] = json!(["$audience", "SYNAPSE"]);
        }
        assert_eq!(parse(&m), Err(ManifestError::AudienceSinkNamed));
    }
}

#[test]
fn audience_placeholder_requires_audience_and_only_names_sinks() {
    for field in ["sinks", "status", "claims", "facts", "audience"] {
        let mut m = manifest(None);
        match field {
            "sinks" => m[field][0]["agent"] = json!("$audience"),
            "status" => m[field] = json!(["$audience"]),
            "claims" => m[field] = json!([{"agent":"$audience","source_kind":"idle"}]),
            "facts" => m[field] = json!({"targets":["$audience"],"text":false}),
            _ => m[field] = json!({"kind":"agent","id":"$audience"}),
        }
        assert_eq!(
            parse(&m),
            Err(ManifestError::AudiencePlaceholderWithoutAudience),
            "{field}"
        );
    }
}

#[test]
fn agent_audience_resolves_to_same_stable_author() {
    let catalog = MockCatalog::standard();
    catalog.add_named_agent("agent_owner", "SYNAPSE");
    for id in ["SYNAPSE", "agent_owner"] {
        let m = parse(&manifest(Some(json!({"kind":"agent","id":id})))).unwrap();
        install::validate(
            &m,
            "agent_owner",
            &catalog,
            &ShellDenylist::default(),
            false,
        )
        .unwrap();
        install::validate(&m, "operator", &catalog, &ShellDenylist::default(), false).unwrap();
        install::validate(
            &m,
            "local:unverified",
            &catalog,
            &ShellDenylist::default(),
            false,
        )
        .unwrap();
        assert!(matches!(
            install::validate(&m, "ALF", &catalog, &ShellDenylist::default(), false),
            Err(InstallError::ForeignAgentTarget {
                field: "audience.id",
                ..
            })
        ));
    }
    let m = parse(&manifest(Some(json!({"kind":"agent","id":"missing"})))).unwrap();
    assert!(matches!(
        install::validate(&m, "operator", &catalog, &ShellDenylist::default(), false),
        Err(InstallError::UnknownAgent {
            field: "audience.id",
            ..
        })
    ));
}

#[test]
fn broad_audience_requires_operator_authorship() {
    let catalog = MockCatalog::standard();
    catalog.add_workspace("team");
    for audience in [
        json!({"kind":"workspace","id":"team"}),
        json!({"kind":"global"}),
    ] {
        let m = parse(&manifest(Some(audience))).unwrap();
        assert!(matches!(
            install::validate(&m, "SYNAPSE", &catalog, &ShellDenylist::default(), false),
            Err(InstallError::AudienceRequiresOperator)
        ));
    }
}

#[test]
fn operator_and_local_authors_may_request_broad_audiences() {
    let catalog = MockCatalog::standard();
    catalog.add_workspace("team");
    for audience in [
        json!({"kind":"workspace","id":"team"}),
        json!({"kind":"global"}),
    ] {
        let m = parse(&manifest(Some(audience))).unwrap();
        for author in ["operator", "local:unverified"] {
            install::validate(&m, author, &catalog, &ShellDenylist::default(), false).unwrap();
        }
    }
}

#[test]
fn workspace_audience_must_exist() {
    let catalog = MockCatalog::standard();
    let m = parse(&manifest(Some(json!({"kind":"workspace","id":"missing"})))).unwrap();
    assert!(
        matches!(install::validate(&m, "operator", &catalog, &ShellDenylist::default(), false), Err(InstallError::UnknownWorkspace(id)) if id == "missing")
    );
}

#[test]
fn audience_writes_check_declared_sink_kind_not_recipient() {
    let catalog = MockCatalog::standard();
    let mut m = parse(&manifest(Some(json!({"kind":"global"})))).unwrap();
    for (kind, args) in [
        (
            Primitive::SinkDigest,
            json!({"agent":"unknown","item":{},"action":"wake"}),
        ),
        (
            Primitive::SinkStatus,
            json!({"agent":"unknown","value":"hello"}),
        ),
    ] {
        authorize::check(
            &m,
            &ShellDenylist::default(),
            &catalog,
            &CallKind::Primitive(kind),
            &args,
        )
        .unwrap();
        assert!(
            authorize::check(
                &m,
                &ShellDenylist::default(),
                &catalog,
                &CallKind::Primitive(kind),
                &json!({})
            )
            .is_err()
        );
    }
    assert_eq!(
        m.digest_cap("unknown"),
        Some(basal_core::manifest::DigestAction::Piggyback)
    );
    m.sinks.clear();
    m.status.clear();
    for kind in [Primitive::SinkDigest, Primitive::SinkStatus] {
        assert!(
            authorize::check(
                &m,
                &ShellDenylist::default(),
                &catalog,
                &CallKind::Primitive(kind),
                &json!({"agent":"unknown"})
            )
            .is_err()
        );
    }
}

#[test]
fn static_writes_still_require_static_targets_and_caps() {
    let catalog = MockCatalog::standard();
    let m = parse(&manifest(None)).unwrap();
    assert_eq!(m.audience, None);
    for kind in [Primitive::SinkDigest, Primitive::SinkStatus] {
        authorize::check(
            &m,
            &ShellDenylist::default(),
            &catalog,
            &CallKind::Primitive(kind),
            &json!({"agent":"SYNAPSE"}),
        )
        .unwrap();
        assert!(
            authorize::check(
                &m,
                &ShellDenylist::default(),
                &catalog,
                &CallKind::Primitive(kind),
                &json!({"agent":"ALF"})
            )
            .is_err()
        );
    }
    assert!(
        authorize::check(
            &m,
            &ShellDenylist::default(),
            &catalog,
            &CallKind::Primitive(Primitive::SinkDigest),
            &json!({"agent":"SYNAPSE","action":"wake"})
        )
        .is_err()
    );
}

#[test]
fn audience_changes_are_covered_by_code_hash() {
    let a = manifest(Some(json!({"kind":"agent","id":"SYNAPSE"}))).to_string();
    let b = manifest(Some(json!({"kind":"global"}))).to_string();
    assert_ne!(
        basal_core::ids::code_hash("return 1;", &a),
        basal_core::ids::code_hash("return 1;", &b)
    );
    assert!(matches!(
        parse(&manifest(Some(json!({"kind":"global"}))))
            .unwrap()
            .audience,
        Some(Audience::Global {})
    ));
}
