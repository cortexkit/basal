//! Evidence predicates shared by the live suite and its offline tests. Missing
//! fields fail closed: a null value matching another null is not a measurement.

use serde_json::{Value, json};

/// Broca records `scope_epoch`, while core publishes the selector's `epoch`.
pub fn scope_matches(ownership: &Value, registered: &Value) -> bool {
    let scope = &ownership["scope"];
    scope["owner"] == json!({"kind":"reserved","module_id":"prefrontal-core"})
        && scope["owner"] == registered["owner"]
        && registered["ref"].as_str().is_some_and(|s| !s.is_empty())
        && registered["epoch"].as_u64().is_some_and(|n| n > 0)
        && scope["ref"] == registered["ref"]
        && scope["scope_epoch"] == registered["epoch"]
}

pub fn basal_first_principal(ownership: &Value) -> bool {
    ownership["first_principal"] == json!({"kind":"reserved","module_id":"basal"})
}

pub fn stamped_flow(ownership: &Value, flow_id: &str) -> bool {
    ownership["scope"]["flow_id"] == flow_id && !flow_id.is_empty()
}

/// A successful open or a different refusal does not prove carrier enforcement.
pub fn non_carrier_refused(reply: &Value) -> bool {
    reply["refused"]["code"] == "scope_not_carrier"
}

/// Read the actual saved send outcome, not a local pre-dispatch refusal.
pub fn unscoped_send_refused(snapshot: &Value) -> bool {
    snapshot["outcome"]["rejected"] == true
        && snapshot["outcome"]["value"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .is_some_and(|v| v["code"] == "flow_scope_required")
}

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
    let text = snapshot["params"].as_str()?;
    serde_json::from_str(text).ok()
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
    fn scope_evidence_rejects_mismatched_ref_epoch_owner_and_missing_fields() {
        let registered = json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"core-ref","epoch":7});
        let owner = json!({"scope":{"owner":registered["owner"],"ref":"core-ref","scope_epoch":7}});
        assert!(scope_matches(&owner, &registered));
        for (key, wrong) in [
            ("ref", json!("other-ref")),
            ("scope_epoch", json!(8)),
            ("owner", json!({"kind":"direct"})),
            ("ref", Value::Null),
            ("scope_epoch", Value::Null),
        ] {
            let mut bad = owner.clone();
            bad["scope"][key] = wrong;
            assert!(!scope_matches(&bad, &registered), "{key}");
        }
        assert!(!scope_matches(&Value::Null, &Value::Null));
    }

    #[test]
    fn automation_evidence_requires_attested_basal_and_this_flow() {
        let mut owner = json!({"first_principal":{"kind":"reserved","module_id":"basal"},"scope":{"flow_id":"flow-a"}});
        assert!(basal_first_principal(&owner));
        assert!(stamped_flow(&owner, "flow-a"));
        assert!(!stamped_flow(&owner, "flow-b"));
        owner["first_principal"] = json!({"kind":"direct"});
        assert!(!basal_first_principal(&owner));
        assert!(!basal_first_principal(&Value::Null));
        assert!(!stamped_flow(&Value::Null, "flow-a"));
    }

    #[test]
    fn carrier_evidence_requires_the_daemons_exact_non_carrier_refusal() {
        assert!(non_carrier_refused(
            &json!({"refused":{"code":"scope_not_carrier"}})
        ));
        for reply in [
            json!({"ok":{"opened":true}}),
            json!({"refused":{"code":"scope_ended"}}),
            Value::Null,
        ] {
            assert!(!non_carrier_refused(&reply));
        }
    }

    #[test]
    fn negative_send_evidence_requires_a_rejected_flow_scope_required_outcome() {
        let snapshot = |rejected, code| json!({"outcome":{"rejected":rejected,"value":json!({"code":code}).to_string()}});
        assert!(unscoped_send_refused(&snapshot(
            true,
            "flow_scope_required"
        )));
        assert!(!unscoped_send_refused(&snapshot(
            false,
            "flow_scope_required"
        )));
        assert!(!unscoped_send_refused(&snapshot(
            true,
            "scope_owner_mismatch"
        )));
        assert!(!unscoped_send_refused(
            &json!({"state":"active","broca_run_id":"b1"})
        ));
        assert!(!unscoped_send_refused(&Value::Null));
    }

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
        use basal_host::broca::{Route, StoredCall, wire::ModelParams};
        use basal_host::selector::ModelSelection;
        use basal_proto::JsonText;

        let send = json!({"tools":[],"tool_choice":{"type":"none"},"prompt":"fixture"});
        let params = JsonText::new(send.to_string()).unwrap();
        // StoredCall serializes the frozen send's UTF-8 bytes as a JSON string.
        // Use that serializer instead of guessing the persisted snapshot shape.
        let call = StoredCall {
            deferred: false,
            route: Route {
                flow_id: Some("flow".into()),
                project_root: "/rig".into(),
                harness: "basal".into(),
                session: "basal:fixture".into(),
            },
            basal_run_id: "run".into(),
            position: 0,
            send_id: "send".into(),
            params: params.as_str().as_bytes().to_vec(),
            envelope: "not a send".into(),
            labels: None,
            handle: None,
            broca_run_id: None,
            state: None,
            outcome: None,
            unknown: None,
            acknowledged: false,
            report_attempted: false,
            selection: ModelSelection {
                provider_id: "openai".into(),
                model_id: "gpt-6-luna".into(),
                variant: None,
                decision_id: "decision".into(),
                runner: ModelParams {
                    provider: "openai".into(),
                    model: "gpt-6-luna".into(),
                    variant: None,
                },
            },
        };
        let snapshot = serde_json::to_value(call).unwrap();
        assert_eq!(snapshot["params"].as_str(), Some(params.as_str()));
        assert_eq!(frozen_send(&snapshot), Some(send));
        for invalid in [
            json!({"params":"not json"}),
            json!({"params":[255]}),
            json!({"envelope":{"tools":[],"tool_choice":{"type":"none"}}}),
            Value::Null,
        ] {
            assert_eq!(frozen_send(&invalid), None);
        }
    }
}
