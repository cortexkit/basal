//! Evidence predicates shared by the live suite and its offline tests. Missing
//! fields fail closed: a null value matching another null is not a measurement.

use serde_json::{Value, json};

pub fn selected_luna(selection: &Value) -> bool {
    selection["providerID"] == "openai"
        && selection["modelID"] == "gpt-6-luna"
        && selection["runner"]["provider"] == "openai"
        && selection["runner"]["model"] == "gpt-6-luna"
        && selection["decisionID"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
}

pub fn frozen_send(snapshot: &Value) -> Option<Value> {
    let bytes: Vec<u8> = serde_json::from_value(snapshot["params"].clone()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn no_tools(send: &Value) -> bool {
    send["tools"] == json!([]) && send["tool_choice"] == json!({"type":"none"})
}

/// A completed run can spend its entire small output allowance on reasoning.
/// Empty visible text is still a result, but a missing transcript is not.
pub fn result_matches_transcript(result: &Value, text: Option<&str>) -> bool {
    text.is_some_and(|t| result["text"] == t)
}

/// Broca's canonical input is already fresh input. Missing capped measurements
/// stay null and retain the unmeasured part of the reservation; they are not
/// evidence of zero usage (OpenAI may omit cache-write usage entirely).
pub fn settled_usage(charge: &Value, usage: &Value) -> bool {
    let Some(reserved) = charge["reserved"].as_u64().filter(|n| *n > 0) else {
        return false;
    };
    let fields = ["input_tokens", "cache_write_tokens", "output_tokens"];
    let measured: u64 = fields.iter().filter_map(|k| usage[k].as_u64()).sum();
    let unreported = if fields.iter().any(|k| usage[k].is_null()) {
        reserved.saturating_sub(measured)
    } else {
        0
    };
    charge["state"] == "settled"
        && usage["input_tokens"].as_u64().is_some()
        && usage["output_tokens"].as_u64().is_some()
        && fields
            .iter()
            .chain(["cached_input_tokens"].iter())
            .all(|k| charge[k] == usage[k])
        && charge["unreported_tokens"] == unreported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_empty_text_is_valid_but_missing_transcript_is_not() {
        assert!(result_matches_transcript(&json!({"text":""}), Some("")));
        assert!(result_matches_transcript(
            &json!({"text":"hello"}),
            Some("hello")
        ));
        assert!(!result_matches_transcript(
            &json!({"text":"hello"}),
            Some("")
        ));
        assert!(!result_matches_transcript(&json!({"text":""}), None));
        assert!(!result_matches_transcript(&Value::Null, Some("")));
    }

    #[test]
    fn only_a_complete_luna_selection_passes() {
        let good = json!({"providerID":"openai","modelID":"gpt-6-luna","decisionID":"d1",
            "runner":{"provider":"openai","model":"gpt-6-luna"}});
        assert!(selected_luna(&good));
        for pointer in [
            "/providerID",
            "/modelID",
            "/decisionID",
            "/runner/provider",
            "/runner/model",
        ] {
            let mut bad = good.clone();
            *bad.pointer_mut(pointer).unwrap() = json!("");
            assert!(!selected_luna(&bad), "{pointer}");
        }
        assert!(!selected_luna(&Value::Null));
    }

    #[test]
    fn tools_evidence_requires_empty_tools_and_explicit_none() {
        assert!(no_tools(&json!({"tools":[],"tool_choice":{"type":"none"}})));
        assert!(!no_tools(
            &json!({"tools":[{"name":"shell"}],"tool_choice":{"type":"none"}})
        ));
        assert!(!no_tools(
            &json!({"tools":[],"tool_choice":{"type":"auto"}})
        ));
        assert!(!no_tools(&Value::Null));
    }

    #[test]
    fn usage_evidence_distinguishes_fresh_input_and_reservation() {
        let usage = json!({"input_tokens":7,"cached_input_tokens":5,"output_tokens":3,"cache_write_tokens":0});
        let charge = json!({"state":"settled","reserved":800,"input_tokens":7,
            "cached_input_tokens":5,"output_tokens":3,"cache_write_tokens":0,"unreported_tokens":0});
        assert!(settled_usage(&charge, &usage));
        for (key, wrong) in [
            ("input_tokens", json!(12)),
            ("output_tokens", json!(800)),
            ("unreported_tokens", json!(800)),
            ("state", json!("reserved")),
            ("reserved", json!(0)),
        ] {
            let mut bad = charge.clone();
            bad[key] = wrong;
            assert!(!settled_usage(&bad, &usage), "{key}");
        }
        assert!(!settled_usage(&Value::Null, &Value::Null));
        assert!(!settled_usage(&charge, &json!({"output_tokens":3})));
        let partial = json!({"input_tokens":7,"cached_input_tokens":5,"output_tokens":3});
        let mut retained = charge.clone();
        retained["cache_write_tokens"] = Value::Null;
        retained["unreported_tokens"] = json!(790);
        assert!(settled_usage(&retained, &partial));
        retained["unreported_tokens"] = json!(0);
        assert!(!settled_usage(&retained, &partial));
    }

    #[test]
    fn frozen_send_decodes_real_bytes_not_the_envelope() {
        let send = json!({"tools":[],"tool_choice":{"type":"none"}});
        let snapshot =
            json!({"params":serde_json::to_vec(&send).unwrap(), "envelope":"not a send"});
        assert_eq!(frozen_send(&snapshot), Some(send));
        assert_eq!(frozen_send(&json!({"params":[255]})), None);
    }
}
