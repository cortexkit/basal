use super::*;
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({"id":"approval-text","version":1,"purpose":"A test flow",
        "explanation":"Watches the hourly clock and records nothing.",
        "trigger":{"schedule":{"interval":"1h"}}})
}

#[test]
fn explanation_is_required_only_at_intake() {
    let mut value = manifest();
    value.as_object_mut().unwrap().remove("explanation");
    let text = value.to_string();
    let stored = Manifest::parse_stored(&text).unwrap();
    assert_eq!(stored.explanation, None);
    assert_eq!(stored.changes, None);
    assert_eq!(
        Manifest::parse(&text),
        Err(ManifestError::ExplanationRequired)
    );
    assert_eq!(
        Manifest::parse_ordinary(&text),
        Err(ManifestError::ExplanationRequired)
    );
}

#[test]
fn approval_text_lengths_count_unicode_scalars() {
    for (field, max) in [("explanation", 1500), ("changes", 600)] {
        for len in [1, max] {
            let mut value = manifest();
            value[field] = json!("🦀".repeat(len));
            let parsed = Manifest::parse(&value.to_string()).unwrap();
            assert_eq!(
                if field == "explanation" {
                    parsed.explanation
                } else {
                    parsed.changes
                },
                Some("🦀".repeat(len))
            );
        }
        let mut value = manifest();
        value[field] = json!("🦀".repeat(max + 1));
        assert!(matches!(Manifest::parse(&value.to_string()),
            Err(ManifestError::Invalid { field: refused, reason })
                if refused == field && reason == format!("longer than {max} Unicode scalar values")));
    }
}

#[test]
fn approval_text_refuses_blank_controls_and_non_strings() {
    for field in ["explanation", "changes"] {
        for text in [
            "",
            " \n \u{2003}",
            "watch\tclock",
            "watch\rclock",
            "watch\0clock",
            "watch\u{0085}clock",
        ] {
            let mut value = manifest();
            value[field] = json!(text);
            assert!(
                matches!(Manifest::parse(&value.to_string()),
                Err(ManifestError::Invalid { field: refused, .. }) if refused == field),
                "{field}: {text:?}"
            );
        }
        let mut value = manifest();
        value[field] = json!(" watch\nclock ");
        assert!(Manifest::parse(&value.to_string()).is_ok());
        for non_string in [Value::Null, json!(1), json!(false), json!([]), json!({})] {
            value[field] = non_string;
            assert!(matches!(
                Manifest::parse(&value.to_string()),
                Err(ManifestError::Decode(_))
            ));
        }
    }
}

#[test]
fn changes_follow_stored_history_not_the_version_number() {
    let mut value = manifest();
    value["version"] = json!(42);
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    assert_eq!(parsed.check_version_text(false), Ok(()));
    assert_eq!(
        parsed.check_version_text(true),
        Err(ManifestError::ChangesRequired)
    );
    value["changes"] = json!("Checks hourly instead of daily, to catch problems sooner.");
    let parsed = Manifest::parse(&value.to_string()).unwrap();
    assert_eq!(parsed.check_version_text(true), Ok(()));
    assert_eq!(
        parsed.check_version_text(false),
        Err(ManifestError::ChangesOnFirstVersion)
    );
}

#[test]
fn approval_text_parameter_names_remain_literal() {
    let mut value = manifest();
    value["explanation"] = json!("Reads $params.path to watch a file; never changes that file.");
    value["changes"] = json!("Uses $params.path instead of a fixed file for reuse.");
    let mut parsed = Manifest::parse(&value.to_string()).unwrap();
    crate::templates::effective(&mut parsed, &json!({"path":"/somewhere"}));
    assert_eq!(parsed.explanation.as_deref(), value["explanation"].as_str());
    assert_eq!(parsed.changes.as_deref(), value["changes"].as_str());
}
