use super::*;
use crate::broca::BrocaStore;
use crate::codemode::models;
use basal_host::broca::{
    BrocaHost, Route, StateStore, Transport as BrocaTransport, fake::FakeBroca, wire::*,
};
use basal_host::selector::{ModelSelector, RoutingSelector};
use basal_host::{
    CallClass, CallRequest, CompletionSink, Dispatched, Host, TokenUsage, TransportError,
};

#[derive(Default)]
struct Routing {
    calls: Mutex<Vec<Value>>,
    outcomes: Mutex<Vec<Value>>,
}
impl Transport for Routing {
    fn catalog(&self) -> std::result::Result<Value, WireError> {
        unreachable!()
    }
    fn tool(&self, _: &str, _: &str, _: Value, _: &str) -> std::result::Result<Value, WireError> {
        panic!("no tool route")
    }
    fn management(
        &self,
        module: &str,
        op: &str,
        params: Value,
    ) -> std::result::Result<Value, WireError> {
        assert_eq!(module, "prefrontal-routing");
        match op {
            "route.select" => {
                self.calls.lock().unwrap().push(params);
                Ok(
                    json!({"selected":{"model":{"providerID":"registry","modelID":"chosen"}},"decisionID":"decision","runner":{"provider":"fake","model":"test"}}),
                )
            }
            "route.set_decision_outcome" => {
                self.outcomes.lock().unwrap().push(params);
                Ok(json!({"ok":true}))
            }
            _ => panic!("unexpected route op {op}"),
        }
    }
}

struct ObservedBroca {
    fake: FakeBroca,
    store: Arc<Store>,
    frozen: Mutex<Vec<bool>>,
}
impl BrocaTransport for ObservedBroca {
    fn send(
        &self,
        route: &Route,
        params: &[u8],
    ) -> std::result::Result<SendResult, basal_host::broca::BrocaError> {
        let send: SendParams = serde_json::from_slice(params).unwrap();
        let frozen = self.store.read(|c| Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM codemode_models m JOIN codemode_calls c USING(run_id,position) WHERE c.idempotency_key=?1 AND m.envelope IS NOT NULL AND c.entered_at IS NOT NULL)",[send.send_id.as_ref().unwrap()],|r|r.get(0))?)).unwrap();
        self.frozen.lock().unwrap().push(frozen);
        self.fake.send(route, params)
    }
    fn send_status(
        &self,
        route: &Route,
        params: &SendStatusParams,
    ) -> std::result::Result<Value, basal_host::broca::BrocaError> {
        self.fake.send_status(route, params)
    }
    fn event_get(
        &self,
        route: &Route,
        params: &RunEventGetParams,
    ) -> std::result::Result<RunEventReply, basal_host::broca::BrocaError> {
        self.fake.event_get(route, params)
    }
    fn result(
        &self,
        route: &Route,
        params: &RunResultParams,
    ) -> std::result::Result<RunResultResponse, basal_host::broca::BrocaError> {
        self.fake.result(route, params)
    }
    fn status(
        &self,
        route: &Route,
        params: &StatusParams,
    ) -> std::result::Result<RunStatusResponse, basal_host::broca::BrocaError> {
        self.fake.status(route, params)
    }
}
struct CommittedBroca {
    host: Arc<BrocaHost>,
    committed: mpsc::Sender<u64>,
}
impl Host for CommittedBroca {
    fn attach_codemode(&self, sink: Arc<dyn CompletionSink>) {
        self.host.attach_codemode(sink);
    }
    fn configure_flow(
        &self,
        flow_id: &str,
        scope: Option<basal_host::flow_scope::RegisteredScope>,
    ) {
        self.host.configure_flow(flow_id, scope);
    }
    fn classify(&self, kind: &CallKind) -> CallClass {
        self.host.classify(kind)
    }
    fn dispatch(&self, request: &CallRequest) -> std::result::Result<Dispatched, TransportError> {
        self.host.dispatch(request)
    }
    fn dispatch_committed(&self, request: &CallRequest) {
        self.host.dispatch_committed(request);
        self.committed.send(request.position).unwrap();
    }
    fn now_ms(&self) -> f64 {
        self.host.now_ms()
    }
    fn random(&self) -> f64 {
        self.host.random()
    }
    fn attach(&self, sink: Arc<dyn CompletionSink>) {
        self.host.attach(sink);
    }
}

