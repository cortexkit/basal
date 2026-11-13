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
    answer: Mutex<Option<Value>>,
    refusals: Mutex<VecDeque<WireError>>,
}
impl Transport for Routing {
    fn catalog(&self) -> std::result::Result<Value, WireError> {
        Ok(
            json!({"modules":[{"module_id":"model-picker","capabilities":{"provides":["routing/v1"]}}]}),
        )
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
        assert_eq!(module, "model-picker");
        match op {
            "role.describe" => Ok(basal_host::selector::fake_describe(false)),
            "route.select" => {
                self.calls.lock().unwrap().push(params);
                if let Some(refusal) = self.refusals.lock().unwrap().pop_front() {
                    return Err(refusal);
                }
                Ok(self.answer.lock().unwrap().clone().unwrap_or_else(|| {
                    basal_host::selector::fake_answer(
                        "decision",
                        ("registry", "chosen"),
                        ("fake", "test"),
                    )
                }))
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
    model_waits: mpsc::Receiver<(u64, i64)>,
}
impl ModelFixture {
    fn new() -> Self {
        Self::with_limits(Limits::default())
    }
    fn with_limits(limits: Limits) -> Self {
        let f = Fixture::new();
        let (model_wait, model_waits) = mpsc::channel();
        *f.supervisor.0.after_model_wait.lock().unwrap() =
            Some(Arc::new(move |position, recheck| {
                model_wait.send((position, recheck)).unwrap();
            }));
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
        f.catalog.set_op(
            "provider",
            "classify_alias",
            OpDecl {
                kind: Some(OpKind::Query),
                cause_echo: false,
                shell_capable: false,
            },
        );
        f.catalog.set_tool_contract(
            "provider",
            "classify_alias",
            json!({"capabilities":[cortexkit_llm_tools::classify::TAG]}),
        );
        f.start(
            &[
                (
                    "model",
                    "basal",
                    "model",
                    catalog[0]["input_schema"].clone(),
                ),
                (
                    "llm_classify",
                    "provider",
                    "classify_alias",
                    cortexkit_llm_tools::classify::input_schema(),
                ),
            ],
            limits,
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
            model_waits,
        }
    }
    fn wait_model_retry(&self, sends: usize) -> i64 {
        // A queued row and a recorded send do not prove the driver registered
        // its retry: the row is cleared before the recheck deadline is set,
        // and a snapshot read before the send can still look queued. Advance
        // the manual clock only after the driver's registration acknowledgement.
        let (position, recheck) = self.model_waits.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(position, 0);
        assert_eq!(self.wire.fake.sends().len(), sends);
        let calls = self.f.calls();
        assert!(calls[0].intent_at.is_none());
        assert!(calls[0].entered_at.is_none());
        assert_eq!(calls[0].outcome, Outcome::Pending);
        recheck
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

#[test]
fn transient_model_send_retries_with_one_reservation_then_charges_only_success() {
    let f = ModelFixture::new();
    f.wire
        .fake
        .refuse_next_send_with_detail("transient", "credentials_unavailable");
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    f.wait_model_retry(1);
    assert!(f.wire.fake.keys().is_empty());
    assert!(f.f.parent.try_recv().is_err());
    assert!(f.f.supervisor.holds("r").is_empty());
    let ledger = || {
        f.f.host.store.read(|c| {
        Ok(c.query_row("SELECT COUNT(*),SUM(reserved),SUM(charged) FROM codemode_models WHERE run_id='r'", [], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<i64>>(2)?)))?)
    }).unwrap()
    };
    let reserved = ledger();
    assert_eq!(reserved.0, 1);
    assert!(reserved.1 > 0);
    assert_eq!(
        reserved.2, None,
        "a free refusal neither charges nor settles the retry reservation"
    );
    f.wire
        .fake
        .refuse_next_send_with_detail("transient", "variant_unsupported");
    f.f.clock.advance(PROVIDER_RECHECK_MS);
    f.wait_model_retry(2);
    assert_eq!(ledger(), reserved);
    f.f.clock.advance(PROVIDER_RECHECK_MS);
    let delivery = f.finish_usage(
        0,
        "answer",
        Usage {
            input_tokens: Some(1),
            cache_write_tokens: Some(0),
            output_tokens: Some(1),
            ..Usage::default()
        },
    );
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(ledger(), (1, reserved.1, Some(2)));
    let dollars: (i64, f64) = f.f.host.store.read(|c| Ok(c.query_row("SELECT COUNT(*),COALESCE(SUM(COALESCE(charged,reserved)),0) FROM classify_costs WHERE kind='program' AND run_id='r'", [], |r| Ok((r.get(0)?,r.get(1)?)))?)).unwrap();
    assert_eq!(
        dollars,
        (1, 0.000002),
        // Direct model calls now share the dollar ledger and cap with classifiers.
        "model retries settle one shared dollar reservation from actual usage"
    );
    assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    let sends = f.wire.fake.sends();
    assert_eq!(sends.len(), 3);
    assert!(sends.iter().all(|s| s == &sends[0]));
    f.f.finish("null");
    assert_eq!(f.f.terminal()["person_wait_ms"], 0);
}

#[test]
fn model_without_price_is_refused_before_send_and_releases_token_reservation() {
    let f = ModelFixture::new();
    let mut answer =
        basal_host::selector::fake_answer("decision", ("registry", "chosen"), ("fake", "test"));
    answer.as_object_mut().unwrap().remove("price");
    *f.routing.answer.lock().unwrap() = Some(answer);
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    let message = f.f.parent.recv_timeout(TIMEOUT).unwrap_or_else(|error| {
        panic!(
            "no refusal: {error:?}; run={:?}; calls={:?}",
            f.run(),
            f.f.calls()
        )
    });
    let ParentMessage::Deliver(delivery) = message else {
        panic!("expected a model refusal, got {message:?}")
    };
    assert_eq!(delivery.settlement, Settlement::Rejected);
    assert_eq!(
        parse(delivery.value.as_str()).unwrap()["code"],
        "price_unknown"
    );
    assert!(f.wire.fake.sends().is_empty());
    f.f.host
        .store
        .read(|c| {
            let charged: u64 = c.query_row(
                "SELECT charged FROM codemode_models WHERE run_id='r' AND position=0",
                [],
                |r| r.get(0),
            )?;
            let dollars: u64 = c.query_row(
                "SELECT COUNT(*) FROM classify_costs WHERE run_id='r'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!((charged, dollars), (0, 0));
            Ok(())
        })
        .unwrap();
    f.f.finish("null");
}

#[test]
fn model_task_estimate_is_replaced_once_by_actual_usage_in_both_ledgers() {
    let f = ModelFixture::new();
    let mut answer =
        basal_host::selector::fake_answer("decision", ("registry", "chosen"), ("fake", "test"));
    answer["price"]["estimated_task_usd"] = json!(0.125);
    *f.routing.answer.lock().unwrap() = Some(answer);
    let call = f.request(0, "model", json!({"prompt":"hello","max_output":32}));
    let request = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    let retry = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    assert_eq!(request.args, retry.args);
    assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    assert_eq!(
        f.f.host
            .store
            .read(|c| crate::classify_cost::spent(c, "program", "r"))
            .unwrap(),
        0.125
    );
    let usage = TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(20),
        cache_write_tokens: Some(0),
        cached_input_tokens: Some(50),
        ..Default::default()
    };
    for _ in 0..2 {
        f.f.host
            .store
            .write(|tx| models::settle(tx, "r", 0, Some(usage), false))
            .unwrap();
    }
    f.f.host.store.read(|c| {
        let dollars: (f64,f64) = c.query_row("SELECT reserved,charged FROM classify_costs WHERE kind='program' AND run_id='r' AND position=0", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let tokens: u64 = c.query_row("SELECT charged FROM codemode_models WHERE run_id='r' AND position=0", [], |r| r.get(0))?;
        assert_eq!(dollars, (0.125, 0.00012));
        assert_eq!(tokens, 120);
        Ok(())
    }).unwrap();
}

#[test]
fn model_and_classifier_share_the_program_dollar_cap_including_inflight_reserves() {
    let f = ModelFixture::with_limits(Limits {
        model_cost_usd: 0.125,
        ..Limits::default()
    });
    let mut answer =
        basal_host::selector::fake_answer("decision", ("registry", "chosen"), ("fake", "test"));
    answer["price"]["estimated_task_usd"] = json!(0.0625);
    *f.routing.answer.lock().unwrap() = Some(answer);
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(f.committed.recv_timeout(TIMEOUT).unwrap(), 0);
    assert_eq!(
        f.f.host
            .store
            .read(|c| crate::classify_cost::spent(c, "program", "r"))
            .unwrap(),
        0.0625
    );
    let vectors: Value = serde_json::from_str(cortexkit_llm_tools::vectors::CLASSIFY).unwrap();
    let mut input = vectors["valid_requests"][0]["value"].clone();
    input["max_cost_usd"] = json!(0.125);
    f.f.issue(1, "llm_classify", input);
    let attempt = f.f.attempt();
    assert_eq!(attempt.input["max_cost_usd"], 0.0625);
    attempt
        .reply
        .send(Ok(json!({"answers":{},"usage":{},"cost_usd":0.0625})))
        .unwrap();
    assert_eq!(f.f.delivery().position, 1);
    f.f.issue(2, "model", json!({"prompt":"hello","max_output":1}));
    let refused = f.f.delivery();
    assert_eq!(refused.position, 2);
    assert_eq!(
        parse(refused.value.as_str()).unwrap()["code"],
        "budget_exhausted"
    );
    assert_eq!(f.wire.fake.sends().len(), 1);
    f.wire
        .fake
        .finish(
            &store::call_key("r", 0),
            "answer",
            RunFinishReason::Completed,
            Some(Usage {
                input_tokens: Some(1),
                cache_write_tokens: Some(0),
                output_tokens: Some(1),
                ..Default::default()
            }),
        )
        .unwrap();
    f.broca.poll().unwrap();
    assert_eq!(f.f.delivery().position, 0);
    assert_eq!(
        f.f.host
            .store
            .read(|c| crate::classify_cost::spent(c, "program", "r"))
            .unwrap(),
        0.062502
    );
    f.f.finish("null");
}

#[test]
fn transient_model_send_wait_spends_wall_time_and_ends_unsent_for_free() {
    let f = ModelFixture::new();
    f.wire.fake.refuse_next_send("transient");
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    f.wait_model_retry(1);
    let blocked = f.f.block_handled();
    f.f.worker
        .send(WorkerMessage::Blocked { awaiting: vec![0] })
        .unwrap();
    blocked.recv_timeout(TIMEOUT).unwrap();
    assert!(f.f.supervisor.holds("r").is_empty());
    f.f.clock.set(100_000);
    let result = f.f.terminal();
    assert_eq!(result["status"], "budget_exhausted:wall");
    assert_eq!(result["person_wait_ms"], 0);
    f.f.assert_never_sent(0, Outcome::ToolUnavailable);
    assert_eq!(f.wire.fake.sends().len(), 1);
    let charged: i64 =
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
    assert_eq!(charged, 0);
    assert_eq!(
        f.f.host
            .store
            .read(|c| crate::classify_cost::spent(c, "program", "r"))
            .unwrap(),
        0.0
    );
}

#[test]
fn nonretryable_model_send_codes_are_final_regardless_of_detail() {
    for code in ["variant_unsupported", "open_failed"] {
        let f = ModelFixture::new();
        f.wire
            .fake
            .refuse_next_send_with_detail(code, "transient credentials_unavailable");
        f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
        let delivery = f.f.delivery();
        assert_eq!(delivery.settlement, Settlement::Rejected);
        assert_eq!(parse(delivery.value.as_str()).unwrap()["code"], code);
        assert_eq!(f.wire.fake.sends().len(), 1);
        assert_ne!(f.f.calls()[0].outcome, Outcome::Pending);
    }
}

#[test]
fn transient_repeat_of_accepted_model_send_stays_pending() {
    let f = ModelFixture::new();
    let input = json!({"prompt":"hello","max_output":1});
    f.f.issue(0, "model", input.clone());
    assert_eq!(f.committed.recv_timeout(TIMEOUT).unwrap(), 0);
    let call = HostCall {
        position: 0,
        kind: CallKind::Tool {
            name: "model".into(),
        },
        args: JsonText::new(input.to_string()).unwrap(),
    };
    let request = models::prepare(&f.f.host.store, f.selector.as_ref(), &f.run(), &call).unwrap();
    f.wire.fake.refuse_next_send("transient");
    assert!(matches!(
        f.broca.dispatch(&request),
        Err(TransportError::Unavailable {
            sent: basal_host::Sent::Maybe(_),
            ..
        })
    ));
    let saved = f.broca.dispatch_model(&request).unwrap();
    assert!(matches!(saved, Dispatched::Accepted { .. }));
    assert!(f.f.parent.try_recv().is_err());
    f.wire
        .fake
        .finish(
            &request.idempotency_key,
            "answer",
            RunFinishReason::Completed,
            None,
        )
        .unwrap();
    f.broca.poll().unwrap();
    assert_eq!(f.f.delivery().settlement, Settlement::Fulfilled);
}

#[test]
fn account_model_send_wait_honors_hint_then_succeeds_with_one_reservation() {
    for code in ["account_selection_refused", "account_limit_exhausted"] {
        let f = ModelFixture::new();
        // A classifier reservation belongs to a different call. Model retries
        // must not settle it or consume a second dollar reservation beside it.
        f.f.host
            .store
            .write(|tx| {
                let mut input = json!({"max_cost_usd":0.12});
                crate::classify_cost::reserve(
                    tx, "program", "r", 7, None, 100, 0.5, None, &mut input,
                )?
                .unwrap();
                Ok(())
            })
            .unwrap();
        f.wire
            .fake
            .refuse_next_send_with_data(code, json!({"retry_after_s":2.5}));
        f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
        f.wait_model_retry(1);
        let original = f.f.clock.now_ms();
        f.f.clock.set(original + 2499);
        f.f.catalog_notice();
        let blocked = f.f.block_handled();
        f.f.worker
            .send(WorkerMessage::Blocked { awaiting: vec![0] })
            .unwrap();
        blocked.recv_timeout(TIMEOUT).unwrap();
        assert_eq!(
            f.wire.fake.sends().len(),
            1,
            "neither timer nor catalog wake bypasses the hint"
        );
        f.f.clock.set(original + 2500);
        assert_eq!(
            f.finish_usage(
                0,
                "answer",
                Usage {
                    input_tokens: Some(1),
                    cache_write_tokens: Some(0),
                    output_tokens: Some(1),
                    ..Usage::default()
                }
            )
            .settlement,
            Settlement::Fulfilled
        );
        let ledgers: (i64, i64, i64, f64) =
            f.f.host
                .store
                .read(|c| {
                    let (models, charged): (i64, i64) = c.query_row(
                        "SELECT COUNT(*),SUM(charged) FROM codemode_models WHERE run_id='r'",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    let dollars: i64 = c.query_row(
                        "SELECT COUNT(*) FROM classify_costs WHERE run_id='r'",
                        [],
                        |r| r.get(0),
                    )?;
                    Ok((
                        models,
                        charged,
                        dollars,
                        crate::classify_cost::spent(c, "program", "r")?,
                    ))
                })
                .unwrap();
        // The classifier keeps its charge; the successful model call adds two microdollars.
        assert_eq!(ledgers, (1, 2, 2, 0.120002));
        let sends = f.wire.fake.sends();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0], sends[1]);
        assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    }
}

fn concurrency_full(reset_at_ms: Option<i64>) -> WireError {
    WireError::RefusedDetails {
        code: "no_route".into(),
        message: "all slots occupied".into(),
        detail: json!({"reason":"concurrency_full","reset_at_ms":reset_at_ms}),
    }
}

#[test]
fn routing_concurrency_wait_reselects_then_sends_once_with_original_arguments() {
    let f = ModelFixture::new();
    f.routing
        .refusals
        .lock()
        .unwrap()
        .push_back(concurrency_full(Some(2600)));
    f.f.issue(
        0,
        "model",
        json!({"prompt":"original prompt","max_output":1}),
    );
    assert_eq!(f.wait_model_retry(0), 2600);
    assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    assert_eq!(
        f.f.host
            .store
            .read(|c| crate::classify_cost::spent(c, "program", "r"))
            .unwrap(),
        0.0
    );
    f.f.clock.set(2599);
    f.f.catalog_notice();
    let blocked = f.f.block_handled();
    f.f.worker
        .send(WorkerMessage::Blocked { awaiting: vec![0] })
        .unwrap();
    blocked.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    f.f.clock.set(2600);
    assert_eq!(
        f.finish_usage(
            0,
            "answer",
            Usage {
                input_tokens: Some(1),
                cache_write_tokens: Some(0),
                output_tokens: Some(1),
                ..Usage::default()
            }
        )
        .settlement,
        Settlement::Fulfilled
    );
    let calls = f.routing.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], calls[1]);
    let sends = f.wire.fake.sends();
    assert_eq!(sends.len(), 1);
    let send: SendParams = serde_json::from_slice(&sends[0].1).unwrap();
    assert_eq!(send.prompt, "original prompt");
    assert_eq!(
        f.f.host
            .store
            .read(|c| crate::classify_cost::spent(c, "program", "r"))
            .unwrap(),
        0.000002
    );
}

#[test]
fn routing_concurrency_wait_deadline_is_unsent_and_free() {
    let f = ModelFixture::new();
    f.routing
        .refusals
        .lock()
        .unwrap()
        .push_back(concurrency_full(Some(1_000_000)));
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(f.wait_model_retry(0), 100_000);
    f.f.clock.set(100_000);
    let result = f.f.terminal();
    assert_eq!(result["status"], "budget_exhausted:wall");
    assert_eq!(result["person_wait_ms"], 0);
    assert_eq!(result["calls"][0]["code"], "resource_busy");
    f.f.assert_never_sent(0, Outcome::Refused);
    assert!(f.wire.fake.sends().is_empty());
    assert_eq!(f.routing.calls.lock().unwrap().len(), 1);
    f.f.host
        .store
        .read(|c| {
            let charged: u64 = c.query_row(
                "SELECT charged FROM codemode_models WHERE run_id='r' AND position=0",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(charged, 0);
            assert_eq!(crate::classify_cost::spent(c, "program", "r")?, 0.0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn routing_concurrency_without_reset_uses_the_existing_provider_backoff() {
    let f = ModelFixture::new();
    f.routing
        .refusals
        .lock()
        .unwrap()
        .push_back(concurrency_full(None));
    f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
    assert_eq!(
        f.wait_model_retry(0),
        f.f.clock.now_ms() + PROVIDER_RECHECK_MS
    );
    f.f.clock.advance(PROVIDER_RECHECK_MS);
    assert_eq!(
        f.finish_usage(
            0,
            "answer",
            Usage {
                input_tokens: Some(1),
                cache_write_tokens: Some(0),
                output_tokens: Some(1),
                ..Usage::default()
            }
        )
        .settlement,
        Settlement::Fulfilled
    );
    assert_eq!(f.wire.fake.sends().len(), 1);
}

#[test]
fn account_model_send_hint_beyond_wall_deadline_keeps_typed_unsent_code() {
    for code in ["account_selection_refused", "account_limit_exhausted"] {
        let f = ModelFixture::new();
        f.wire
            .fake
            .refuse_next_send_with_data(code, json!({"retry_after_s":1e100}));
        f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
        f.wait_model_retry(1);
        f.f.clock.set(100_000);
        let result = f.f.terminal();
        assert_eq!(result["status"], "budget_exhausted:wall");
        assert_eq!(result["person_wait_ms"], 0);
        assert_eq!(result["calls"][0]["code"], code);
        assert!(result["error"]["message"].as_str().unwrap().contains(code));
        f.f.assert_never_sent(0, Outcome::Refused);
        assert_eq!(f.wire.fake.sends().len(), 1);
        assert_eq!(
            f.f.host
                .store
                .read(|c| Ok(c.query_row(
                    "SELECT charged FROM codemode_models WHERE run_id='r' AND position=0",
                    [],
                    |r| r.get::<_, i64>(0)
                )?))
                .unwrap(),
            0
        );
    }
}

#[test]
fn account_model_send_malformed_hint_falls_back_to_provider_backoff() {
    for data in [
        json!({}),
        json!({"retry_after_s":-1}),
        json!({"retry_after_s":"2"}),
    ] {
        let f = ModelFixture::new();
        f.wire
            .fake
            .refuse_next_send_with_data("account_selection_refused", data);
        f.f.issue(0, "model", json!({"prompt":"hello","max_output":1}));
        assert_eq!(
            f.wait_model_retry(1),
            f.f.clock.now_ms() + PROVIDER_RECHECK_MS
        );
        f.f.clock.advance(PROVIDER_RECHECK_MS);
        assert_eq!(
            f.finish_usage(
                0,
                "answer",
                Usage {
                    input_tokens: Some(1),
                    cache_write_tokens: Some(0),
                    output_tokens: Some(1),
                    ..Usage::default()
                }
            )
            .settlement,
            Settlement::Fulfilled
        );
        assert_eq!(f.wire.fake.sends().len(), 2);
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
        json!({"consumer":"basal.codemode","requirements":{"iq":50,"eq":50,"tools":"none","max_output_tokens":1},
        "runners":["broca"],"correlation_id":store::call_key("r",0),"task_ref":"codemode:r"})
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
        ..Default::default()
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
        ..Default::default()
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
        assert_eq!(
            call["requirements"],
            json!({"iq":50,"eq":0,"max_output_tokens":1,"tools":"none"})
        );
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
    for (position, name, input, mut expected) in [
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
        expected["max_output_tokens"] = json!(1);
        expected["tools"] = json!("none");
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
