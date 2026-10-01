use basal_host::broca::{
    BrocaError, BrocaHost, StateStore, Transport,
    fake::{FakeBroca, MemoryStore},
    wire::*,
};
use basal_host::selector::{FakeSelector, ModelSelector, SelectionRequest};
use basal_host::{
    CallRequest, Completion, CompletionAck, CompletionSink, Dispatched, Host, SinkError,
    TokenUsage, UnknownOutcome,
};
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn request(p: Primitive, position: u64, args: Value) -> CallRequest {
    let key = format!("key:{position}");
    let selection = FakeSelector::default()
        .select(&SelectionRequest {
            iq: 40,
            eq: 20,
            flow_id: "f".into(),
            run_id: "r".into(),
            send_id: key.clone(),
        })
        .unwrap();
    CallRequest { flow_id: "f".into(), run_id: "r".into(), position, kind: CallKind::Primitive(p),
        args: JsonText::new(json!({"send_id": key, "work_class": "flow:f", "session": format!("basal:flow-f:r:{position}"), "op": p.name(), "max_output": 12, "request": args, "selection":selection}).to_string()).unwrap(),
        idempotency_key: key, attempt: 1 }
}
fn llm(position: u64) -> CallRequest {
    request(
        Primitive::Llm,
        position,
        json!({"prompt": "hello", "max_output": 999}),
    )
}
fn host(fake: Arc<FakeBroca>, store: Arc<dyn StateStore>) -> BrocaHost {
    BrocaHost::new(
        fake,
        store,
        "/project".into(),
        "basal".into(),
        Arc::new(FakeSelector::default()),
    )
}
#[derive(Default)]
struct Sink {
    completions: Mutex<Vec<Completion>>,
    unknowns: Mutex<Vec<UnknownOutcome>>,
    fail: Mutex<bool>,
    attempts: Mutex<usize>,
}
impl CompletionSink for Sink {
    fn complete(&self, c: &Completion) -> Result<CompletionAck, SinkError> {
        *self.attempts.lock().unwrap() += 1;
        if *self.fail.lock().unwrap() {
            return Err(SinkError("cut".into()));
        }
        let mut completions = self.completions.lock().unwrap();
        if completions.iter().any(|old| old == c) {
            return Ok(CompletionAck::Duplicate);
        }
        completions.push(c.clone());
        Ok(CompletionAck::Accepted)
    }
    fn unknown(&self, u: &UnknownOutcome) -> Result<CompletionAck, SinkError> {
        let mut unknowns = self.unknowns.lock().unwrap();
        if unknowns.iter().any(|old| old == u) {
            return Ok(CompletionAck::Duplicate);
        }
        unknowns.push(u.clone());
        Ok(CompletionAck::Accepted)
    }
}
fn value(c: &Completion) -> Value {
    serde_json::from_str(c.outcome.value.as_str()).unwrap()
}
fn usage() -> Usage {
    Usage {
        input_tokens: Some(7),
        cache_write_tokens: Some(2),
        output_tokens: Some(5),
        cached_input_tokens: Some(100),
        reasoning_tokens: Some(3),
    }
}

#[test]
fn exact_send_contract_and_parallel_sessions() {
    let fake = Arc::new(FakeBroca::default());
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    assert_eq!(
        h.dispatch_model(&llm(0)).unwrap(),
        Dispatched::Accepted {
            handle: "run:key:0".into()
        }
    );
    h.dispatch_model(&llm(1)).unwrap();
    let sends = fake.sends();
    assert_eq!(
        sends[0].0,
        basal_host::broca::Route {
            project_root: "/project".into(),
            harness: "basal".into(),
            session: "basal:flow-f:r:0".into()
        }
    );
    assert_eq!(sends[1].0.session, "basal:flow-f:r:1");
    assert_ne!(sends[0].0.session, sends[1].0.session);
    let expected = json!({"prompt":"hello","system":null,"send_id":"key:0","model":{"provider":"fake","model":"test"},"tools":[],"tool_choice":{"type":"none"},"generation":{"max_output_tokens":12},"stop_when":[],"cache":null,"work_class":"flow:f","append_episode":false});
    assert_eq!(
        serde_json::from_slice::<Value>(&sends[0].1).unwrap(),
        expected
    );
    assert!(expected.get("session").is_none());
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    fake.finish("key:1", "second", RunFinishReason::Completed, None)
        .unwrap();
    h.poll().unwrap();
    fake.finish("key:0", "first", RunFinishReason::Completed, None)
        .unwrap();
    h.poll().unwrap();
    let completions = sink.completions.lock().unwrap();
    assert_eq!(
        completions.iter().map(|c| c.position).collect::<Vec<_>>(),
        vec![1, 0]
    );
    assert_eq!(value(&completions[0]), json!({"text":"second"}));
    assert_eq!(value(&completions[1]), json!({"text":"first"}));
}

