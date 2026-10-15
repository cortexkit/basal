use super::*;
use std::sync::mpsc;

#[derive(Default)]
struct Wire {
    calls: Arc<Mutex<Vec<&'static str>>>,
    page: Mutex<Value>,
    fail_ack: AtomicBool,
}

impl Transport for Wire {
    fn catalog(&self) -> Result<Value, WireError> {
        panic!("unexpected catalog")
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> Result<Value, WireError> {
        panic!("unexpected tool")
    }
    fn management(&self, _: &str, op: &str, _: Value) -> Result<Value, WireError> {
        match op {
            "elicitation.answers" => {
                self.calls.lock().unwrap().push("answers");
                Ok(self.page.lock().unwrap().clone())
            }
            "elicitation.ack" => {
                self.calls.lock().unwrap().push("ack");
                Ok(json!({"ok": !self.fail_ack.swap(false, Ordering::AcqRel)}))
            }
            "elicitation.request" => Ok(json!({"elicitation_id":"raised"})),
            _ => panic!("unexpected management call {op}"),
        }
    }
}

struct Sink {
    open: AtomicBool,
    calls: Arc<Mutex<Vec<&'static str>>>,
    inventory: mpsc::Sender<()>,
}

impl DecisionSink for Sink {
    fn has_open_cards(&self) -> Result<bool, crate::SinkError> {
        if !self.open.load(Ordering::Acquire) {
            let _ = self.inventory.send(());
        }
        Ok(self.open.load(Ordering::Acquire))
    }
    fn decide(&self, _: &crate::DecisionEvent) -> Result<(), crate::SinkError> {
        panic!("install decisions do not use the answer feed")
    }
    fn answer(&self, _: &DecisionAnswer) -> Result<(), crate::SinkError> {
        self.calls.lock().unwrap().push("apply");
        self.open.store(false, Ordering::Release);
        Ok(())
    }
}

fn setup(open: bool, answered: bool) -> (Arc<Wire>, Arc<Sink>, mpsc::Receiver<()>) {
    let wire = Arc::new(Wire::default());
    *wire.page.lock().unwrap() = if answered {
        json!({"cursor":1,"records":[{"elicitation_id":"old", "state":"answered", "answered_choice_id":"reenable", "flow_decision":{"decision":"reenable","flow_id":"flow","version":1}}]})
    } else {
        json!({"cursor":0,"records":[]})
    };
    let (tx, rx) = mpsc::channel();
    let sink = Arc::new(Sink {
        open: AtomicBool::new(open),
        calls: wire.calls.clone(),
        inventory: tx,
    });
    (wire, sink, rx)
}

#[test]
fn idle_consent_makes_zero_rpcs() {
    let (wire, sink, _) = setup(false, false);
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink);
    // Five chances to poll, made directly, without sleeping or asserting elapsed time.
    for _ in 0..5 {
        consent.poll_once().unwrap();
    }
    assert_eq!(wire.calls.lock().unwrap().len(), 0);
}

#[test]
fn startup_reconciles_closed_unacked_cards_then_stays_idle() {
    let (wire, sink, inventory) = setup(false, true);
    let consent = CoreConsent::new(wire.clone()).with_polling();
    consent.attach(sink);
    // Inventory is consulted only after the startup page is applied and acked.
    inventory.recv_timeout(Duration::from_secs(60)).unwrap();
    assert_eq!(*wire.calls.lock().unwrap(), ["answers", "apply", "ack"]);
    for _ in 0..5 {
        consent.poll_once().unwrap();
    }
    assert_eq!(*wire.calls.lock().unwrap(), ["answers", "apply", "ack"]);
}

#[test]
fn failed_ack_is_retried_after_the_last_card_closes() {
    let (wire, sink, _) = setup(true, true);
    wire.fail_ack.store(true, Ordering::Release);
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink);
    assert!(consent.poll_once().is_err());
    consent.poll_once().unwrap();
    consent.poll_once().unwrap();
    assert_eq!(
        *wire.calls.lock().unwrap(),
        ["answers", "apply", "ack", "answers", "apply", "ack"]
    );
}

