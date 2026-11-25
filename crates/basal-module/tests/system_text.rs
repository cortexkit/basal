use basal_module::{manifest, tool};
use cortexkit_role_tool_provider::SYSTEM_TEXT_DEFAULT;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TEXT_DIGEST: &str = "ac2a3af84c4039603f45e90c8ae1b28fd8c9a4b40e9b6cc3cdef316bf82dc5d0";

#[test]
fn catalog_answers_without_a_filter_keep_their_exact_bytes() {
    // Pin SHA-256 of complete replies to catch wire changes beyond tool definitions and text.
    let default_digests = [
        "ae0a80096497467169d5a6d9ddcdfea43dea1736ffdf7067f945623a45f41adb",
        "a69dd73166692729fb0f054600687f7f7f71990f8c65e2bc463b160f59780bf9",
    ];
    let preset_digests = [
        "1a350cbc807bf4918faa29008531a6df94796a5fcab71f1054c43d7c42410c21",
        "7c4a813946c609d9c098ab276f1c2a75ab8215ea303effb2e19bf672b582c81e",
    ];
    for preset in [None, Some("head"), Some("worker"), Some("reader")] {
        let digests = if preset.is_some() {
            preset_digests
        } else {
            default_digests
        };
        for (with_text, expected) in [false, true].into_iter().zip(digests) {
            let mut request = json!({"params":{}});
            if let Some(preset) = preset {
                request["preset"] = json!(preset);
            }
            if with_text {
                request["system_text"] = json!({"params":{}});
                if let Some(preset) = preset {
                    request["system_text"]["preset"] = json!(preset);
                }
            }
            let answer = tool::catalog(request).unwrap();
            let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&answer).unwrap()));
            assert_eq!(digest, expected, "{preset:?}, with text {with_text}");
        }
    }
}

fn arguments() -> Value {
    json!({
        "params":{},
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
    assert_eq!(answer["presets"], json!(["head", "worker", "reader"]));
    assert_eq!(
        answer["system_text"]["tool_names"],
        json!(["codemode", "flow"])
    );
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
