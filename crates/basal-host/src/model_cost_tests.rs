use super::*;
use serde_json::{Value, json};

fn price(index: usize) -> Price {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../../test-vectors/routing-v1/consumers-and-placement.json"
    ))
    .unwrap();
    serde_json::from_value(vectors["answers"][index]["price"].clone()).unwrap()
}

fn usage() -> TokenUsage {
    serde_json::from_value(json!({
        "input_tokens":100, "output_tokens":20, "cached_input_tokens":50,
        "cache_write_tokens":30, "reasoning_tokens":19
    }))
    .unwrap()
}

#[test]
fn metered_cost_prices_disjoint_usage_and_does_not_add_reasoning() {
    // 100*3 + 20*15 + 50*0.3 + 30*3.75 = 727.5 microdollars.
    assert_eq!(charge(&price(0), Some(usage()), 50_000, false), 728);
}

#[test]
fn subscription_equivalent_rates_are_not_divided_by_multiplier_again() {
    let usage = TokenUsage {
        cache_write_tokens: Some(0),
        ..usage()
    };
    // The selected rates already include the 20x multiplier: 10 + 10 + 0.5.
    assert_eq!(charge(&price(1), Some(usage), 50_000, false), 21);
}

#[test]
fn complete_cache_write_split_uses_exact_lifetime_rates() {
    let usage = TokenUsage {
        cache_write_5m_tokens: Some(10),
        cache_write_1h_tokens: Some(20),
        ..usage()
    };
    // Fresh, output and cache-read cost 615; lifetime writes cost 37.5 + 120.
    assert_eq!(charge(&price(0), Some(usage), 50_000, false), 773);
}

#[test]
fn unmatched_cache_write_lifetime_never_borrows_plain_rate() {
    let mut price = price(0);
    price
        .rates_per_mtok
        .cache_write_ttl
        .as_mut()
        .unwrap()
        .remove("1h");
    let usage = TokenUsage {
        cache_write_5m_tokens: Some(10),
        cache_write_1h_tokens: Some(20),
        ..usage()
    };
    assert_eq!(charge(&price, Some(usage), 50_000, false), 653);
}

#[test]
fn absent_or_partial_cache_write_split_uses_the_plain_total() {
    let partial = TokenUsage {
        cache_write_5m_tokens: Some(10),
        ..usage()
    };
    assert_eq!(charge(&price(0), Some(partial), 50_000, false), 728);
    assert_eq!(charge(&price(0), Some(usage()), 50_000, false), 728);
}

#[test]
fn task_estimate_reserves_then_actual_usage_replaces_it() {
    let mut price = price(0);
    assert_eq!(reserve(&price, 100, 32).unwrap(), 15_000);
    assert_eq!(charge(&price, Some(usage()), 15_000, false), 728);
    price.estimated_task_usd = None;
    assert_eq!(reserve(&price, 100, 32).unwrap(), 780);
}

#[test]
fn missing_usage_keeps_liability_and_proven_unsent_releases_it() {
    assert_eq!(charge(&price(0), None, 15_000, false), 15_000);
    assert_eq!(
        charge(&price(0), Some(TokenUsage::default()), 15_000, false),
        15_000
    );
    assert_eq!(charge(&price(0), Some(usage()), 15_000, true), 0);
}