struct ModelFixture {
    f: Fixture,
    wire: Arc<ObservedBroca>,
    routing: Arc<Routing>,
    selector: Arc<dyn ModelSelector>,
    broca: Arc<BrocaHost>,
    committed: mpsc::Receiver<u64>,
}
impl ModelFixture {
    fn new() -> Self {
        let f = Fixture::new();
        let routing = Arc::new(Routing::default());
        let selector: Arc<dyn ModelSelector> = Arc::new(RoutingSelector::new(routing.clone()));
        let wire = Arc::new(ObservedBroca {
            fake: FakeBroca::default(),
            store: f.host.store.clone(),
            frozen: Mutex::new(Vec::new()),
        });
        let broca = Arc::new(BrocaHost::new(
            wire.clone(),
            Arc::new(BrocaStore::new(f.host.store.clone())),
            "/".into(),
            "basal".into(),
            selector.clone(),
        ));
        let (committed, commits) = mpsc::channel();
        f.supervisor.clone().with_models(
            Arc::new(CommittedBroca {
                host: broca.clone(),
                committed,
            }),
            selector.clone(),
        );
        let catalog = models::catalog();
        f.start(
            &[(
                "model",
                "basal",
                "model",
                catalog[0]["input_schema"].clone(),
            )],
            Limits::default(),
            100_000,
        );
        assert!(matches!(
            f.parent.recv_timeout(TIMEOUT).unwrap(),
            ParentMessage::Activate(_)
        ));
        Self {
            f,
            wire,
            routing,
            selector,
            broca,
            committed: commits,
        }
    }
    fn request(&self, position: u64, name: &str, input: Value) -> HostCall {
        let call = HostCall {
            position,
            kind: CallKind::Tool { name: name.into() },
            args: JsonText::new(input.to_string()).unwrap(),
        };
        self.f
            .host
            .store
            .write(|tx| {
                store::insert_call(
                    tx,
                    "r",
                    position,
                    name,
                    call.args.len() as u64,
                    CallStart::Intent { at: 100 },
                )
            })
            .unwrap();
        call
    }
    fn run(&self) -> RunRecord {
        let Lookup::Found(run) = self.f.host.store.read(|c| store::lookup(c, "r")).unwrap() else {
            panic!()
        };
        *run
    }
    fn model(&self, pos: u64) -> Delivery {
        self.model_input(pos, json!({"prompt":"hello","max_output":1}))
    }
    fn model_input(&self, pos: u64, input: Value) -> Delivery {
        self.invoke(pos, "model", input)
    }
    fn invoke(&self, pos: u64, name: &str, input: Value) -> Delivery {
        self.f.issue(pos, name, input);
        let until = Instant::now() + TIMEOUT;
        loop {
            if let Ok(ParentMessage::Deliver(d)) =
                self.f.parent.recv_timeout(Duration::from_millis(1))
            {
                return d;
            }
            if self.wire.fake.keys().contains(&store::call_key("r", pos)) {
                break;
            }
            assert!(
                Instant::now() < until,
                "model neither dispatched nor refused"
            );
        }
        self.finish_usage(
            pos,
            "done",
            Usage {
                input_tokens: Some(1),
                cache_write_tokens: Some(0),
                output_tokens: Some(1),
                ..Usage::default()
            },
        )
    }
    fn finish_usage(&self, position: u64, text: &str, usage: Usage) -> Delivery {
        let key = store::call_key("r", position);
        // Wait for dispatch_committed to finish its first read before completing
        // the fake run. A concurrent read skips a locked call, and no bus event
        // reaches this fixture to wake the host again.
        assert_eq!(self.committed.recv_timeout(TIMEOUT).unwrap(), position);
        self.wire
            .fake
            .finish(&key, text, RunFinishReason::Completed, Some(usage))
            .unwrap();
        self.broca.poll().unwrap();
        self.f.delivery()
    }
}
impl Drop for ModelFixture {
    fn drop(&mut self) {
        self.f.cleanup(true);
    }
}

