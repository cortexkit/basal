//! Dollar-equivalent accounting from routing's selected rates and Broca's usage.

use cortexkit_role_routing::price::Price;
use serde_json::Number;

use crate::{TokenUsage, selector::SelectionError};

#[cfg(test)]
#[path = "model_cost_tests.rs"]
mod tests;

pub fn price_unknown() -> SelectionError {
    SelectionError::Refused {
        code: "price_unknown".into(),
        detail: "routing did not price the selected model".into(),
    }
}

fn micros(value: f64) -> Result<u64, SelectionError> {
    if !value.is_finite() || value < 0.0 || value >= u64::MAX as f64 {
        return Err(SelectionError::Refused {
            code: "price_unknown".into(),
            detail: "the model price cannot be represented in microdollars".into(),
        });
    }
    Ok(value.ceil() as u64)
}

fn rate(number: &Number) -> f64 {
    number.as_f64().expect("validated routing rate")
}

pub fn reserve(price: &Price, input: u64, output: u64) -> Result<u64, SelectionError> {
    if price.check().is_err() {
        return Err(price_unknown());
    }
    micros(if let Some(estimate) = &price.estimated_task_usd {
        rate(estimate) * 1_000_000.0
    } else {
        input as f64 * rate(&price.rates_per_mtok.input)
            + output as f64 * rate(&price.rates_per_mtok.output)
    })
}

/// One token at a rate per million USD tokens costs that rate in microdollars.
/// Subscription rates are already divided by their multiplier by routing.
pub fn charge(price: &Price, usage: Option<TokenUsage>, reserved: u64, no_effect: bool) -> u64 {
    if no_effect {
        return 0;
    }
    let Some(usage) = usage else {
        return reserved;
    };
    let rates = &price.rates_per_mtok;
    let bucket = |tokens: Option<u64>, price: Option<&Number>| {
        tokens.unwrap_or(0) as f64 * price.map_or(0.0, rate)
    };
    let writes = match (usage.cache_write_5m_tokens, usage.cache_write_1h_tokens) {
        (Some(short), Some(long)) => {
            bucket(Some(short), rates.cache_write_rate(Some("5m")))
                + bucket(Some(long), rates.cache_write_rate(Some("1h")))
        }
        _ => bucket(usage.cache_write_tokens, rates.cache_write_rate(None)),
    };
    let measured = bucket(usage.input_tokens, Some(&rates.input))
        + bucket(usage.output_tokens, Some(&rates.output))
        + bucket(usage.cached_input_tokens, rates.cache_read.as_ref())
        + writes;
    let measured = micros(measured).unwrap_or(u64::MAX);
    // An absent measurement is not free usage. Keep the reservation's liability
    // just as token accounting does when a capped usage component is missing.
    if usage.input_tokens.is_none()
        || usage.output_tokens.is_none()
        || usage.cache_write_tokens.is_none()
    {
        measured.max(reserved)
    } else {
        measured
    }
}

/// Old frozen selections have no price and retain their original accounting.
pub fn recorded_charge(
    request: &str,
    usage: Option<TokenUsage>,
    reserved: u64,
    no_effect: bool,
) -> Option<u64> {
    let envelope: serde_json::Value = serde_json::from_str(request).ok()?;
    let price: Price =
        serde_json::from_value(envelope.get("selection")?.get("price")?.clone()).ok()?;
    price.check().ok()?;
    Some(charge(&price, usage, reserved, no_effect))
}
