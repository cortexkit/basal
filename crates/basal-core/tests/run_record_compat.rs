use basal_core::run_log::LogRecord;
use basal_core::run_records::{Body, Deferred, Record};

#[test]
fn priced_selection_keeps_the_placed_intent_layout_and_json_reader_compatible() {
    use basal_core::StoredClass;
    use basal_core::run_records::{CallIdentity, Intent, Reservation};
    use basal_proto::{CallKind, HostCall, JsonText, Primitive};
    use serde::Deserialize;

    // Preserve the pre-pricing Intent layout for rollback readers. Its JSON
    // field is a string, not a postcard struct whose layout grows with selection.
    #[derive(Deserialize)]
    struct PlacedIntent {
        call: CallIdentity,
        idempotency_key: Option<String>,
        frozen_request: String,
        attempts: u64,
        dispatch_count_window: Option<(i64, u64)>,
        audit_at: i64,
        reservation: Option<(u64, u64, u64, i64)>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PlacedSelection {
        #[serde(rename = "providerID")]
        provider_id: String,
        #[serde(rename = "modelID")]
        model_id: String,
        variant: Option<String>,
        #[serde(rename = "decisionID")]
        decision_id: String,
        runner: basal_host::broca::wire::ModelParams,
    }
    let frozen = JsonText::new(r#"{"selection":{"providerID":"registry","modelID":"chosen","decisionID":"decision","runner":{"provider":"runner","model":"wire-model"},"harness":"basal","price":{"basis":"metered","rates_per_mtok":{"input":3,"output":15},"estimated_task_usd":0.015}}}"#).unwrap();
    let identity = CallIdentity::new(
        HostCall {
            position: 7,
            kind: CallKind::Primitive(Primitive::Llm),
            args: JsonText::new(r#"{"prompt":"hello"}"#).unwrap(),
        },
        StoredClass::Mutation,
    );
    let record = Record {
        run_id: "priced-run".into(),
        generation: 3,
        body: Body::Intent(Intent {
            call: identity.clone(),
            idempotency_key: Some("send-7".into()),
            frozen_request: frozen.clone(),
            attempts: 1,
            dispatch_count_window: None,
            audit_at: 17,
            reservation: Some(Reservation {
                input_tokens: 100,
                output_tokens: 32,
                cost_micros: 15_000,
                window_start: 0,
            }),
        }),
    };
    let bytes = postcard::to_allocvec(&record).unwrap();
    let (run, generation, ordinal, placed): (String, u64, u32, PlacedIntent) =
        postcard::from_bytes(&bytes).unwrap();
    assert_eq!((run.as_str(), generation, ordinal), ("priced-run", 3, 5));
    assert_eq!(placed.call, identity);
    assert_eq!(placed.idempotency_key.as_deref(), Some("send-7"));
    assert_eq!(placed.attempts, 1);
    assert_eq!(placed.dispatch_count_window, None);
    assert_eq!(placed.audit_at, 17);
    assert_eq!(placed.reservation, Some((100, 32, 15_000, 0)));
    assert_eq!(placed.frozen_request, frozen.as_str());
    let envelope: serde_json::Value = serde_json::from_str(&placed.frozen_request).unwrap();
    let selection: PlacedSelection = serde_json::from_value(envelope["selection"].clone()).unwrap();
    assert_eq!(selection.provider_id, "registry");
    assert_eq!(selection.model_id, "chosen");
    assert_eq!(selection.variant, None);
    assert_eq!(selection.decision_id, "decision");
    assert_eq!(selection.runner.provider, "runner");
    assert_eq!(selection.runner.model, "wire-model");
    assert_eq!(Record::decode(8, &bytes).unwrap(), record);
}

#[test]
fn pre_refund_deferral_record_decodes_with_refund_enabled() {
    // Body::Deferred had enum ordinal 11 and ended with a bool. This tuple
    // encodes that pre-change field order, independently of today's Record.
    let old = postcard::to_allocvec(&(
        "run-old",
        2u64,
        11u32,
        7u64,
        19i64,
        "old readiness refusal",
        17i64,
        true,
    ))
    .unwrap();
    let record = Record::decode(14, &old).unwrap();
    let Body::Deferred(deferred) = &record.body else {
        panic!("wrong record kind")
    };
    assert_eq!(record.run_id, "run-old");
    assert_eq!(record.generation, 2);
    assert_eq!(deferred.position, 7);
    assert_eq!(deferred.retry_not_before, 19);
    assert!(deferred.proven_unsent);
    assert!(deferred.refund_dispatch);
    assert_eq!(deferred.attempt, None);
    assert_eq!(
        postcard::to_allocvec(&record).unwrap(),
        old,
        "old flag bytes keep their exact meaning"
    );
}

#[test]
fn transport_budget_flags_and_attempt_ordinal_round_trip_without_becoming_refunds() {
    for proven_unsent in [false, true] {
        let record = Record {
            run_id: "run-new".into(),
            generation: 1,
            body: Body::Deferred(Deferred {
                position: 0,
                retry_not_before: 20,
                cause: "transport outcome".into(),
                audit_at: 10,
                proven_unsent,
                refund_dispatch: false,
                attempt: Some(3),
            }),
        };
        let bytes = postcard::to_allocvec(&record).unwrap();
        assert_eq!(Record::decode(14, &bytes).unwrap(), record);
    }
    let invalid =
        postcard::to_allocvec(&("run-bad", 1u64, 11u32, 0u64, 19i64, "bad flags", 17i64, 8u8))
            .unwrap();
    assert!(Record::decode(14, &invalid).is_err());
}

#[test]
fn older_human_readable_call_detail_defaults_to_refunding() {
    let old = serde_json::json!({"position":0,"retry_not_before":19,"cause":"ready later","audit_at":17,"proven_unsent":true});
    let deferred: Deferred = serde_json::from_value(old).unwrap();
    assert!(deferred.refund_dispatch);
    assert_eq!(deferred.attempt, None);
    let value = serde_json::to_value(&deferred).unwrap();
    assert_eq!(value["proven_unsent"], true);
    assert_eq!(value["refund_dispatch"], true);
}

#[test]
fn pre_scope_cut_snapshot_decodes_each_old_reservation_bool_without_field_shift() {
    use basal_core::StoredClass;
    use basal_core::run_log::Lsn;
    use basal_core::run_records::*;
    use serde::Serialize;
    use std::collections::BTreeMap;
    #[derive(Serialize)]
    struct OldUsage {
        input_tokens: u64,
        output_tokens: u64,
    }
    #[derive(Serialize)]
    struct OldArrival {
        position: u64,
        arrival_seq: u64,
        outcome: String,
        settlement: bool,
        value: String,
        value_hash: [u8; 32],
        handle: Option<String>,
        usage: Option<OldUsage>,
        cost_micros: Option<u64>,
        kv_effect: Option<KvEffect>,
        durability: ArrivalDurability,
    }
    #[derive(Serialize)]
    struct OldCall {
        identity: CallIdentity,
        pre_accounted_attempts: u64,
        pre_accounted_reservation: bool,
        intent: Option<Intent>,
        prior_intents: Vec<Intent>,
        sync: Option<Sync>,
        accepted: Option<String>,
        arrival: Option<OldArrival>,
        delivery_order: Option<u64>,
        deferred: Option<Deferred>,
        prior_deferrals: Vec<Deferred>,
        unknown: Option<String>,
        resolutions: Vec<Resolution>,
    }
    #[derive(Serialize)]
    struct OldState {
        admitted_ms: i64,
        generation: u64,
        claim: Option<Claim>,
        activation_count: u64,
        broken: u64,
        fingerprint: Option<String>,
        script_started: bool,
        phase: String,
        deadline_exempt: bool,
        awaited: Vec<u64>,
        parked: bool,
        readiness: u64,
        event_bodies: Vec<Body>,
        calls: BTreeMap<u64, OldCall>,
        overlay: BTreeMap<String, Option<String>>,
        next_delivery_order: u64,
        next_arrival_seq: u64,
        commit_seq: Option<u64>,
    }
    for reserved in [false, true] {
        let call = OldCall {
            identity: CallIdentity::new(
                basal_proto::HostCall {
                    position: 7,
                    kind: basal_proto::CallKind::Op {
                        module: "old".into(),
                        op: "write".into(),
                    },
                    args: basal_proto::JsonText::null(),
                },
                StoredClass::KeyedMutation,
            ),
            pre_accounted_attempts: 3,
            pre_accounted_reservation: reserved,
            intent: None,
            prior_intents: vec![],
            sync: None,
            accepted: None,
            arrival: Some(OldArrival {
                position: 7,
                arrival_seq: 0,
                outcome: "fulfilled".into(),
                settlement: true,
                value: "null".into(),
                value_hash: basal_core::ids::payload_hash(
                    basal_proto::Settlement::Fulfilled,
                    &basal_proto::JsonText::null(),
                ),
                handle: None,
                usage: Some(OldUsage {
                    input_tokens: 9,
                    output_tokens: 5,
                }),
                cost_micros: None,
                kv_effect: None,
                durability: ArrivalDurability::default(),
            }),
            delivery_order: None,
            deferred: None,
            prior_deferrals: vec![],
            unknown: None,
            resolutions: vec![],
        };
        let human = serde_json::to_value(&call).unwrap();
        let decoded: CallState = serde_json::from_value(human).unwrap();
        assert!(!decoded.scope_cut);
        assert!(decoded.usage_report.is_none());
        assert_eq!(decoded.pre_accounted_reservation, reserved);
        let old_state = OldState {
            admitted_ms: 1,
            generation: 2,
            claim: None,
            activation_count: 3,
            broken: 4,
            fingerprint: None,
            script_started: true,
            phase: "old".into(),
            deadline_exempt: false,
            awaited: vec![7],
            parked: true,
            readiness: 9,
            event_bodies: vec![],
            calls: BTreeMap::from([(7, call)]),
            overlay: BTreeMap::from([("tail".into(), Some("preserved".into()))]),
            next_delivery_order: 11,
            next_arrival_seq: 12,
            commit_seq: None,
        };
        // Snapshot's pre-change enum ordinal is 18. All state/call fields above
        // use the old schema, including the reservation bool rather than flags.
        let bytes = postcard::to_allocvec(&(
            "old-snapshot",
            2u64,
            18u32,
            Lsn::default(),
            11u64,
            12u64,
            old_state,
        ))
        .unwrap();
        let mut record = Record::decode(21, &bytes).unwrap();
        assert_eq!(postcard::to_allocvec(&record).unwrap(), bytes);
        let Body::Snapshot { state, .. } = &mut record.body else {
            panic!("snapshot")
        };
        assert!(!state.calls[&7].scope_cut);
        assert!(state.calls[&7].usage_report.is_none());
        let usage = state.calls[&7]
            .arrival
            .as_ref()
            .unwrap()
            .usage
            .as_ref()
            .unwrap();
        assert_eq!((usage.input_tokens, usage.output_tokens), (9, 5));
        assert_eq!(usage.input_tokens + usage.output_tokens, 14);
        assert_eq!(state.calls[&7].pre_accounted_reservation, reserved);
        assert_eq!(state.overlay["tail"].as_deref(), Some("preserved"));
        assert_eq!(
            (state.next_delivery_order, state.next_arrival_seq),
            (11, 12)
        );
        state.calls.get_mut(&7).unwrap().scope_cut = true;
        state.calls.get_mut(&7).unwrap().usage_report = Some(UsageReport {
            input_tokens: Some(7),
            cache_write_tokens: Some(2),
            output_tokens: Some(5),
            cached_input_tokens: Some(100),
        });
        assert_eq!(
            Record::decode(21, &postcard::to_allocvec(&record).unwrap()).unwrap(),
            record
        );
    }
}
