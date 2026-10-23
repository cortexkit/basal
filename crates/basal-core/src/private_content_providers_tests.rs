use super::*;
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({"id":"providers","version":1,"purpose":"Check provider grants",
        "trigger":{"schedule":{"cron":"* * * * *"}}})
}

fn invalid_list(providers: Value) {
    let mut value = manifest();
    value["private_content_providers"] = providers;
    let error = Manifest::parse(&value.to_string()).unwrap_err();
    assert!(
        matches!(&error, ManifestError::Invalid { field, .. } if field == "private_content_providers"),
        "{value}: {error}"
    );
}

#[test]
fn valid_provider_lists_preserve_order() {
    for providers in [
        json!(["openai", "cloudflare-workers-ai", "typesafe"]),
        json!(["a", "a0-", "z".repeat(64)]),
        json!((0..16).map(|i| format!("provider-{i}")).collect::<Vec<_>>()),
    ] {
        let mut value = manifest();
        value["private_content_providers"] = providers.clone();
        let parsed = Manifest::parse(&value.to_string()).unwrap();
        assert_eq!(json!(parsed.private_content_providers), providers);
        assert_eq!(
            serde_json::to_value(&parsed).unwrap()["private_content_providers"],
            providers
        );
    }
}

#[test]
fn absent_and_empty_provider_lists_mean_base_set_only() {
    let mut value = manifest();
    let absent = Manifest::parse(&value.to_string()).unwrap();
    value["private_content_providers"] = json!([]);
    let empty = Manifest::parse(&value.to_string()).unwrap();
    assert!(absent.private_content_providers.is_empty());
    assert_eq!(absent, empty);
}

#[test]
fn provider_ids_reject_invalid_shapes() {
    for id in [
        "", "OpenAI", "open_ai", "open.ai", "é", "a\n", " a", "0ai", "-ai",
    ] {
        invalid_list(json!([id]));
    }
    invalid_list(json!(["a".repeat(65)]));
}

#[test]
fn provider_lists_reject_more_than_sixteen() {
    invalid_list(json!(
        (0..17).map(|i| format!("provider-{i}")).collect::<Vec<_>>()
    ));
}

#[test]
fn provider_lists_reject_duplicates() {
    invalid_list(json!(["openai", "typesafe", "openai"]));
}

#[test]
fn provider_lists_reject_non_arrays_and_non_strings() {
    for providers in [
        json!(null),
        json!("openai"),
        json!({}),
        json!(1),
        json!([1]),
        json!([null]),
        json!([{}]),
        json!([true]),
        json!([["openai"]]),
    ] {
        invalid_list(providers);
    }
}

#[test]
fn provider_lists_reject_duplicate_json_keys() {
    let text = r#"{"id":"providers","version":1,"purpose":"Check provider grants",
        "trigger":{"schedule":{"cron":"* * * * *"}},
        "private_content_providers":["openai"],"private_content_providers":["typesafe"]}"#;
    assert!(matches!(
        Manifest::parse(text),
        Err(ManifestError::Decode(_))
    ));
    assert!(matches!(
        Manifest::parse_stored(text),
        Err(ManifestError::Decode(_))
    ));
}

#[test]
fn templates_accept_only_literal_provider_lists() {
    let mut value = manifest();
    value["params"] = json!({"provider":{"type":"enum","values":["openai","typesafe"]}});
    value["template"] = json!({"max_per_agent":1,"max_lifetime":"1d"});
    value["private_content_providers"] = json!(["openai", "typesafe"]);
    let mut parsed = Manifest::parse(&value.to_string()).unwrap();
    let values =
        crate::templates::validate_values(&parsed, &json!({"provider":"openai"}), 0).unwrap();
    crate::templates::effective(&mut parsed, &values);
    assert_eq!(parsed.private_content_providers, ["openai", "typesafe"]);
    for providers in [json!(["$params.provider"]), json!("$params.provider")] {
        value["private_content_providers"] = providers;
        assert!(matches!(
            Manifest::parse(&value.to_string()),
            Err(ManifestError::Invalid { field, .. }) if field == "private_content_providers"
        ));
    }
}

#[test]
fn stored_provider_lists_do_not_reapply_intake_rules() {
    let mut value = manifest();
    value["private_content_providers"] = json!(["Legacy_ID", "Legacy_ID", "0legacy"]);
    let parsed = Manifest::parse_stored(&value.to_string()).unwrap();
    assert_eq!(
        parsed.private_content_providers,
        ["Legacy_ID", "Legacy_ID", "0legacy"]
    );
    assert!(Manifest::parse(&value.to_string()).is_err());
    value["private_content_providers"] =
        json!((0..17).map(|i| format!("provider-{i}")).collect::<Vec<_>>());
    assert_eq!(
        Manifest::parse_stored(&value.to_string())
            .unwrap()
            .private_content_providers
            .len(),
        17
    );
}
