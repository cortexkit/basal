use basal_module::{manifest, tool};
use cortexkit_role_tool_provider::SYSTEM_TEXT_DEFAULT;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TEXT_DIGEST: &str = "ac2a3af84c4039603f45e90c8ae1b28fd8c9a4b40e9b6cc3cdef316bf82dc5d0";
const EMPTY_TEXT_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn arguments() -> Value {
    json!({
        "params":{"capabilities":["basal:codemode/v1","basal:flow.manage/v1"]},
        "system_text":{"params":{}}
    })
}

#[test]
fn default_system_text_is_declared_and_its_exact_bytes_are_pinned() {
    let manifest = serde_json::to_value(manifest::manifest()).unwrap();
    assert!(
        manifest["capabilities"]["provides"]
            .as_array()
            .unwrap()
            .contains(&json!(SYSTEM_TEXT_DEFAULT))
    );
    let answer = tool::catalog(arguments()).unwrap();
    let text = answer["system_text"]["text"].as_str().unwrap();
    assert_eq!(text, include_str!("../src/codemode_system_text.md"));
    assert_eq!(
        format!("{:x}", Sha256::digest(text.as_bytes())),
        TEXT_DIGEST
    );
    assert_eq!(answer["system_text"]["item_digest"], TEXT_DIGEST);
    assert_eq!(answer["system_text"]["preflight_digest"], TEXT_DIGEST);
    assert!(answer.get("presets").is_none());
    assert!(answer["system_text"].get("composition_digest").is_none());
}

#[test]
fn catalog_tool_bytes_are_unchanged_by_system_text_support() {
    let with_text = tool::catalog(arguments()).unwrap();
    let without_text = tool::catalog(json!({"params":arguments()["params"]})).unwrap();
    assert!(without_text.get("system_text").is_none());
    assert_eq!(with_text["tools"], without_text["tools"]);
    // SHA-256 of the serialized tools array before default system text was
    // added: descriptions, schemas and every other tool field are pinned.
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&with_text["tools"]).unwrap())
        ),
        "e0ba997bbd48b5e8ddbbf8fec252e31239ae8c09649bab24ac01ac442c2ed93e"
    );
}

#[test]
fn flow_only_catalog_has_empty_system_text_with_matching_digests() {
    let mut request = json!({
        "params":{"capabilities":["basal:flow.manage/v1"]},
        "system_text":{"params":{}},
        "composition":{}
    });
    let full = tool::catalog(request.clone()).unwrap();
    assert_eq!(full["system_text"]["text"], "");
    assert_eq!(full["system_text"]["item_digest"], EMPTY_TEXT_DIGEST);
    assert_eq!(full["system_text"]["preflight_digest"], EMPTY_TEXT_DIGEST);
    assert_eq!(full["system_text"]["tool_names"], json!(["flow"]));
    assert_eq!(
        full["system_text"]["composition_digest"],
        "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
    request["digest_only"] = json!(true);
    assert_eq!(
        tool::catalog(request).unwrap(),
        json!({"generation":full["generation"],"catalog_digest":full["catalog_digest"]})
    );
}

#[test]
fn system_text_names_match_every_filtered_tool_set() {
    let guidance = include_str!("../src/codemode_system_text.md");
    for (tags, names, expected_text, expected_digest) in [
        (json!([]), json!([]), "", EMPTY_TEXT_DIGEST),
        (
            json!(["basal:codemode/v1"]),
            json!(["codemode"]),
            guidance,
            TEXT_DIGEST,
        ),
        (
            json!(["basal:flow.manage/v1"]),
            json!(["flow"]),
            "",
            EMPTY_TEXT_DIGEST,
        ),
        (
            json!(["basal:codemode/v1", "basal:flow.manage/v1"]),
            json!(["codemode", "flow"]),
            guidance,
            TEXT_DIGEST,
        ),
    ] {
        let mut request = arguments();
        request["params"]["capabilities"] = tags;
        let answer = tool::catalog(request).unwrap();
        assert_eq!(answer["system_text"]["tool_names"], names);
        assert_eq!(answer["system_text"]["text"], expected_text);
        assert_eq!(answer["system_text"]["item_digest"], expected_digest);
        assert_eq!(answer["system_text"]["preflight_digest"], expected_digest);
    }
}

#[test]
fn system_text_composition_and_digest_only_match_the_full_answer() {
    let mut request = arguments();
    let preflight = tool::catalog(request.clone()).unwrap();
    request["composition"] = json!({});
    let full = tool::catalog(request.clone()).unwrap();
    let composition_digest = "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a";
    assert_eq!(full["composition_digest"], composition_digest);
    assert_eq!(
        full["system_text"]["composition_digest"],
        composition_digest
    );
    assert_eq!(full["system_text"]["preflight_digest"], TEXT_DIGEST);
    assert_eq!(
        full["system_text"]["text"],
        preflight["system_text"]["text"]
    );
    request["digest_only"] = json!(true);
    assert_eq!(
        tool::catalog(request).unwrap(),
        json!({"generation":full["generation"],"catalog_digest":full["catalog_digest"]})
    );
    let without_text = tool::catalog(json!({"params":arguments()["params"]})).unwrap();
    assert_eq!(without_text["generation"], preflight["generation"]);
    assert_ne!(without_text["catalog_digest"], preflight["catalog_digest"]);
}

#[test]
fn system_text_refuses_named_presets_and_unknown_params() {
    for (item, field) in [
        (
            json!({"preset":"codemode","params":{}}),
            "system_text.preset",
        ),
        (
            json!({"params":{"unknown":true}}),
            "system_text.params.unknown",
        ),
    ] {
        let mut request = arguments();
        request["system_text"] = item;
        let error = tool::catalog(request).unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert_eq!(error.detail.unwrap()["field"], field);
    }
}