#[test]
fn reissue_after_cut_is_byte_identical_and_finished_is_immediate() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    fake.cut_next_reply();
    assert!(matches!(
        host(fake.clone(), store.clone()).dispatch_model(&llm(0)),
        Err(BrocaError::Unavailable {
            proven_unsent: false,
            ..
        })
    ));
    let h = host(fake.clone(), store);
    assert_eq!(
        h.dispatch_model(&llm(0)).unwrap(),
        Dispatched::Accepted {
            handle: "run:key:0".into()
        }
    );
    let sends = fake.sends();
    assert_eq!(sends[0], sends[1]);
    fake.finish("key:0", "done", RunFinishReason::Completed, Some(usage()))
        .unwrap();
    let Dispatched::Completed(outcome) = h.dispatch_model(&llm(0)).unwrap() else {
        panic!("Finished did not complete")
    };
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap(),
        json!({"text":"done"})
    );
    assert_eq!(outcome.usage.unwrap().output_tokens, Some(5));
    assert_eq!(fake.sends()[0], fake.sends()[2]);
}

#[test]
fn pending_submission_keeps_its_handle_and_usage_is_metadata() {
    let fake = Arc::new(FakeBroca::default());
    fake.pending_next();
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    assert_eq!(
        h.dispatch_model(&llm(0)).unwrap(),
        Dispatched::Accepted {
            handle: "submission:key:0".into()
        }
    );
    fake.finish("key:0", "answer", RunFinishReason::Completed, Some(usage()))
        .unwrap();
    h.poll().unwrap();
    let completions = sink.completions.lock().unwrap();
    assert_eq!(completions[0].handle, "submission:key:0");
    assert_eq!(value(&completions[0]), json!({"text":"answer"}));
    assert_eq!(
        completions[0].outcome.usage,
        Some(TokenUsage {
            input_tokens: Some(7),
            cache_write_tokens: Some(2),
            output_tokens: Some(5),
            cached_input_tokens: Some(100)
        })
    );
}

