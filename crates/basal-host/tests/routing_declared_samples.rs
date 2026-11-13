//! Basal's routing adapters against real-handler captures, preserved byte-for-byte.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use basal_host::broca::{
    BrocaHost,
    fake::{FakeBroca, MemoryStore},
};
use basal_host::selector::{
    ModelSelection, ModelSelector, RoutingSelector, SelectionError, SelectionRequest,
    decode_selection, fake_describe, select_params,
};
use basal_host::transport::{Transport, WireError, map_error};
use basal_host::{CallRequest, TokenUsage, model_cost};
use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subc_client_rs::consumer::CallError;
use subc_protocol::ErrorBody;

const SUMS: &str = include_str!("../../../test-vectors/route-select-declared-v1/SHA256SUMS");
const SUMS_SHA256: &str = "7164bc6bd38a815c02267c0dedd14069e56e02aa607fc32fd3303c4df544dbc5";

fn directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-vectors/route-select-declared-v1")
}

fn sample(stem: &str, suffix: &str) -> Value {
    let path = directory().join(format!("{stem}.{suffix}.json"));
    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap()
}

fn reply_names() -> BTreeSet<String> {
    SUMS.lines()
        .filter_map(|line| {
            let (_, name) = line.split_once("  ").unwrap();
            name.strip_suffix(".reply.json").map(str::to_owned)
        })
        .collect()
}

fn request() -> SelectionRequest {
    SelectionRequest {
        input_tokens: 100,
        max_output_tokens: 32,
        caller_class: "flow".into(),
        iq: 50,
        eq: 0,
        flow_id: "flow-a".into(),
        run_id: "run-a".into(),
        send_id: "send-1".into(),
    }
}

struct CapturedRouting {
    reply: Value,
}

impl Transport for CapturedRouting {
    fn catalog(&self) -> Result<Value, WireError> {
        Ok(json!({"modules":[{"module_id":"striatum","capabilities":{"provides":["routing/v1"]}}]}))
    }

    fn management(&self, module: &str, op: &str, _: Value) -> Result<Value, WireError> {
        assert_eq!(module, "striatum");
        match op {
            "role.describe" => Ok(fake_describe(false)),
            "route.select" => {
                if let Some(result) = self.reply.get("result") {
                    Ok(result.clone())
                } else {
                    let error: ErrorBody = serde_json::from_value(self.reply.clone()).unwrap();
                    Err(map_error(CallError::Module(error)))
                }
            }
            other => panic!("unexpected routing operation {other}"),
        }
    }

    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("routing samples use management operations")
    }
}

fn select_then_send(stem: &str) -> (Result<ModelSelection, SelectionError>, usize) {
    let selector = Arc::new(RoutingSelector::new(Arc::new(CapturedRouting {
        reply: sample(stem, "reply"),
    })));
    let broca = Arc::new(FakeBroca::default());
    let host = BrocaHost::new(
        broca.clone(),
        Arc::new(MemoryStore::default()),
        "/project".into(),
        "basal".into(),
        selector.clone(),
    );
    let selection_request = request();
    let selection = selector.select(&selection_request);
    if let Ok(selection) = &selection {
        // Exercise the send if admission accidentally succeeds, so the zero-send
        // assertion below cannot pass merely because the fixture never sends.
        let request = CallRequest {
            flow_id: selection_request.flow_id,
            flow_version: None,
            run_id: selection_request.run_id,
            position: 0,
            kind: CallKind::Primitive(Primitive::Llm),
            args: JsonText::new(json!({
                "send_id":"send-1", "work_class":"flow:flow-a", "session":"basal:flow-flow-a:run-a:0",
                "op":"llm", "max_output":32, "request":{"prompt":"hello"}, "selection":selection
            }).to_string()).unwrap(),
            idempotency_key: selection_request.send_id,
            attempt: 1,
        };
        host.dispatch_model(&request).unwrap();
    }
    (selection, broca.sends().len())
}

