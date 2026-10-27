use super::*;

struct MustNotSend;
impl Host for MustNotSend {
    fn classify(&self, _: &CallKind) -> CallClass {
        CallClass::Query
    }
    fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
        panic!("a dry run must never send a credential-bearing fetch")
    }
    fn now_ms(&self) -> f64 {
        0.0
    }
    fn random(&self) -> f64 {
        0.5
    }
    fn attach(&self, _: Arc<dyn CompletionSink>) {}
}

#[test]
fn flow_secrets_both_dry_run_modes_capture_secret_fetch_without_contacting_live_host() {
    for mode in [Mode::Capture, Mode::Live] {
        let host = CaptureHost {
            mode,
            live: Arc::new(MustNotSend),
            catalog: Arc::new(basal_host::MockCatalog::new()),
            clock: basal_core::Clock::manual(0),
            random_state: AtomicU64::new(1),
            live_positions: Mutex::new(BTreeSet::new()),
        };
        let envelope = json!({"args":{"url":"https://x.test/repos/a","options":{"secret":"named-key"}},
            "grant":{"hosts":[{"host":"x.test","methods":["GET"]}]},"credential":{"name":"named-key","value":"[secret]"}});
        let mut request = CallRequest {
            flow_id: "flow".into(),
            flow_version: Some(1),
            run_id: "dry-run".into(),
            position: 0,
            kind: CallKind::Primitive(basal_proto::Primitive::NetFetch),
            args: JsonText::new(envelope.to_string()).unwrap(),
            idempotency_key: "key".into(),
            attempt: 1,
        };
        assert!(!host.runs_live(&request));
        let Dispatched::Completed(outcome) = host.dispatch(&request).unwrap() else {
            panic!("captured synchronously")
        };
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            CAPTURED
        );
        assert!(request.args.as_str().contains("named-key"));
        let mut ordinary = envelope;
        ordinary["args"]["options"]
            .as_object_mut()
            .unwrap()
            .remove("secret");
        request.args = JsonText::new(ordinary.to_string()).unwrap();
        assert_eq!(host.runs_live(&request), mode == Mode::Live);
    }
}
