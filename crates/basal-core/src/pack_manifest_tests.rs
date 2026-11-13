use super::*;
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({
        "id":"family-planning", "version":1,
        "purpose":"Plan family commitments",
        "explanation":"Read the selected calendar and reminder list and send a planning summary to the bound destinations.",
        "trigger":{"schedule":{"interval":"1h"}},
        "abilities":[
            {"key":"family_calendar","token":"cap/calendar.read@1","scope":["calendar"]},
            {"key":"family_reminders","token":"cap/reminders.read@1","scope":["list"]}
        ],
        "destinations":[
            {"key":"planning","kind":"room"},
            {"key":"me","kind":"agent_inbox"},
            {"key":"alerts","kind":"notice","severity_max":"attention"}
        ]
    })
}

fn invalid_field(value: &Value, path: &str) {
    let error = Manifest::parse(&value.to_string()).unwrap_err();
    assert!(
        matches!(&error, ManifestError::Invalid { field, .. } if field == path),
        "{value}: {error:?}"
    );
}

fn pack_code(value: &Value, code: &str) {
    let error = Manifest::parse_ordinary(&value.to_string()).unwrap_err();
    assert!(
        matches!(&error, ManifestError::Pack { code: actual, .. } if *actual == code),
        "{value}: {error:?}"
    );
}

#[test]
fn pack_shapes_round_trip_stored_but_wait_for_runtime_at_intake() {
    let mut value = manifest();
    for kind in ["room", "agent_inbox", "plexus_action"] {
        value["destinations"][0]["kind"] = json!(kind);
        pack_code(&value, "pack_runtime_unavailable");
    }
    for severity in ["info", "attention"] {
        value["destinations"][2]["severity_max"] = json!(severity);
        let stored = Manifest::parse_stored(&value.to_string()).unwrap();
        assert_eq!(stored.abilities.as_ref().unwrap().len(), 2);
        assert_eq!(stored.destinations.as_ref().unwrap().len(), 3);
        let round_trip = serde_json::to_string(&stored).unwrap();
        assert_eq!(Manifest::parse_stored(&round_trip).unwrap(), stored);
        pack_code(&value, "pack_runtime_unavailable");
    }
    for member in ["abilities", "destinations"] {
        let mut value = manifest();
        value
            .as_object_mut()
            .unwrap()
            .remove(if member == "abilities" {
                "destinations"
            } else {
                "abilities"
            });
        value[member] = json!([]);
        pack_code(&value, "pack_runtime_unavailable");
        value.as_object_mut().unwrap().remove(member);
        assert!(Manifest::parse_ordinary(&value.to_string()).is_ok());
    }
}

#[test]
fn pack_token_grammar_is_exact() {
    for token in [
        "cap/a@1",
        "cap/calendar.read@1",
        "cap/a0_b.c-d@123",
        "cap/a@999999999999999999999999999999999999",
    ] {
        assert!(packs::valid_token(token), "{token}");
    }
    for token in [
        "",
        "calendar.read",
        "cap/@1",
        "cap/A@1",
        "cap/0a@1",
        "cap/a@0",
        "cap/a@01",
        "cap/a@",
        "cap/a@+1",
        "cap/a@-1",
        "cap/a@1.0",
        "cap/a@1@2",
        "cap/a/b@1",
        "cap/a b@1",
        "cap/é@1",
        "cap/a@١",
        "cap/a@1\n",
        " cap/a@1",
    ] {
        assert!(!packs::valid_token(token), "{token:?}");
        let mut value = manifest();
        value["abilities"][0]["token"] = json!(token);
        invalid_field(&value, "abilities[0].token");
    }
}

#[test]
fn pack_keys_and_scope_are_bounded_and_unique() {
    for list in ["abilities", "destinations"] {
        for key in ["", "A", "0key", "a-b", "a.b", "é", &"a".repeat(41)] {
            let mut value = manifest();
            value[list][0]["key"] = json!(key);
            invalid_field(&value, &format!("{list}[0].key"));
        }
        let mut value = manifest();
        value[list][1]["key"] = value[list][0]["key"].clone();
        invalid_field(&value, &format!("{list}[1].key"));
        let entry = value[list][0].clone();
        value[list] = json!(
            (0..16)
                .map(|i| {
                    let mut entry = entry.clone();
                    entry["key"] = json!(format!("key_{i}"));
                    entry
                })
                .collect::<Vec<_>>()
        );
        pack_code(&value, "pack_runtime_unavailable");
        value[list].as_array_mut().unwrap().push(entry);
        invalid_field(&value, list);
    }
    for (scope, path) in [
        (json!([]), "abilities[0].scope"),
        (json!(["calendar", "calendar"]), "abilities[0].scope[1]"),
        (json!(["Calendar"]), "abilities[0].scope[0]"),
        (
            json!((0..9).map(|i| format!("key_{i}")).collect::<Vec<_>>()),
            "abilities[0].scope",
        ),
    ] {
        let mut value = manifest();
        value["abilities"][0]["scope"] = scope;
        invalid_field(&value, path);
    }
    let mut value = manifest();
    value["abilities"][0]["key"] = json!("a".repeat(40));
    value["destinations"][0]["key"] = value["abilities"][0]["key"].clone();
    value["abilities"][0]["scope"] = json!((0..8).map(|i| format!("key_{i}")).collect::<Vec<_>>());
    pack_code(&value, "pack_runtime_unavailable");
}