#[test]
fn model_catalog_is_basal_owned_and_inputs_are_closed() {
    let catalog = models::add_catalog(&json!([])).unwrap();
    assert_eq!(catalog[0]["name"], "model");
    assert_eq!(catalog.as_array().unwrap().len(), 1);
    let f = ModelFixture::new();
    for (i, input) in [
        json!({"prompt":"hi","tools":[]}),
        json!({"prompt":"hi","model":"x"}),
        json!({"prompt":"hi","provider":"x"}),
        json!({"prompt":"hi","max_output":0}),
        json!({"prompt":"hi","system":7}),
    ]
    .into_iter()
    .enumerate()
    {
        f.f.issue(i as u64, "model", input);
        let d = f.f.delivery();
        assert_eq!(d.settlement, Settlement::Rejected);
        assert_eq!(parse(d.value.as_str()).unwrap()["code"], "invalid_input");
    }
    assert!(f.wire.fake.sends().is_empty());
}

#[test]
fn model_routing_uses_codemode_caller_class_and_run_task() {
    let f = ModelFixture::new();
    f.model_input(0, json!({"prompt":"hello","max_output":1,"iq":50,"eq":50}));
    let calls = f.routing.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        json!({"targetAgent":"codemode","requirements":{"iq":50,"eq":50},
        "excludeRouteKeys":[],"sendID":store::call_key("r",0),"taskId":"codemode:r","substrate":"broca"})
    );
    assert_eq!(
        f.routing.outcomes.lock().unwrap().as_slice(),
        [json!({"decisionID":"decision","outcome":"completed"})]
    );
}

#[test]
fn model_calls_use_distinct_run_scoped_sessions() {
    let f = ModelFixture::new();
    f.model(0);
    f.model(1);
    let sends = f.wire.fake.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].0.session, "basal:codemode-r:0");
    assert_eq!(sends[1].0.session, "basal:codemode-r:1");
    for send in sends {
        assert_eq!(send.0.flow_id.as_deref(), Some("codemode:r"));
    }
}

#[test]
fn model_routing_is_journaled_before_send_and_reused_on_resend() {
    let f = ModelFixture::new();
    f.wire.fake.cut_next_reply();
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(f.f.delivery().settlement, Settlement::Rejected);
    assert_eq!(*f.wire.frozen.lock().unwrap(), [true]);
    let call = HostCall {
        position: 0,
        kind: CallKind::Tool {
            name: "model".into(),
        },
        args: JsonText::new(json!({"prompt":"hello","max_output":1}).to_string()).unwrap(),
    };
    let first = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    f.broca.dispatch_model(&first).unwrap();
    let again = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    assert_eq!(first, again);
    assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    let sends = f.wire.fake.sends();
    assert_eq!(sends[0], sends[1]);
}

#[test]
fn broca_without_run_scope_opt_in_is_refused_without_dispatch() {
    let f = ModelFixture::new();
    f.f.host.broca_opted_in.store(false, Ordering::SeqCst);
    let d = f.model(0);
    let value = parse(d.value.as_str()).unwrap();
    assert_eq!(d.settlement, Settlement::Rejected);
    assert_eq!(value["code"], "tool_unavailable");
    assert_eq!(value["reason"], "provider_not_opted_in");
    assert!(f.routing.calls.lock().unwrap().is_empty());
    assert!(f.wire.fake.sends().is_empty());
    assert_eq!(f.f.calls()[0].outcome, Outcome::ToolUnavailable);
}

/// When Broca answers a codemode model run with the provider's own `{code,
/// message, detail}`, the program receives exactly that. A codemode run has no
/// flow, so basal's flow-refusal wording would describe a scope the program
/// never has, and would hide which layer refused the call.
#[test]
fn a_provider_refusal_reaches_the_program_as_the_provider_sent_it() {
    let f = ModelFixture::new();
    f.wire.fake.pending_next();
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(f.committed.recv_timeout(TIMEOUT).unwrap(), 0);
    let provider = json!({
        "class": "permanent",
        "code": "flow_scope_required",
        "message": "provider's own text",
        "detail": {"scope_kind": "codemode"},
    });
    f.wire
        .fake
        .finish_with_error(
            &store::call_key("r", 0),
            "error",
            serde_json::from_value(provider.clone()).unwrap(),
            Some(Usage::default()),
        )
        .unwrap();
    f.broca.poll().unwrap();
    let delivery = f.f.delivery();
    assert_eq!(delivery.settlement, Settlement::Rejected);
    let value = parse(delivery.value.as_str()).unwrap();
    assert_eq!(value["tool"], "model");
    assert_eq!(value["outcome"], "error");
    // The provider's reply is the refusal: basal adds no wording of its own,
    // and the rest of the reply travels in `detail`.
    assert_eq!(value["code"], "flow_scope_required");
    assert_eq!(value["message"], "provider's own text");
    assert_eq!(value["detail"], provider);
    assert_eq!(f.f.calls()[0].code.as_deref(), Some("flow_scope_required"));
}