#[test]
fn declared_samples_manifest_and_every_file_match_pinned_hashes() {
    let manifest = fs::read(directory().join("SHA256SUMS")).unwrap();
    assert_eq!(format!("{:x}", Sha256::digest(&manifest)), SUMS_SHA256);
    assert_eq!(manifest, SUMS.as_bytes());
    let mut covered = BTreeSet::from(["SHA256SUMS".to_owned()]);
    for line in SUMS.lines() {
        let (expected, name) = line.split_once("  ").unwrap();
        assert!(!name.contains(['/', '\\']));
        assert!(
            covered.insert(name.to_owned()),
            "duplicate manifest member {name}"
        );
        let bytes = fs::read(directory().join(name)).unwrap();
        assert_eq!(format!("{:x}", Sha256::digest(bytes)), expected, "{name}");
    }
    let actual: BTreeSet<_> = fs::read_dir(directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(covered, actual);
    assert_eq!(covered.len(), 22);
    assert_eq!(reply_names().len(), 10);
}

fn expected_rates(stem: &str) -> Value {
    match stem {
        "price-subscription-5m-1h" => json!({
            "input":0.18181818181818182, "output":0.9090909090909091,
            "cache_read":0.045454545454545456, "cache_write":0.22727272727272727,
            "cache_write_ttl":{"5m":0.22727272727272727,"1h":0.36363636363636365}
        }),
        "price-subscription-30m-12h" => json!({
            "input":1.0, "output":5.0, "cache_read":0.5, "cache_write":2.0,
            "cache_write_ttl":{"30m":2.5,"12h":4.0}
        }),
        _ => json!({
            "input":4, "output":20, "cache_read":1, "cache_write":5,
            "cache_write_ttl":{"5m":5,"1h":8}
        }),
    }
}

#[test]
fn declared_samples_priced_successes_decode_exact_prices_and_harnesses() {
    let mut priced = 0;
    let mut unpriced = 0;
    for stem in reply_names() {
        let reply = sample(&stem, "reply");
        let Some(answer) = reply.get("result") else {
            continue;
        };
        let decoded = decode_selection(answer.clone());
        if stem == "price-unpriced" {
            assert!(
                matches!(decoded, Err(SelectionError::Refused {code,..}) if code == "price_unknown")
            );
            unpriced += 1;
            continue;
        }
        let selected = decoded.unwrap_or_else(|error| panic!("{stem}: {error}"));
        let price = selected.price.as_ref().unwrap();
        let subscription = stem.starts_with("price-subscription-");
        assert_eq!(
            serde_json::to_value(&price.basis).unwrap(),
            json!(if subscription {
                "subscription_equivalent"
            } else {
                "metered"
            }),
            "{stem}"
        );
        assert_eq!(
            serde_json::to_value(&price.multiplier).unwrap(),
            if subscription { json!(22) } else { Value::Null },
            "{stem}"
        );
        assert_eq!(
            serde_json::to_value(&price.rates_per_mtok).unwrap(),
            expected_rates(&stem),
            "{stem}"
        );
        assert_eq!(
            serde_json::to_value(price).unwrap(),
            answer["price"],
            "{stem}"
        );
        let lifetimes: BTreeSet<_> = price
            .rates_per_mtok
            .cache_write_ttl
            .as_ref()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            lifetimes,
            if stem == "price-subscription-30m-12h" {
                BTreeSet::from(["30m", "12h"])
            } else {
                BTreeSet::from(["5m", "1h"])
            },
            "{stem}"
        );
        assert_eq!(
            selected.harness.as_deref(),
            answer["runner"]["harness"].as_str(),
            "{stem}"
        );
        if stem == "broca-harness" {
            assert_eq!(selected.harness.as_deref(), Some("broca"));
        }
        priced += 1;
    }
    assert_eq!((priced, unpriced), (7, 1));
}

#[test]
fn declared_samples_select_refuses_priced_non_broca_before_any_send() {
    let (selected, sends) = select_then_send("price-metered");
    assert_eq!(sends, 0);
    assert!(
        matches!(selected, Err(SelectionError::Refused {code,..}) if code == "route_runner_missing")
    );
}

#[test]
fn declared_samples_unpriced_non_broca_refuses_price_unknown_before_send() {
    // This answer is also non-Broca. Price validation deliberately runs before
    // runner admission, so the independent no-price refusal takes precedence.
    let (selected, sends) = select_then_send("price-unpriced");
    assert_eq!(sends, 0);
    assert!(matches!(selected, Err(SelectionError::Refused {code,..}) if code == "price_unknown"));
}

#[test]
fn declared_samples_error_frames_keep_routings_typed_refusal_codes() {
    for (stem, expected) in [
        ("caller-runner-unresolved", "no_route"),
        ("consumer-not-owned", "consumer_not_owned"),
    ] {
        let (selected, sends) = select_then_send(stem);
        assert_eq!(sends, 0);
        let Err(SelectionError::Refused { code, detail }) = selected else {
            panic!("{stem}: {selected:?}")
        };
        assert_eq!(code, expected);
        assert_eq!(detail, sample(stem, "reply")["message"].as_str().unwrap());
    }
}

fn usage_with_lifetimes() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(110),
        output_tokens: Some(22),
        cached_input_tokens: Some(44),
        cache_write_tokens: Some(33),
        cache_write_5m_tokens: Some(11),
        cache_write_1h_tokens: Some(22),
    }
}

#[test]
fn declared_samples_subscription_settlement_split_and_plain_write_rates() {
    let selected =
        decode_selection(sample("price-subscription-5m-1h", "reply")["result"].clone()).unwrap();
    let price = selected.price.unwrap();
    // In microdollars, (110*4 + 22*20 + 44*1 + 11*5 + 22*8)/22
    // = 52.5; round up once to 53. Routing already applied the 22x multiplier.
    assert_eq!(
        model_cost::charge(&price, Some(usage_with_lifetimes()), 10_000, false),
        53
    );
    let plain = TokenUsage {
        cache_write_5m_tokens: None,
        cache_write_1h_tokens: None,
        ..usage_with_lifetimes()
    };
    // Without a split: (110*4 + 22*20 + 44*1 + 33*5)/22 = 49.5 -> 50.
    assert_eq!(model_cost::charge(&price, Some(plain), 10_000, false), 50);
}

#[test]
fn declared_samples_long_lifetimes_leave_unmatched_short_buckets_unpriced() {
    let selected =
        decode_selection(sample("price-subscription-30m-12h", "reply")["result"].clone()).unwrap();
    let price = selected.price.unwrap();
    // The quote has only 30m/12h rates, not 5m/1h: 110*1 + 22*5 + 44*0.5
    // = 242 microdollars. Do not add 33*2 from the plain cache-write rate.
    assert_eq!(
        model_cost::charge(&price, Some(usage_with_lifetimes()), 10_000, false),
        242
    );
}

#[test]
fn declared_samples_basal_request_member_names_limits_and_identity() {
    let captured = sample("basal-flow", "request");
    assert_eq!(captured["method"], "route.select");
    let params = select_params(&request());
    assert_eq!(params["consumer"], captured["params"]["consumer"]);
    for field in ["iq", "tools"] {
        assert_eq!(
            params["requirements"][field],
            captured["params"]["requirements"][field]
        );
    }
    assert_eq!(
        params,
        json!({
            "consumer":"basal.flow", "requirements":{"iq":50,"eq":0,"tools":"none","max_output_tokens":32},
            "runners":["broca"], "correlation_id":"send-1", "task_ref":"flow:flow-a:run-a"
        })
    );
}
