use basal_host::broca::{
    BrocaError, BrocaHost, StateStore, Transport,
    fake::{FakeBroca, MemoryStore},
    wire::*,
};
use basal_host::{
    CallRequest, Completion, CompletionAck, CompletionSink, Dispatched, Host, SinkError, TokenUsage,
};
use basal_proto::{CallKind, JsonText, Primitive, Settlement};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

fn model() -> ModelParams {
    ModelParams {
        provider: "fake".into(),
        model: "test".into(),
        variant: None,
    }
}
fn request(p: Primitive, position: u64, args: Value) -> CallRequest {
    let key = format!("key:{position}");
    CallRequest { flow_id: "f".into(), run_id: "r".into(), position, kind: CallKind::Primitive(p),
        args: JsonText::new(json!({"send_id": key, "work_class": "flow:f", "session": format!("basal:flow-f:r:{position}"), "op": p.name(), "max_output": 12, "request": args}).to_string()).unwrap(),
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
    BrocaHost::new(fake, store, "/project".into(), "basal".into(), model())
}
#[derive(Default)]
struct Sink {
    completions: Mutex<Vec<Completion>>,
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

#[test]
fn durable_cursor_resumes_text_without_loss_or_duplicate() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let sink = Arc::new(Sink::default());
    let h = host(fake.clone(), store.clone());
    h.attach(sink.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.assistant(
        "key:0",
        vec![ContentBlock::Text {
            text: "buffered".into(),
        }],
    )
    .unwrap();
    h.poll().unwrap();
    let saved = store.load().unwrap()[0].clone();
    assert_eq!(saved.text.as_deref(), Some("buffered"));
    assert!(saved.cursor.is_some());
    drop(h);
    let h = host(fake.clone(), store.clone());
    h.attach(sink.clone());
    assert_eq!(
        fake.subscriptions().last().unwrap().1.from,
        saved.cursor.map(FromWire::Cursor)
    );
    fake.finish("key:0", "final", RunFinishReason::Completed, None)
        .unwrap();
    fake.configure("key:0", false, false, true, None).unwrap();
    h.poll().unwrap();
    h.poll().unwrap();
    assert_eq!(sink.completions.lock().unwrap().len(), 1);
    assert_eq!(
        value(&sink.completions.lock().unwrap()[0]),
        json!({"text":"final"})
    );
    drop(h);
    host(fake, store).attach(sink.clone());
    assert_eq!(sink.completions.lock().unwrap().len(), 1);
    assert_eq!(*sink.attempts.lock().unwrap(), 1);
}

struct CutStore {
    inner: MemoryStore,
    cut: Mutex<bool>,
}
impl StateStore for CutStore {
    fn load(&self) -> Result<Vec<basal_host::broca::StoredCall>, BrocaError> {
        self.inner.load()
    }
    fn save(&self, call: &basal_host::broca::StoredCall) -> Result<(), BrocaError> {
        if call.text.is_some() && *self.cut.lock().unwrap() {
            return Err(BrocaError::Store("cut between buffering and save".into()));
        }
        self.inner.save(call)
    }
}
#[test]
fn cursor_and_text_commit_together_at_a_cut() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(CutStore {
        inner: MemoryStore::default(),
        cut: Mutex::new(true),
    });
    let h = host(fake.clone(), store.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.assistant(
        "key:0",
        vec![ContentBlock::Text {
            text: "held".into(),
        }],
    )
    .unwrap();
    assert!(matches!(h.poll(), Err(BrocaError::Store(_))));
    let saved = store.load().unwrap()[0].clone();
    assert_eq!(saved.text, None);
    assert_eq!(
        saved.cursor,
        Some(Cursor {
            wal_seq: 1,
            sub_index: 0
        })
    );
    *store.cut.lock().unwrap() = false;
    drop(h);
    let h = host(fake.clone(), store.clone());
    h.poll().unwrap();
    let saved = store.load().unwrap()[0].clone();
    assert_eq!(saved.text.as_deref(), Some("held"));
    assert_eq!(
        saved.cursor,
        Some(Cursor {
            wal_seq: 1,
            sub_index: 1
        })
    );
    fake.finish("key:0", "held", RunFinishReason::Completed, None)
        .unwrap();
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    assert_eq!(
        value(&sink.completions.lock().unwrap()[0]),
        json!({"text":"held"})
    );
}

#[test]
fn status_recovers_missed_finish_and_archived_read_recovers_text() {
    for archived in [false, true] {
        let fake = Arc::new(FakeBroca::default());
        let store = Arc::new(MemoryStore::default());
        let h = host(fake.clone(), store.clone());
        h.dispatch_model(&llm(0)).unwrap();
        h.poll().unwrap();
        drop(h);
        fake.finish(
            "key:0",
            "from history",
            RunFinishReason::Completed,
            Some(usage()),
        )
        .unwrap();
        fake.configure("key:0", archived, true, false, None)
            .unwrap();
        fake.forget_stream("key:0").unwrap();
        let sink = Arc::new(Sink::default());
        let h = host(fake.clone(), store);
        h.attach(sink.clone());
        assert_eq!(
            value(&sink.completions.lock().unwrap()[0]),
            json!({"text":"from history"})
        );
        assert_eq!(
            sink.completions.lock().unwrap()[0]
                .outcome
                .usage
                .unwrap()
                .input_tokens,
            Some(7)
        );
        assert!(fake.calls().0 > 0);
        assert!(fake.calls().1 > 0);
    }
}

#[test]
fn read_refusal_is_journalable_with_status_usage() {
    let fake = Arc::new(FakeBroca::default());
    let store = Arc::new(MemoryStore::default());
    let h = host(fake.clone(), store.clone());
    h.dispatch_model(&llm(0)).unwrap();
    fake.finish(
        "key:0",
        "not readable",
        RunFinishReason::Completed,
        Some(usage()),
    )
    .unwrap();
    fake.configure("key:0", true, true, false, Some("access_denied".into()))
        .unwrap();
    let sink = Arc::new(Sink::default());
    h.attach(sink.clone());
    let c = sink.completions.lock().unwrap();
    assert_eq!(c[0].outcome.settlement, Settlement::Rejected);
    assert_eq!(value(&c[0])["code"], "access_denied");
    assert_eq!(c[0].outcome.usage.unwrap().output_tokens, Some(5));
}

#[test]
fn every_status_terminal_rejects_or_completes_and_nonterminals_wait() {
    for state in [
        "completed",
        "cancelled",
        "error",
        "max_steps",
        "transform_unavailable",
        "interrupted",
        "active",
        "paused",
        "unknown",
    ] {
        let fake = Arc::new(FakeBroca::default());
        let h = host(fake.clone(), Arc::new(MemoryStore::default()));
        let sink = Arc::new(Sink::default());
        h.attach(sink.clone());
        h.dispatch_model(&llm(0)).unwrap();
        h.poll().unwrap();
        fake.assistant(
            "key:0",
            vec![ContentBlock::Text {
                text: "last".into(),
            }],
        )
        .unwrap();
        let status: RunStatusResponse = serde_json::from_value(json!({"state": state, "usage": {"input_tokens":0,"output_tokens":0,"cache_write_tokens":0,"cached_input_tokens":0}})).unwrap();
        fake.set_status("key:0", status).unwrap();
        h.poll().unwrap();
        let c = sink.completions.lock().unwrap();
        if ["active", "paused", "unknown"].contains(&state) {
            assert!(c.is_empty(), "{state}");
        } else {
            assert_eq!(c.len(), 1);
            assert_eq!(c[0].outcome.usage.unwrap().input_tokens, Some(0));
            if state == "completed" {
                assert_eq!(value(&c[0]), json!({"text":"last"}));
            } else {
                assert_eq!(c[0].outcome.settlement, Settlement::Rejected);
                assert_eq!(value(&c[0])["code"], state);
            }
        }
    }
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
fn wire_fixtures_pin_control_read_and_status_shapes() {
    let fixture = json!({"kind":"control","cursor":{"wal_seq":8,"sub_index":2},"unit":{"type":"run_finished","run_id":"x","reason":"completed","usage":{"input_tokens":0,"reasoning_tokens":3,"output_tokens":5},"retries_used":0}});
    let event: SubscribeEvent = serde_json::from_value(fixture.clone()).unwrap();
    assert_eq!(serde_json::to_value(event).unwrap(), fixture);
    let page = json!({"messages":[{"ordinal":1,"mid":"m","message":{"role":"assistant","content":[{"type":"text","text":"answer"},{"type":"reasoning","text":"hidden"}]}}],"head":{"wal_seq":8,"sub_index":1},"lineage_state":{"last_run_id":"x","state":"completed","usage":{}}});
    let decoded: SessionReadResponse = serde_json::from_value(page.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), page);
    assert_eq!(
        serde_json::to_value(StatusParams {
            run_id: Some("x".into())
        })
        .unwrap(),
        json!({"run_id":"x"})
    );
    assert_eq!(
        serde_json::to_value(SubscribeParams {
            from: Some(FromWire::Named("start".into()))
        })
        .unwrap(),
        json!({"from":"start"})
    );
    assert_eq!(
        serde_json::to_value(SubscribeParams {
            from: Some(FromWire::Cursor(Cursor {
                wal_seq: 8,
                sub_index: 2
            }))
        })
        .unwrap(),
        json!({"from":{"wal_seq":8,"sub_index":2}})
    );
}