#[test]
fn pack_destination_kind_and_severity_are_checked() {
    for (kind, severity, field) in [
        ("unknown", None, "kind"),
        ("notice", None, "severity_max"),
        ("notice", Some("urgent"), "severity_max"),
        ("notice", Some("INFO"), "severity_max"),
        ("room", Some("attention"), "severity_max"),
        ("agent_inbox", Some("info"), "severity_max"),
        ("plexus_action", Some("info"), "severity_max"),
    ] {
        let mut value = manifest();
        value["destinations"][0] = json!({"key":"planning", "kind":kind});
        if let Some(severity) = severity {
            value["destinations"][0]["severity_max"] = json!(severity);
        }
        invalid_field(&value, &format!("destinations[0].{field}"));
    }
}

#[test]
fn pack_decode_errors_name_the_field_and_keep_duplicate_json_key_refusals() {
    for (pointer, replacement, path) in [
        ("/abilities", json!(null), "abilities"),
        ("/destinations", json!({}), "destinations"),
        ("/abilities/0", json!(1), "abilities[0]"),
        ("/abilities/0/key", json!(false), "abilities[0].key"),
        ("/abilities/0/token", json!([]), "abilities[0].token"),
        ("/abilities/0/scope", json!(null), "abilities[0].scope"),
        ("/abilities/0/scope/0", json!(4), "abilities[0].scope[0]"),
        ("/destinations/0/kind", json!(null), "destinations[0].kind"),
    ] {
        let mut value = manifest();
        *value.pointer_mut(pointer).unwrap() = replacement;
        invalid_field(&value, path);
    }
    for (list, field) in [("abilities", "token"), ("destinations", "kind")] {
        let mut value = manifest();
        value[list][0].as_object_mut().unwrap().remove(field);
        invalid_field(&value, &format!("{list}[0].{field}"));
        value = manifest();
        value[list][0]["recipient"] = json!("forbidden");
        invalid_field(&value, &format!("{list}[0].recipient"));
    }
    let mut value = manifest();
    value["destinations"][0]["severity_max"] = Value::Null;
    invalid_field(&value, "destinations[0].severity_max");
    let text = manifest().to_string().replace(
        "\"family_calendar\"",
        "\"family_calendar\",\"key\":\"other\"",
    );
    assert!(matches!(
        Manifest::parse(&text),
        Err(ManifestError::Decode(_))
    ));
    assert!(matches!(
        Manifest::parse_stored(&text),
        Err(ManifestError::Decode(_))
    ));
}

#[test]
fn pack_net_fetch_is_forbidden() {
    let mut value = manifest();
    value["net"] = json!({"fetch":[{"host":"calendar.example"}]});
    pack_code(&value, "pack_net_fetch_forbidden");
    assert!(Manifest::parse_stored(&value.to_string()).is_ok());
    value["abilities"] = json!([]);
    pack_code(&value, "pack_runtime_unavailable");
}

#[test]
fn pack_literal_grants_are_forbidden_for_either_member() {
    for member in ["abilities", "destinations"] {
        for (grant, entries) in [
            (
                "sinks",
                json!([{"agent":"SYNAPSE", "digest_max":"piggyback"}]),
            ),
            ("ops", json!([{"module":"calendar", "op":"events.list"}])),
        ] {
            let mut value = manifest();
            value
                .as_object_mut()
                .unwrap()
                .remove(if member == "abilities" {
                    "destinations"
                } else {
                    "abilities"
                });
            value[member] = json!([]);
            value[grant] = entries;
            pack_code(&value, "pack_literal_grant");
            assert!(Manifest::parse_stored(&value.to_string()).is_ok());
        }
    }
}

#[test]
fn pack_stored_decode_does_not_apply_semantic_intake_rules() {
    let mut value = manifest();
    value["abilities"][0] = json!({"key":"Old-Key","token":"legacy","scope":[]});
    value["destinations"][0] = json!({"key":"Old-Key","kind":"legacy","severity_max":"urgent"});
    let parsed = Manifest::parse_stored(&value.to_string()).unwrap();
    let round_trip = serde_json::to_string(&parsed).unwrap();
    assert_eq!(Manifest::parse_stored(&round_trip).unwrap(), parsed);
    invalid_field(&value, "abilities[0].key");
}

#[test]
fn pack_templates_and_packages_refuse_abilities() {
    for template in [false, true] {
        let mut value = manifest();
        if template {
            value["params"] = json!({"note":{"type":"string","max_bytes":32}});
            value["template"] = json!({"max_per_agent":1,"max_live":2,"max_lifetime":"1d"});
        }
        let decoded = Manifest::decode(&value.to_string()).unwrap();
        let error = decoded.check_for_intake(IntakeKind::Package).unwrap_err();
        assert!(matches!(
            error,
            ManifestError::Pack {
                code: "pack_template_unsupported",
                ..
            }
        ));
    }
}