#[test]
fn open_cards_back_off_to_five_seconds_and_a_raise_resets_fast_polling() {
    let mut delay = POLL_MIN;
    let mut delays = vec![delay.as_millis()];
    for _ in 0..6 {
        delay = next_poll_delay(delay, false);
        delays.push(delay.as_millis());
    }
    assert_eq!(delays, [250, 500, 1000, 2000, 4000, 5000, 5000]);
    assert_eq!(next_poll_delay(delay, true), POLL_MIN);
    let (wire, sink, _) = setup(true, false);
    let consent = CoreConsent::new(wire.clone());
    consent.attach(sink);
    consent.poll_once().unwrap();
    assert_eq!(*wire.calls.lock().unwrap(), ["answers"]);
    let seen = consent.state.wake.lock().unwrap().0;
    consent
        .raise_decision(&DecisionCard {
            dedup_key: None,
            flow_id: "flow".into(),
            version: 1,
            context: DecisionContext::GrantLost {
                provider: "plexus".into(),
                grant: "g".into(),
                grant_label: "restore".into(),
                refused_at_ms: 0,
            },
            title: "Decide".into(),
            prompt: "Check grant".into(),
            args_digest: None,
            expires_in_ms: 60_000,
            options: vec![
                crate::DecisionOption {
                    id: "check_now".into(),
                    label: "Check".into(),
                    decline: false,
                },
                crate::DecisionOption {
                    id: "keep_disabled".into(),
                    label: "Keep disabled".into(),
                    decline: true,
                },
            ],
        })
        .unwrap();
    assert_ne!(consent.state.wake.lock().unwrap().0, seen);
}

#[test]
fn grant_lost_request_has_closed_body_and_choice_ids() {
    let mut card = crate::DecisionCard {
        dedup_key: None,
        flow_id: "flow".into(),
        version: 9,
        context: crate::DecisionContext::GrantLost {
            provider: "plexus".into(),
            grant: "opaque".into(),
            grant_label: "restore grant".into(),
            refused_at_ms: 0,
        },
        title: "Decide".into(),
        prompt: "Check the existing grant".into(),
        args_digest: None,
        expires_in_ms: 60_000,
        options: vec![
            crate::DecisionOption {
                id: "check_now".into(),
                label: "Check now".into(),
                decline: false,
            },
            crate::DecisionOption {
                id: "keep_disabled".into(),
                label: "Keep disabled".into(),
                decline: true,
            },
        ],
    };
    assert_eq!(
        crate::core_consent::decision_request(&card).unwrap()["flow_decision"],
        serde_json::json!({"decision":"grant_lost","flow_id":"flow","provider":"plexus","grant":"opaque","grant_label":"restore grant","refused_at_ms":0})
    );
    card.options[0].id = "offer".into();
    assert!(crate::core_consent::decision_request(&card).is_err());
}

fn flow_decision_card(context: crate::DecisionContext) -> crate::DecisionCard {
    let options = match &context {
        crate::DecisionContext::Reconcile { .. } => vec![
            crate::DecisionOption {
                id: "not_applied".into(),
                label: "Send the call again".into(),
                decline: false,
            },
            crate::DecisionOption {
                id: "applied".into(),
                label: "Continue as applied".into(),
                decline: false,
            },
            crate::DecisionOption {
                id: "cancel".into(),
                label: "Cancel the run".into(),
                decline: false,
            },
            crate::DecisionOption {
                id: "leave".into(),
                label: "Leave it unresolved".into(),
                decline: true,
            },
        ],
        crate::DecisionContext::Reenable { .. } => vec![
            crate::DecisionOption {
                id: "reenable".into(),
                label: "Re-enable the flow".into(),
                decline: false,
            },
            crate::DecisionOption {
                id: "keep".into(),
                label: "Keep disabled".into(),
                decline: true,
            },
        ],
        crate::DecisionContext::GrantLost { .. } => vec![
            crate::DecisionOption {
                id: "check_now".into(),
                label: "Check now".into(),
                decline: false,
            },
            crate::DecisionOption {
                id: "keep_disabled".into(),
                label: "Keep disabled".into(),
                decline: true,
            },
        ],
    };
    crate::DecisionCard {
        dedup_key: Some("flow_decision:fixture".into()),
        flow_id: "flow-x".into(),
        version: 3,
        context,
        title: "Decision".into(),
        prompt: "Decide".into(),
        args_digest: None,
        expires_in_ms: 60_000,
        options,
    }
}

