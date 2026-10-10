use basal_core::grant_loss::grant_key;
use serde_json::json;

#[test]
fn grant_subject_keys_preserve_strings_and_hash_only_oversized_bytes() {
    assert_eq!(
        grant_key(&json!("  opaque string  ")).unwrap(),
        ("  opaque string  ".into(), true)
    );
    assert_eq!(
        grant_key(&json!({"z":0,"a":{"z":2,"a":1}})).unwrap(),
        (r#"{"a":{"a":1,"z":2},"z":0}"#.into(), true)
    );
    let at_limit = "x".repeat(4096);
    assert_eq!(grant_key(&json!(at_limit)).unwrap(), (at_limit, true));
    let oversized = "x".repeat(4097);
    assert_eq!(
        grant_key(&json!(oversized)).unwrap(),
        (
            blake3::hash(oversized.as_bytes()).to_hex().to_string(),
            false
        )
    );
}
