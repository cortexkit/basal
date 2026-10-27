use super::Manifest;
use serde_json::{Value, json};

fn manifest() -> Value {
    json!({"id":"secret-flow","version":1,"purpose":"Read an authenticated API",
        "trigger":{"schedule":{"interval":"1h"}},
        "net":{"fetch":[{"host":"x.test","methods":["GET"]}]},
        "secrets":[{"name":"key","target":{"scheme":"https","host":"x.test","path":"/repos/a"},"methods":["GET"]}]})
}

#[test]
fn flow_secrets_manifest_intake_checks_do_not_tighten_stored_reads() {
    let valid = manifest();
    assert!(Manifest::parse(&valid.to_string()).is_ok());
    let mut invalids = Vec::new();
    for methods in [
        json!([]),
        json!(["GET", "GET"]),
        json!(["get"]),
        json!(["G3T"]),
        json!(["A".repeat(17)]),
        json!(null),
    ] {
        let mut value = valid.clone();
        value["secrets"][0]["methods"] = methods;
        invalids.push(value);
    }
    for count in [0, 17] {
        let mut value = valid.clone();
        value["secrets"] = json!(vec![valid["secrets"][0].clone(); count]);
        invalids.push(value);
    }
    for path in [
        "/repos/a/../b",
        "/repos/a/%2Fother",
        "/repos/a//b",
        "/repos/a?x=y",
    ] {
        let mut value = valid.clone();
        value["secrets"][0]["target"]["path"] = json!(path);
        invalids.push(value);
    }
    let mut login = valid.clone();
    login["secrets"][0]["target"] = json!({"origin":"https://x.test"});
    invalids.push(login.clone());
    login["secrets"][0]
        .as_object_mut()
        .unwrap()
        .remove("methods");
    assert!(Manifest::parse(&login.to_string()).is_ok());
    for value in invalids {
        assert!(
            Manifest::parse(&value.to_string()).is_err(),
            "intake accepted {value}"
        );
        assert!(
            Manifest::parse_stored(&value.to_string()).is_ok(),
            "stored row was tightened: {value}"
        );
    }
}

#[test]
fn flow_secrets_declarations_are_part_of_the_exact_code_hash() {
    let first = manifest().to_string();
    let mut second = manifest();
    second["secrets"][0]["target"]["path"] = json!("/repos/b");
    assert_ne!(
        crate::ids::code_hash("return 1;", &first),
        crate::ids::code_hash("return 1;", &second.to_string())
    );
}

#[test]
fn flow_secrets_null_fields_cannot_hide_declarations_or_login_methods() {
    let mut value = manifest();
    value["secrets"] = Value::Null;
    assert!(Manifest::parse(&value.to_string()).is_err());
    assert!(Manifest::parse_stored(&value.to_string()).is_ok());
    let mut value = manifest();
    value["secrets"][0]["target"] = json!({"origin":"https://x.test"});
    value["secrets"][0]["methods"] = Value::Null;
    assert!(Manifest::parse(&value.to_string()).is_err());
    assert!(Manifest::parse_stored(&value.to_string()).is_ok());
}
