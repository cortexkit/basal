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
fn legacy_filtered_catalog_answer_bytes_are_unchanged() {
    let digests: Vec<_> = [
        json!([]),
        json!(["basal:codemode/v1"]),
        json!(["basal:flow.manage/v1"]),
        json!(["basal:codemode/v1", "basal:flow.manage/v1"]),
    ]
    .into_iter()
    .map(|tags| {
        let mut request = arguments();
        request["params"]["capabilities"] = tags;
        let answer = tool::catalog(request).unwrap();
        format!("{:x}", Sha256::digest(serde_json::to_vec(&answer).unwrap()))
    })
    .collect();
    // SHA-256 of the complete filtered responses. Even filtered-out tools
    // contribute to catalog generation, so a description change moves all four pins.
    assert_eq!(
        digests,
        [
            "6bda14400ee9d3de321eebabcb8e0c60ab934b4a20e12e93f690656b0cc8482b",
            "56c6d72ee8c23f4b88701a3b4a281af99dc33c3afaffb004a69c0acdeb535228",
            "caabecfef2a094f64b1e49f8a30213ade0c670550e3b2fe4e3bbe47ac1403458",
            "6d1b61210e7645cf164fd6e3a7b15ec3bde755e02ab19d77dd77cc5495a46f87",
        ]
    );
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
    assert_eq!(answer["presets"], json!(["head"]));
    assert!(answer["system_text"].get("composition_digest").is_none());
}

#[test]
fn catalog_tool_bytes_are_unchanged_by_system_text_support() {
    let with_text = tool::catalog(arguments()).unwrap();
    let without_text = tool::catalog(json!({"params":arguments()["params"]})).unwrap();
    assert!(without_text.get("system_text").is_none());
    assert_eq!(with_text["tools"], without_text["tools"]);
    // SHA-256 of the serialized tools array, so any change to a description,
    // schema or other tool field is deliberate. Heads register these bytes.
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&with_text["tools"]).unwrap())
        ),
        "9b40e4549f3b07bbb50362434b616030607a1759176987473c87ac695bd83ca2"
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

/// Naming the `head` preset for the system text alone leaves the catalog's
/// tools and text as the default's.
#[test]
fn head_system_text_on_the_default_catalog_matches_the_default() {
    let default = tool::catalog(arguments()).unwrap();
    let request =
        json!({"params":arguments()["params"],"system_text":{"preset":"head","params":{}}});
    let named = tool::catalog(request).unwrap();
    assert_eq!(named["tools"], default["tools"]);
    assert_eq!(named["system_text"], default["system_text"]);
    assert_eq!(named["generation"], default["generation"]);
}

#[test]
fn preset_discovery_and_selected_system_text_match_the_session() {
    let default = tool::catalog(json!({"params":{}})).unwrap();
    assert_eq!(default["presets"], json!(["head", "worker", "reader"]));
    let guidance = include_str!("../src/codemode_system_text.md");
    for (preset, names, text, digest) in [
        ("head", json!(["codemode"]), guidance, TEXT_DIGEST),
        ("worker", json!(["codemode"]), guidance, TEXT_DIGEST),
        ("reader", json!(["codemode"]), guidance, TEXT_DIGEST),
    ] {
        let answer = tool::catalog(json!({
            "preset":preset,"params":{},"system_text":{"preset":preset,"params":{}}
        }))
        .unwrap();
        let tools: Vec<_> = answer["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| &tool["name"])
            .collect();
        assert_eq!(tools, names.as_array().unwrap().iter().collect::<Vec<_>>());
        assert!(answer.get("presets").is_none(), "{preset}");
        assert_eq!(answer["system_text"]["tool_names"], names, "{preset}");
        assert_eq!(answer["system_text"]["text"], text, "{preset}");
        assert_eq!(answer["system_text"]["item_digest"], digest, "{preset}");
        assert_eq!(
            answer["system_text"]["preflight_digest"], digest,
            "{preset}"
        );
    }
}

#[test]
fn legacy_capability_filter_only_narrows_the_selected_preset() {
    let request = |preset, capabilities| json!({"preset":preset,"params":{"capabilities":capabilities},"system_text":{"params":{}}});
    let worker = tool::catalog(request("worker", json!(["basal:flow.manage/v1"]))).unwrap();
    assert_eq!(worker["tools"], json!([]));
    assert_eq!(worker["system_text"]["tool_names"], json!([]));
    assert_eq!(worker["system_text"]["text"], "");

    let head = tool::catalog(request(
        "head",
        json!(["basal:codemode/v1", "basal:flow.manage/v1"]),
    ))
    .unwrap();
    assert_eq!(head["tools"].as_array().unwrap().len(), 1);
    assert_eq!(head["tools"][0]["name"], "codemode");
    assert_eq!(head["system_text"]["tool_names"], json!(["codemode"]));
}

#[test]
fn catalog_refuses_unknown_presets_and_system_text_params() {
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
    let mut request = arguments();
    request["preset"] = json!("codemode");
    let error = tool::catalog(request).unwrap_err();
    assert_eq!(error.detail.unwrap()["field"], "preset");
}