#[test]
fn model_call_budget_rejects_call_21_without_ending_run() {
    let f = ModelFixture::new();
    for pos in 0..20 {
        assert_eq!(f.model(pos).settlement, Settlement::Fulfilled);
    }
    let d = f.model(20);
    assert_eq!(d.settlement, Settlement::Rejected);
    let value = parse(d.value.as_str()).unwrap();
    assert_eq!(value["code"], "budget_exhausted");
    assert_eq!(value["reason"], "model_calls");
    assert_eq!(f.wire.fake.sends().len(), 20);
    f.f.finish("\"caught\"");
    assert_eq!(f.f.terminal()["status"], "completed");
    assert_eq!(f.f.calls()[20].code.as_deref(), Some("budget_exhausted"));
}

#[test]
fn model_token_budget_reserves_before_send_and_is_catchable() {
    let f = ModelFixture::new();
    f.wire.fake.pending_next();
    f.f.issue(0, "model", json!({"prompt":"hi","max_output":199_000}));
    let until = Instant::now() + TIMEOUT;
    while f.wire.fake.sends().is_empty() {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    let d = f.invoke(
        1,
        "model",
        json!({"prompt":"x".repeat(700),"max_output":4096}),
    );
    assert_eq!(d.position, 1);
    assert_eq!(parse(d.value.as_str()).unwrap()["reason"], "model_tokens");
    assert_eq!(parse(d.value.as_str()).unwrap()["code"], "budget_exhausted");
    assert_eq!(f.wire.fake.keys().len(), 1);
    f.finish_usage(
        0,
        "done",
        Usage {
            input_tokens: Some(1),
            cache_write_tokens: Some(0),
            output_tokens: Some(1),
            ..Usage::default()
        },
    );
    assert_eq!(f.model(2).settlement, Settlement::Fulfilled);
    f.f.finish("1");
    assert_eq!(f.f.terminal()["status"], "completed");
}

#[test]
fn model_usage_counts_fresh_cache_writes_output_but_not_cached_input() {
    let f = ModelFixture::new();
    let call = f.request(0, "model", json!({"prompt":"hi"}));
    models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    let usage = TokenUsage {
        input_tokens: Some(100),
        cache_write_tokens: Some(20),
        output_tokens: Some(30),
        cached_input_tokens: Some(1_000_000),
    };
    f.f.host
        .store
        .write(|tx| models::settle(tx, "r", 0, Some(usage), false))
        .unwrap();
    f.f.host
        .store
        .write(|tx| models::settle(tx, "r", 0, None, false))
        .unwrap();
    let charged: u64 =
        f.f.host
            .store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT charged FROM codemode_models WHERE run_id='r' AND position=0",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
    assert_eq!(charged, 150);
    let call = f.request(1, "model", json!({"prompt":"hi"}));
    models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    f.f.host
        .store
        .write(|tx| {
            models::settle(
                tx,
                "r",
                1,
                Some(TokenUsage {
                    input_tokens: Some(1),
                    ..TokenUsage::default()
                }),
                false,
            )
        })
        .unwrap();
    let (charged, reserved): (u64, u64) =
        f.f.host
            .store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT charged,reserved FROM codemode_models WHERE run_id='r' AND position=1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
    assert_eq!(charged, reserved);
}

#[test]
fn model_dispatch_declares_no_tools() {
    let f = ModelFixture::new();
    f.model(0);
    let sends = f.wire.fake.sends();
    let send: SendParams = serde_json::from_slice(&sends[0].1).unwrap();
    assert!(send.tools.is_empty());
    assert_eq!(send.tool_choice, json!({"type":"none"}));
    assert!(!send.append_episode);
}

#[test]
fn retired_local_classify_is_unknown_to_programs() {
    let f = ModelFixture::new();
    f.f.issue(0, "classify", json!({"text":"good","labels":["yes","no"]}));
    assert_eq!(
        parse(f.f.delivery().value.as_str()).unwrap()["code"],
        "unknown_tool"
    );
    assert!(f.wire.fake.sends().is_empty());
}

#[test]
fn completed_codemode_runs_are_never_resent_by_broca_polling() {
    let f = ModelFixture::new();
    f.wire.fake.pending_next();
    let call = f.request(0, "model", json!({"prompt":"hi"}));
    let request = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    f.broca.dispatch_model(&request).unwrap();
    f.f.supervisor.cancel("r").unwrap();
    f.broca.poll().unwrap();
    assert_eq!(f.wire.fake.sends().len(), 1);
    assert!(
        BrocaStore::new(f.f.host.store.clone())
            .pending_ids()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn broca_startup_waits_for_codemode_recovery_before_polling() {
    let f = ModelFixture::new();
    f.wire.fake.pending_next();
    let call = f.request(0, "model", json!({"prompt":"hi"}));
    let request = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    f.broca.dispatch_model(&request).unwrap();
    let restarting = BrocaHost::new(
        f.wire.clone(),
        Arc::new(BrocaStore::new(f.f.host.store.clone())),
        "/".into(),
        "basal".into(),
        f.selector.clone(),
    );
    restarting.poll().unwrap();
    assert_eq!(f.wire.fake.sends().len(), 1);
    // Nor is Broca asked about the queued call before the supervisor that
    // owns it has attached.
    assert_eq!(f.wire.fake.send_statuses(), 0);
}

#[test]
fn model_forwards_program_values_and_returns_completion_text() {
    let f = ModelFixture::new();
    let d = f.model_input(
        0,
        json!({"prompt":"own prompt","system":"own system","max_output":23}),
    );
    assert_eq!(parse(d.value.as_str()).unwrap(), json!({"text":"done"}));
    let sends = f.wire.fake.sends();
    let send: SendParams = serde_json::from_slice(&sends[0].1).unwrap();
    assert_eq!(send.prompt, "own prompt");
    assert_eq!(send.system.as_deref(), Some("own system"));
    assert_eq!(send.generation.max_output_tokens, Some(23));
}

#[test]
fn overcap_provider_usage_still_yields_typed_budget_refusal() {
    let f = ModelFixture::new();
    for position in 0..2 {
        let call = f.request(position, "model", json!({"prompt":"hi"}));
        models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    }
    let usage = TokenUsage {
        input_tokens: Some(u64::MAX),
        cache_write_tokens: Some(0),
        output_tokens: Some(0),
        cached_input_tokens: None,
    };
    for position in 0..2 {
        f.f.host
            .store
            .write(|tx| models::settle(tx, "r", position, Some(usage), false))
            .unwrap();
    }
    let call = f.request(2, "model", json!({"prompt":"hi"}));
    let error = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap_err();
    assert!(
        matches!(error, WireError::RefusedDetails {code,detail,..} if code=="budget_exhausted" && detail["reason"]=="model_tokens")
    );
}

#[test]
fn model_defaults_to_iq_50_and_eq_0() {
    let f = ModelFixture::new();
    f.invoke(0, "model", json!({"prompt":"hello","max_output":1}));
    let calls = f.routing.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    for call in calls.iter() {
        assert_eq!(call["requirements"], json!({"iq":50,"eq":0}));
    }
    let catalog = models::catalog();
    for tool in catalog.as_array().unwrap() {
        let properties = &tool["input_schema"]["properties"];
        for (name, default) in [("iq", 50), ("eq", 0)] {
            assert_eq!(properties[name]["default"], default);
            let description = properties[name]["description"].as_str().unwrap();
            assert!(description.contains("0–100"));
            assert!(description.contains(&format!("Defaults to {default}.")));
        }
    }
}

#[test]
fn model_forwards_explicit_iq_and_eq() {
    let f = ModelFixture::new();
    for (position, name, input, expected) in [
        (
            0,
            "model",
            json!({"prompt":"hello","max_output":1,"iq":0,"eq":100}),
            json!({"iq":0,"eq":100}),
        ),
        (
            1,
            "model",
            json!({"prompt":"hello","max_output":1,"iq":100,"eq":0}),
            json!({"iq":100,"eq":0}),
        ),
        (
            2,
            "model",
            json!({"prompt":"hello","max_output":1,"iq":87.0,"eq":42.0}),
            json!({"iq":87,"eq":42}),
        ),
        (
            3,
            "model",
            json!({"prompt":"hello","max_output":1,"iq":37,"eq":82}),
            json!({"iq":37,"eq":82}),
        ),
    ] {
        assert_eq!(
            f.invoke(position, name, input).settlement,
            Settlement::Fulfilled
        );
        let calls = f.routing.calls.lock().unwrap();
        assert_eq!(calls[position as usize]["requirements"], expected);
    }
}

#[test]
fn model_refuses_invalid_demands_before_routing() {
    let f = ModelFixture::new();
    let mut position = 0;
    for name in ["model"] {
        for field in ["iq", "eq"] {
            for bad in [json!(-1), json!(101), json!(0.5), json!("50"), Value::Null] {
                let mut input = json!({"prompt":"hello","max_output":1});
                input[field] = bad;
                let delivery = f.invoke(position, name, input);
                assert_eq!(delivery.settlement, Settlement::Rejected);
                assert_eq!(
                    parse(delivery.value.as_str()).unwrap()["code"],
                    "invalid_input"
                );
                position += 1;
            }
        }
    }
    assert!(f.routing.calls.lock().unwrap().is_empty());
    assert!(f.wire.fake.sends().is_empty());
}

#[test]
fn a_codemode_model_call_settles_from_a_committed_run_ended() {
    let f = ModelFixture::new();
    let key = store::call_key("r", 0);
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(f.committed.recv_timeout(TIMEOUT).unwrap(), 0);
    // The accepted call is waiting: its run id is indexed for the event
    // reader, and it has a scheduled check.
    let adapter = BrocaStore::new(f.f.host.store.clone());
    let (run_id, check_at): (Option<String>, Option<i64>) =
        f.f.host
            .store
            .read(|c| {
                Ok(c.query_row(
                    "SELECT broca_run_id, check_at FROM broca_waits WHERE send_id = ?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
    assert_eq!(run_id, Some(format!("run:{key}")));
    assert!(check_at.is_some());
    // The run was started with a 100 s deadline and has waited on no one.
    assert_eq!(adapter.deadline("r").unwrap(), Some(100_000));
    f.wire
        .fake
        .finish(
            &key,
            "from the event",
            RunFinishReason::Completed,
            Some(Usage::default()),
        )
        .unwrap();
    let wake = f.wire.fake.publish_ended(&key).unwrap();
    // Commit the wake as the event reader does.
    f.f.host
        .store
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_wakes (send_id, event_key, kind, digest, run_id, session_ref, seq, received_at) VALUES (?1, ?2, 'ended', ?3, ?4, ?5, ?6, 0)",
                rusqlite::params![key, wake.event_key, wake.digest, wake.run_id, wake.session_ref, wake.seq],
            )?;
            Ok(())
        })
        .unwrap();
    // A fresh adapter reads the committed wake back, as after a restart.
    assert_eq!(adapter.wakes().unwrap(), vec![wake]);
    assert_eq!(adapter.next_check().unwrap(), Some(0));
    f.broca.wake().unwrap();
    let d = f.f.delivery();
    assert_eq!(
        parse(d.value.as_str()).unwrap(),
        json!({"text":"from the event"})
    );
    // Settled: the wait and its wake are gone, so nothing is due again.
    assert!(adapter.wakes().unwrap().is_empty());
    assert_eq!(adapter.next_check().unwrap(), None);
}

#[test]
fn deleting_a_codemode_call_cascades_to_its_wait_and_wakes() {
    let f = ModelFixture::new();
    let key = store::call_key("r", 0);
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(f.committed.recv_timeout(TIMEOUT).unwrap(), 0);
    let counts = |store: &Store| -> (i64, i64) {
        store
            .read(|c| {
                Ok((
                    c.query_row("SELECT COUNT(*) FROM broca_waits", [], |r| r.get(0))?,
                    c.query_row("SELECT COUNT(*) FROM broca_wakes", [], |r| r.get(0))?,
                ))
            })
            .unwrap()
    };
    f.f.host
        .store
        .write(|tx| {
            tx.execute(
                "INSERT INTO broca_wakes (send_id, event_key, kind, digest, received_at) VALUES (?1, 'k', 'ended', 'd', 0)",
                [&key],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(&f.f.host.store), (1, 1));
    // Codemode retention deletes the call row; its Broca snapshot goes by
    // cascade, and the wait and wake with it.
    f.f.host
        .store
        .write(|tx| {
            tx.execute(
                "DELETE FROM codemode_calls WHERE run_id = 'r' AND position = 0",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(counts(&f.f.host.store), (0, 0));
}