#[test]
fn reconcile_facts_are_plain_text_and_omit_an_absent_step() {
    let make = |step, unknown_reason| {
        flow_decision_card(crate::DecisionContext::Reconcile {
            run_id: "run-x".into(),
            run_admitted_at_ms: 1_777_593_600_000,
            step,
            call_key: "call-x".into(),
            op: "prefrontal-core.sink.digest".into(),
            attempts: 2,
            unknown_reason,
        })
    };
    let absent_step =
        crate::core_consent::decision_request(&make(None, crate::UnknownReason::ConnectionLost))
            .unwrap();
    assert_eq!(
        absent_step["facts"],
        json!([
            {"label":"Run","value":"2026-05-01 00:00 UTC"},
            {"label":"Call in doubt","value":"prefrontal-core.sink.digest, attempt 2"},
            {"label":"Why unknown","value":"The connection closed before a reply came."}
        ])
    );
    let with_step = crate::core_consent::decision_request(&make(
        Some("writer/send".into()),
        crate::UnknownReason::ConnectionLost,
    ))
    .unwrap();
    assert_eq!(
        with_step["facts"][1],
        json!({"label":"Step","value":"writer/send"})
    );

    for (reason, expected) in [
        (
            crate::UnknownReason::ReplyTimeout,
            "No reply came before the call's deadline.",
        ),
        (
            crate::UnknownReason::ReplyUnreadable,
            "The reply could not be read.",
        ),
        (
            crate::UnknownReason::BasalRestarted,
            "Basal restarted before the reply was saved.",
        ),
        (
            crate::UnknownReason::RetriesExhausted,
            "The retries ended without a clear answer.",
        ),
        (
            crate::UnknownReason::ProviderLostRun,
            "The provider lost track of the run.",
        ),
    ] {
        let request = crate::core_consent::decision_request(&make(None, reason)).unwrap();
        assert_eq!(request["facts"][2]["value"], expected);
    }
}

#[test]
fn reenable_facts_explain_run_and_dispatch_limits_with_plural_counts() {
    for (reason, limit, windows, why) in [
        (
            crate::DisabledReason::DispatchLimitSaturated,
            1,
            1,
            "It hit its limit of 1 call per 60-second window in 1 window in a row.",
        ),
        (
            crate::DisabledReason::RunLimitSaturated,
            3,
            2,
            "It hit its limit of 3 runs per 60-second window in 2 windows in a row.",
        ),
    ] {
        let card = flow_decision_card(crate::DecisionContext::Reenable {
            disabled_at_ms: 1_777_593_600_000,
            disabled_reason: reason,
            limit,
            window_ms: 60_000,
            saturated_windows: windows,
        });
        let request = crate::core_consent::decision_request(&card).unwrap();
        assert_eq!(
            request["facts"],
            json!([
                {"label":"Disabled at","value":"2026-05-01 00:00 UTC"},
                {"label":"Why disabled","value":why}
            ])
        );
    }
}

#[test]
fn grant_lost_facts_show_the_label_not_the_opaque_grant() {
    let card = flow_decision_card(crate::DecisionContext::GrantLost {
        provider: "plexus".into(),
        grant: "opaque:do-not-display-this".into(),
        grant_label: "Read GitHub for cortexkit/basal".into(),
        refused_at_ms: 1_777_593_600_000,
    });
    let request = crate::core_consent::decision_request(&card).unwrap();
    assert_eq!(
        request["facts"],
        json!([
            {"label":"Provider","value":"plexus"},
            {"label":"Grant","value":"Read GitHub for cortexkit/basal"},
            {"label":"Refused at","value":"2026-05-01 00:00 UTC"}
        ])
    );
    assert!(
        !request["facts"]
            .to_string()
            .contains("opaque:do-not-display-this")
    );
}

#[test]
fn phone_fact_values_are_truncated_on_utf8_boundaries() {
    let grant_label = "界".repeat(100);
    let card = flow_decision_card(crate::DecisionContext::GrantLost {
        provider: "plexus".into(),
        grant: "opaque".into(),
        grant_label,
        refused_at_ms: 1_777_593_600_000,
    });
    let request = crate::core_consent::decision_request(&card).unwrap();
    let grant = request["facts"][1]["value"].as_str().unwrap();
    assert!(grant.len() <= 200);
    assert!(grant.ends_with('…'));
    assert!(grant.is_char_boundary(grant.len()));
}

#[test]
fn consent_request_envelope_over_the_requester_limit_is_refused() {
    let card = flow_decision_card(crate::DecisionContext::GrantLost {
        provider: "plexus".into(),
        grant: "opaque".into(),
        grant_label: "g".repeat(1_310_720),
        refused_at_ms: 1_777_593_600_000,
    });
    assert!(crate::core_consent::decision_request(&card).is_err());
}