#[test]
fn usage_absent_empty_and_zero_remain_distinct() {
    for (index, report) in [
        None,
        Some(Usage::default()),
        Some(Usage {
            input_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(0),
            cached_input_tokens: Some(0),
            reasoning_tokens: Some(0),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let fake = Arc::new(FakeBroca::default());
        let h = host(fake.clone(), Arc::new(MemoryStore::default()));
        let sink = Arc::new(Sink::default());
        h.attach(sink.clone());
        h.dispatch_model(&llm(0)).unwrap();
        fake.finish("key:0", "zero", RunFinishReason::Completed, report)
            .unwrap();
        h.poll().unwrap();
        let c = sink.completions.lock().unwrap();
        match index {
            0 => assert_eq!(c[0].outcome.usage, None),
            1 => assert_eq!(c[0].outcome.usage, Some(TokenUsage::default())),
            _ => assert_eq!(c[0].outcome.usage.unwrap().input_tokens, Some(0)),
        }
        assert!(value(&c[0]).get("usage").is_none());
    }
}

fn status(state: &str) -> RunStatusResponse {
    serde_json::from_value(json!({"state": state, "usage": {"input_tokens":0,"output_tokens":0,"cache_write_tokens":0,"cached_input_tokens":0}})).unwrap()
}

#[test]
fn every_run_result_state_maps_to_its_outcome_and_nonterminals_wait() {
    for state in [
        "completed",
        "error",
        "cancelled",
        "interrupted",
        "max_steps",
        "transform_unavailable",
        "active",
        "paused",
    ] {
        let fake = Arc::new(FakeBroca::default());
        let h = host(fake.clone(), Arc::new(MemoryStore::default()));
        let sink = Arc::new(Sink::default());
        h.attach(sink.clone());
        h.dispatch_model(&llm(0)).unwrap();
        h.poll().unwrap();
        let error = ProviderError {
            class: "auth_required".into(),
            message: "provider login expired".into(),
            rest: Default::default(),
        };
        // run.status reports a terminal state (with zero usage) even for the
        // active and paused cases, so a call that keeps waiting proves the
        // decision is taken from run.result alone.
        fake.set(
            "key:0",
            state,
            "last",
            Some(error),
            Some("auth_required".into()),
            status(if ["active", "paused"].contains(&state) {
                "completed"
            } else {
                state
            }),
        )
        .unwrap();
        h.poll().unwrap();
        let c = sink.completions.lock().unwrap();
        if ["active", "paused"].contains(&state) {
            assert!(c.is_empty(), "{state}");
            assert!(sink.unknowns.lock().unwrap().is_empty());
            continue;
        }
        assert_eq!(c.len(), 1, "{state}");
        assert_eq!(c[0].outcome.usage.unwrap().input_tokens, Some(0));
        match state {
            "completed" => {
                assert_eq!(c[0].outcome.settlement, Settlement::Fulfilled);
                assert_eq!(value(&c[0]), json!({"text":"last"}));
            }
            "error" => {
                assert_eq!(c[0].outcome.settlement, Settlement::Rejected);
                assert_eq!(
                    value(&c[0]),
                    json!({"code":"error","class":"auth_required","message":"provider login expired"})
                );
            }
            _ => {
                assert_eq!(c[0].outcome.settlement, Settlement::Rejected);
                assert_eq!(value(&c[0])["code"], state);
            }
        }
    }
}

#[test]
fn a_completed_run_without_text_fulfils_with_empty_text() {
    let fake = Arc::new(FakeBroca::default());
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.finish("key:0", "", RunFinishReason::Completed, Some(usage()))
        .unwrap();
    h.poll().unwrap();
    let c = sink.completions.lock().unwrap();
    assert_eq!(c[0].outcome.settlement, Settlement::Fulfilled);
    assert_eq!(value(&c[0]), json!({"text":""}));
}

#[test]
fn unknown_run_for_an_accepted_call_is_reported_as_unknown_not_rejected() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let h = host(fake.clone(), store.clone());
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.forget("key:0").unwrap();
    h.poll().unwrap();
    assert!(sink.completions.lock().unwrap().is_empty());
    let unknowns = sink.unknowns.lock().unwrap().clone();
    assert_eq!(unknowns.len(), 1);
    assert_eq!(unknowns[0].handle, "run:key:0");
    assert_eq!(
        (unknowns[0].run_id.as_str(), unknowns[0].position),
        ("r", 0)
    );
    assert!(
        unknowns[0].detail.contains("run:key:0") && unknowns[0].detail.contains("unknown_run"),
        "{}",
        unknowns[0].detail
    );
    let saved = store.load().unwrap()[0].clone();
    assert!(saved.acknowledged && saved.outcome.is_none());
    // Settled: neither a later poll nor a restart asks Broca again.
    let calls = fake.calls();
    h.poll().unwrap();
    drop(h);
    host(fake.clone(), store).attach(sink.clone());
    assert_eq!(fake.calls(), calls);
    assert_eq!(sink.unknowns.lock().unwrap().len(), 1);
}

#[test]
fn a_missed_run_finished_is_recovered_on_reconnect_even_when_archived() {
    for archived in [false, true] {
        let fake = Arc::new(FakeBroca::default());
        let store = Arc::new(MemoryStore::default());
        let h = host(fake.clone(), store.clone());
        let sink = Arc::new(Sink::default());
        h.attach(sink.clone());
        h.dispatch_model(&llm(0)).unwrap();
        h.poll().unwrap();
        assert_eq!(fake.watches().len(), 1);
        fake.drop_finish("key:0").unwrap();
        fake.finish(
            "key:0",
            "from run.result",
            RunFinishReason::Completed,
            Some(usage()),
        )
        .unwrap();
        if archived {
            fake.archive("key:0").unwrap();
        }
        // The finish never reached the subscription, so nothing woke the
        // host and nothing was delivered.
        assert_eq!(fake.wakes(), 0);
        assert!(sink.completions.lock().unwrap().is_empty());
        // A reconnect polls, which reads run.result for the pending call.
        h.poll().unwrap();
        let c = sink.completions.lock().unwrap();
        assert_eq!(c.len(), 1, "archived: {archived}");
        assert_eq!(value(&c[0]), json!({"text":"from run.result"}));
        assert_eq!(c[0].outcome.usage.unwrap().input_tokens, Some(7));
    }
}

#[test]
fn a_finished_run_wakes_through_the_watch_opened_at_the_live_head() {
    let fake = Arc::new(FakeBroca::default());
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    h.dispatch_model(&llm(0)).unwrap();
    h.poll().unwrap();
    assert_eq!(fake.watches().len(), 1);
    fake.finish("key:0", "woken", RunFinishReason::Completed, None)
        .unwrap();
    assert_eq!(fake.wakes(), 1);
    h.poll().unwrap();
    assert_eq!(
        value(&sink.completions.lock().unwrap()[0]),
        json!({"text":"woken"})
    );
}

#[test]
fn a_module_restart_with_calls_in_flight_delivers_each_exactly_once() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let sink = Arc::new(Sink::default());
    let h = host(fake.clone(), store.clone());
    h.attach(sink.clone());
    for position in 0..3 {
        h.dispatch_model(&llm(position)).unwrap();
    }
    h.poll().unwrap();
    drop(h);
    // Two calls end while no process watches them.
    fake.finish("key:0", "zero", RunFinishReason::Completed, None)
        .unwrap();
    fake.finish("key:2", "two", RunFinishReason::Cancelled, None)
        .unwrap();
    let h = host(fake.clone(), store.clone());
    h.attach(sink.clone());
    {
        let c = sink.completions.lock().unwrap();
        assert_eq!(c.iter().map(|c| c.position).collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(value(&c[0]), json!({"text":"zero"}));
        assert_eq!(value(&c[1])["code"], "cancelled");
    }
    fake.finish("key:1", "one", RunFinishReason::Completed, None)
        .unwrap();
    h.poll().unwrap();
    drop(h);
    host(fake, store).attach(sink.clone());
    assert_eq!(sink.completions.lock().unwrap().len(), 3);
    assert_eq!(*sink.attempts.lock().unwrap(), 3);
}

#[test]
fn a_status_that_never_catches_up_charges_the_reservation_after_a_bounded_wait() {
    use basal_host::broca::STATUS_LAG_POLLS;
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let sink = Arc::new(Sink::default());
    let h = host(fake.clone(), store.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.finish("key:0", "stuck", RunFinishReason::Completed, Some(usage()))
        .unwrap();
    fake.set_status("key:0", RunStatusResponse::Active).unwrap();
    // The count lives in memory: a restart part-way starts it again.
    for _ in 1..STATUS_LAG_POLLS {
        assert!(matches!(h.poll(), Err(BrocaError::Unavailable { .. })));
    }
    drop(h);
    let h = host(fake.clone(), store.clone());
    for _ in 1..STATUS_LAG_POLLS {
        assert!(matches!(h.poll(), Err(BrocaError::Unavailable { .. })));
    }
    assert!(store.load().unwrap()[0].outcome.is_none());
    h.attach(sink.clone());
    let c = sink.completions.lock().unwrap();
    assert_eq!(c.len(), 1, "the bounded wait ends at the last poll");
    assert_eq!(value(&c[0]), json!({"text":"stuck"}));
    // No usage reaches the ledger, so it charges the whole reservation.
    assert_eq!(c[0].outcome.usage, None);
}

#[test]
fn usage_comes_from_run_status_once_and_waits_while_status_lags() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let h = host(fake.clone(), store.clone());
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.finish(
        "key:0",
        "lagging",
        RunFinishReason::Completed,
        Some(usage()),
    )
    .unwrap();
    // run.result already reads the durable end while the live actor still
    // calls the run active: the outcome waits for the usage.
    fake.set_status("key:0", RunStatusResponse::Active).unwrap();
    assert!(matches!(h.poll(), Err(BrocaError::Unavailable { .. })));
    assert!(sink.completions.lock().unwrap().is_empty());
    assert!(store.load().unwrap()[0].outcome.is_none());
    fake.set_status(
        "key:0",
        RunStatusResponse::Completed {
            metadata: Terminal {
                usage: Some(usage()),
                final_step_finish_reason: Some("stop".into()),
                ..Terminal::default()
            },
        },
    )
    .unwrap();
    h.poll().unwrap();
    let calls = fake.calls();
    h.poll().unwrap();
    assert_eq!(fake.calls(), calls, "a settled call is not read again");
    let c = sink.completions.lock().unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(
        value(&c[0]),
        json!({"text":"lagging","finish_reason":"stop"})
    );
    assert_eq!(
        c[0].outcome.usage,
        Some(TokenUsage {
            input_tokens: Some(7),
            cache_write_tokens: Some(2),
            output_tokens: Some(5),
            cached_input_tokens: Some(100)
        })
    );
}

#[test]
fn classify_contract_is_exact_not_trimmed_or_parsed() {
    for text in ["yes", " yes", "yes\n", "\"yes\"", "maybe"] {
        let fake = Arc::new(FakeBroca::default());
        let h = host(fake.clone(), Arc::new(MemoryStore::default()));
        let sink = Arc::new(Sink::default());
        h.attach(sink.clone());
        h.dispatch_model(&request(
            Primitive::Classify,
            0,
            json!({"text":"outside \"data\"", "labels":["yes","no"]}),
        ))
        .unwrap();
        let send: SendParams = serde_json::from_slice(&fake.sends()[0].1).unwrap();
        assert_eq!(
            send.prompt,
            "{\"labels\":[\"yes\",\"no\"],\"text\":\"outside \\\"data\\\"\"}"
        );
        assert_eq!(
            send.system.as_deref(),
            Some(
                "Classify the text in the JSON object. Return exactly one label from labels, verbatim, with no added quotes, whitespace, explanation or other text."
            )
        );
        fake.finish("key:0", text, RunFinishReason::Completed, Some(usage()))
            .unwrap();
        h.poll().unwrap();
        let c = sink.completions.lock().unwrap();
        if text == "yes" {
            assert_eq!(value(&c[0]), json!("yes"));
        } else {
            assert_eq!(value(&c[0])["code"], "classify_invalid");
            assert_eq!(c[0].outcome.settlement, Settlement::Rejected);
        }
    }
}

#[test]
fn fake_refuses_send_id_reuse_and_restarts_with_same_run() {
    let path = std::env::temp_dir().join(format!("basal-broca-fake-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let fake = Arc::new(FakeBroca::persistent(&path).unwrap());
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    h.dispatch_model(&llm(0)).unwrap();
    let (route, bytes) = fake.sends()[0].clone();
    drop(h);
    drop(fake);
    let fake = FakeBroca::persistent(&path).unwrap();
    assert_eq!(
        fake.send(&route, &bytes).unwrap(),
        SendResult::Active {
            run_id: "run:key:0".into()
        }
    );
    let mut params: SendParams = serde_json::from_slice(&bytes).unwrap();
    params.prompt = "changed".into();
    assert!(
        matches!(fake.send(&route, &serde_json::to_vec(&params).unwrap()), Err(BrocaError::Refused { code, .. }) if code == "send_id_reuse")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn sink_failure_replays_a_durable_completion() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let h = host(fake.clone(), store.clone());
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.finish("key:0", "retry", RunFinishReason::Completed, None)
        .unwrap();
    *sink.fail.lock().unwrap() = true;
    assert!(matches!(h.poll(), Err(BrocaError::Sink(_))));
    drop(h);
    *sink.fail.lock().unwrap() = false;
    host(fake, store).attach(sink.clone());
    assert_eq!(sink.completions.lock().unwrap().len(), 1);
}

#[test]
fn malformed_inputs_and_unknown_kinds_are_typed_errors() {
    let fake = Arc::new(FakeBroca::default());
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    let mut r = llm(0);
    r.kind = CallKind::Primitive(Primitive::Facts);
    assert_eq!(h.dispatch_model(&r), Err(BrocaError::UnsupportedKind));
    r.kind = CallKind::Primitive(Primitive::Llm);
    r.args = JsonText::new("{bad").unwrap();
    assert!(matches!(h.dispatch_model(&r), Err(BrocaError::Invalid(_))));
    let r = request(Primitive::Llm, 0, json!({"prompt":"hi", "tools":[]}));
    assert!(matches!(h.dispatch_model(&r), Err(BrocaError::Invalid(_))));
    assert!(fake.sends().is_empty());
}

#[test]
fn wire_fixtures_pin_run_result_status_and_stream_shapes() {
    // run.result replies and params, in the JSON Broca's broca-wire crate
    // (role.rs) produces.
    let completed = json!({"run_id":"x","state":"completed","final_message":{"ordinal":3,"mid":"m3","text":""}});
    let decoded: RunResultResponse = serde_json::from_value(completed.clone()).unwrap();
    assert_eq!(decoded.final_message.as_ref().unwrap().text, "");
    assert_eq!(serde_json::to_value(decoded).unwrap(), completed);
    let error = json!({"run_id":"x","state":"error","error":{"class":"auth_required","message":"log in","status":401,"provider_code":"expired"}});
    let decoded: RunResultResponse = serde_json::from_value(error.clone()).unwrap();
    assert_eq!(decoded.error.as_ref().unwrap().class, "auth_required");
    assert!(decoded.final_message.is_none());
    assert_eq!(serde_json::to_value(decoded).unwrap(), error);
    let paused = json!({"run_id":"x","state":"paused","reason":"auth_required"});
    let decoded: RunResultResponse = serde_json::from_value(paused.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), paused);
    assert_eq!(
        serde_json::to_value(RunResultParams { run_id: "x".into() }).unwrap(),
        json!({"run_id":"x"})
    );
    assert!(serde_json::from_value::<RunResultParams>(json!({"run_id":"x","extra":1})).is_err());
    assert_eq!(
        serde_json::to_value(StatusParams {
            run_id: Some("x".into())
        })
        .unwrap(),
        json!({"run_id":"x"})
    );
    assert_eq!(
        serde_json::to_value(SubscribeParams::live()).unwrap(),
        json!({"from":"live"})
    );
    // A run_finished control event decodes (its cursor is ignored); an
    // assistant message decodes as an ignored unit, never as text.
    let finished: SubscribeEvent = serde_json::from_value(json!({"kind":"control","cursor":{"wal_seq":8,"sub_index":2},"unit":{"type":"run_finished","run_id":"x","reason":"completed","usage":{"output_tokens":5},"retries_used":0}})).unwrap();
    assert!(
        matches!(finished, SubscribeEvent::Control { unit } if matches!(*unit, ControlUnit::RunFinished { run_id: Some(ref id) } if id == "x"))
    );
    let message: SubscribeEvent = serde_json::from_value(json!({"kind":"control","cursor":{"wal_seq":8,"sub_index":1},"unit":{"type":"assistant_message","message":{"message_id":"m","content":[]}}})).unwrap();
    assert!(
        matches!(message, SubscribeEvent::Control { unit } if matches!(*unit, ControlUnit::Other))
    );
}

#[test]
fn model_outcomes_report_the_selected_decision_best_effort() {
    use basal_host::selector::{FakeSelector, ModelOutcome, SelectionError};
    let fake = Arc::new(FakeBroca::default());
    let selector = Arc::new(FakeSelector::default());
    let store = Arc::new(MemoryStore::default());
    let h = BrocaHost::new(
        fake.clone(),
        store,
        "/project".into(),
        "basal".into(),
        selector.clone(),
    );
    *selector.report_error.lock().unwrap() = Some(SelectionError::Unavailable {
        detail: "router disconnected".into(),
    });
    for (position, reason, expected) in [
        (0, RunFinishReason::Completed, ModelOutcome::Completed),
        (1, RunFinishReason::Error, ModelOutcome::Error),
        (2, RunFinishReason::Cancelled, ModelOutcome::Cancelled),
        (3, RunFinishReason::Interrupted, ModelOutcome::Interrupted),
        (4, RunFinishReason::MaxSteps, ModelOutcome::Error),
        (
            5,
            RunFinishReason::TransformUnavailable,
            ModelOutcome::Error,
        ),
    ] {
        h.dispatch_model(&llm(position)).unwrap();
        fake.finish(&format!("key:{position}"), "answer", reason, None)
            .unwrap();
        h.poll().unwrap();
        h.poll().unwrap();
        assert_eq!(
            selector.reports.lock().unwrap().len(),
            position as usize + 1
        );
        assert_eq!(
            selector.reports.lock().unwrap()[position as usize],
            (format!("decision:key:{position}"), expected)
        );
    }
}
#[test]
fn broker_rejects_script_model_choices_even_with_a_valid_journaled_selection() {
    let fake = Arc::new(FakeBroca::default());
    let h = host(fake.clone(), Arc::new(MemoryStore::default()));
    for (position, options) in [
        json!({"prompt":"x","model":{"provider":"fake","model":"test"}}),
        json!({"prompt":"x","provider":"fake"}),
        json!({"text":"x","labels":["a"],"model":null}),
    ]
    .into_iter()
    .enumerate()
    {
        let p = if position == 2 {
            Primitive::Classify
        } else {
            Primitive::Llm
        };
        let dispatched = h.dispatch(&request(p, position as u64, options)).unwrap();
        let Dispatched::Completed(outcome) = dispatched else {
            panic!("must reject before sending")
        };
        assert_eq!(outcome.settlement, Settlement::Rejected);
        assert_eq!(
            serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
            "model_not_allowed"
        );
    }
    assert!(fake.sends().is_empty());
}
