use super::*;
use cortexkit_role_tool_provider::late_results::LateEntry;
use cortexkit_role_tool_provider::result::ResultFormat;

#[test]
fn legacy_content_members_remain_opaque_in_direct_and_late_results() {
    let body = json!({"path":"a.txt","content":"hello","held":true});
    let f = Fixture::new();
    f.standard();
    f.issue(0, "read", json!({}));
    f.attempt().reply.send(Ok(body.clone())).unwrap();
    let delivered = f.delivery();
    assert_eq!(delivered.settlement, Settlement::Fulfilled);
    assert_eq!(
        serde_json::from_str::<Value>(delivered.value.as_str()).unwrap(),
        body
    );
    let entry = LateEntry::new(
        "result",
        "reserved:core",
        "scope-r",
        7,
        "reserved:basal",
        "key",
        "e",
        100,
    )
    .with_result(body.clone());
    let Some(held::Update::Final(mapped)) = held::map_entry(entry, None) else {
        panic!("expected final result")
    };
    assert_eq!(mapped.outcome, Outcome::Ok);
    assert_eq!(
        serde_json::from_str::<Value>(mapped.value.unwrap().as_str()).unwrap(),
        body
    );
    f.finish("null");
    assert_eq!(f.terminal()["status"], "completed");
}

#[test]
fn published_held_reasons_and_unknown_reasons_all_remain_interim() {
    for reason in [
        "consent",
        "login_handoff",
        "login_capture",
        "running",
        "future_person_action",
    ] {
        let body = json!({"content":[{"type":"text","text":"Waiting"}],"held":{"reason":reason}});
        assert!(matches!(
            held::classify(body, Some(&ResultFormat::ToolResultV1)),
            held::Reply::Held { .. }
        ));
    }
    for body in [
        json!({"content":[{"type":"text","text":"bad"}],"held":{"reason":"Not Snake Case"}}),
        json!({"content":[{"type":"text","text":"bad"}],"held":{"reason":"consent"},"isError":true,"errorCode":"bad_reply"}),
    ] {
        assert!(matches!(
            held::classify(body, Some(&ResultFormat::ToolResultV1)),
            held::Reply::Final(Err(_))
        ));
    }
}

#[test]
fn malformed_held_detail_is_rejected_by_the_shared_decoder() {
    for detail in [json!(null), json!("text"), json!([]), json!(1)] {
        let body = json!({"content":[{"type":"text","text":"Waiting"}],"held":{"reason":"consent","detail":detail}});
        assert!(
            matches!(
                held::classify(body, Some(&ResultFormat::ToolResultV1)),
                held::Reply::Final(Err(_))
            ),
            "non-object detail must not become held"
        );
    }
}

#[test]
fn transient_pull_failure_keeps_the_run_live_and_withdrawable() {
    let f = Fixture::new();
    let (_, pull) = f.provider_held();
    pull.reply
        .send(Err(WireError::TimedOut("slow provider".into())))
        .unwrap();
    let retry = f.custody_attempt("late_results");
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "running"
    );
    let supervisor = f.supervisor.clone();
    let cancel = spawn_bounded(move || supervisor.cancel("r").unwrap());
    let withdraw = f.custody_attempt("tool.withdraw");
    assert!(
        !f.supervisor.0.custody.has_held("codemode:r", "notes"),
        "ending detaches custody before network cleanup"
    );
    withdraw
        .reply
        .send(Ok(json!({"answer":"withdrawn"})))
        .unwrap();
    cancel.join("cancel of the run holding a call");
    assert_eq!(f.calls()[0].outcome, Outcome::Withdrawn);
    drop(retry);
}

#[test]
fn final_run_completion_waits_for_a_successful_late_ack() {
    let f = Fixture::new();
    let parked = f.parked();
    let (call, pull) = f.provider_held();
    pull.reply
        .send(Ok(late_page(
            "i1",
            1,
            vec![late_entry(
                &call.key,
                "e",
                json!({"content":[{"type":"text","text":"done"}],"structuredContent":{"n":1}}),
            )],
        )))
        .unwrap();
    f.delivery();
    let ack = f.custody_attempt("late_results.ack");
    f.finish("1");
    // The driver has taken the program's result but must keep waiting while
    // the page is unacknowledged. Wait for it to block in exactly that state.
    loop {
        let report = parked
            .recv_timeout(TIMEOUT)
            .expect("the driver did not wait for the page acknowledgement");
        if report.completion_pending {
            break;
        }
    }
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "running",
        "the run route must survive until page acknowledgement"
    );
    ack.reply
        .send(Err(WireError::TimedOut("lost ack".into())))
        .unwrap();
    let retry = f.custody_attempt("late_results.ack");
    assert_eq!(
        retry.input["through"],
        json!({"provider_incarnation":"i1","seq":1})
    );
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "running"
    );
    retry.reply.send(Ok(json!({}))).unwrap();
    assert_eq!(f.terminal()["status"], "completed");
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
}

#[test]
fn progress_and_future_entries_before_the_result_do_not_settle_the_call() {
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    let result = late_entry(
        &call.key,
        "final",
        json!({"content":[{"type":"text","text":"done"}],"structuredContent":{"n":42}}),
    );
    let mut progress = result.clone();
    progress["kind"] = json!("progress");
    progress.as_object_mut().unwrap().remove("result");
    progress["text"] = json!("Still working");
    let mut future = progress.clone();
    future["kind"] = json!("future_update");
    // Reusing an event ID must not let a non-result hide the following result.
    pull.reply
        .send(Ok(late_page("i1", 3, vec![progress, future, result])))
        .unwrap();
    let delivery = f.delivery();
    assert_eq!(delivery.settlement, Settlement::Fulfilled);
    assert_eq!(
        serde_json::from_str::<Value>(delivery.value.as_str()).unwrap(),
        json!({"n":42})
    );
    let ack = f.custody_attempt("late_results.ack");
    assert_eq!(
        ack.input["through"],
        json!({"provider_incarnation":"i1","seq":3})
    );
    ack.reply.send(Ok(json!({}))).unwrap();
    f.finish("42");
    assert_eq!(f.terminal()["status"], "completed");
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
    assert!(f.attempts.try_recv().is_err());
}

fn early_late(kind: &str) {
    let f = Fixture::new();
    f.host.content_results.store(true, Ordering::SeqCst);
    f.standard();
    f.issue(0, "read", json!({}));
    let attempt = f.attempt();
    let mut raw = late_entry(
        &attempt.key,
        "early",
        json!({"content":[{"type":"text","text":"ready"}],"structuredContent":{"n":7}}),
    );
    raw["kind"] = json!(kind);
    raw["reason"] = json!("restart_before_dispatch");
    let entry = serde_json::from_value(raw).unwrap();
    let (ack, done) = mpsc::sync_channel(1);
    f.supervisor.0.active.lock().unwrap()["r"]
        .send(Event::LateAnswer {
            call: HostCall {
                position: 0,
                kind: CallKind::Tool {
                    name: "read".into(),
                },
                args: JsonText::null(),
            },
            entry: Ok(entry),
            ack,
        })
        .unwrap();
    done.recv_timeout(TIMEOUT).unwrap();
    let delivery = f.delivery();
    assert_eq!(
        delivery.settlement,
        if kind == "not_started" {
            Settlement::Rejected
        } else {
            Settlement::Fulfilled
        }
    );
    assert_eq!(
        f.calls()[0].outcome,
        if kind == "not_started" {
            Outcome::Withdrawn
        } else {
            Outcome::Ok
        }
    );
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["calls"][0]["waited_on_person"],
        true
    );
    attempt.reply.send(Ok(held_reply())).unwrap();
    f.finish("7");
    assert_eq!(f.terminal()["status"], "completed");
    assert!(f.parent.try_recv().is_err());
}
#[test]
fn not_started_before_the_held_reply_records_the_person_hold() {
    early_late("not_started");
}
#[test]
fn result_before_the_held_reply_records_the_person_hold() {
    early_late("result");
}

#[test]
fn received_final_is_recorded_before_budget_checks_and_never_withdrawn() {
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    let clock = f.clock.clone();
    *f.supervisor.0.before_record.lock().unwrap() = Some(Arc::new(move || clock.set(FAR_DEADLINE)));
    pull.reply
        .send(Ok(late_page(
            "i1",
            1,
            vec![late_entry(
                &call.key,
                "e",
                json!({"content":[{"type":"text","text":"done"}],"structuredContent":{"n":1}}),
            )],
        )))
        .unwrap();
    assert_eq!(f.terminal()["status"], "budget_exhausted:wall");
    assert_eq!(
        f.calls()[0].outcome,
        Outcome::Ok,
        "receipt of the final answer preserves its known outcome"
    );
    if let Ok(ack) = f.custody_attempts.recv_timeout(Duration::from_millis(100)) {
        assert_eq!(
            ack.name, "late_results.ack",
            "a received final must not be withdrawn"
        );
        ack.reply.send(Ok(json!({}))).unwrap();
    }
}

#[test]
fn mark_held_accounts_the_previous_person_interval_before_changing_state() {
    let f = Fixture::new();
    f.host
        .store
        .write(|tx| {
            store::insert_run(tx, &new_run("r"), None)?;
            store::insert_call(tx, "r", 0, "read", 0, CallStart::Intent { at: 100 })?;
            Ok(())
        })
        .unwrap();
    let Lookup::Found(run) = f.host.store.read(|c| store::lookup(c, "r")).unwrap() else {
        panic!()
    };
    let (events, receiver) = mpsc::channel();
    let mut driver = Driver::new(
        f.supervisor.0.clone(),
        *run,
        Start {
            scope: RegisteredScope {
                selector: serde_json::from_value(
                    json!({"owner":{"kind":"reserved","module_id":"core"},"ref":"r","epoch":7}),
                )
                .unwrap(),
                targets: Default::default(),
            },
            presets: Default::default(),
            tools: Default::default(),
            unavailable: Default::default(),
            limits: Limits::default(),
            wall_deadline_ms: FAR_DEADLINE,
        },
        events,
        receiver,
    );
    driver.in_flight = 1;
    driver.flight_positions.insert(0);
    driver.blocked = true;
    driver.mark_held(0, true).unwrap();
    f.clock.set(1_100);
    driver.mark_held(0, false).unwrap();
    assert_eq!(
        driver.person_wait_ms, 1_000,
        "the interval must be charged before held is cleared"
    );
    f.clock.set(2_100);
    driver.mark_held(0, true).unwrap();
    assert_eq!(
        driver.person_wait_ms, 1_000,
        "the intervening non-person interval must not be charged"
    );
}

#[test]
fn cancellation_withdraws_held_calls_in_parallel_and_shutdown_releases_drivers_lock() {
    let f = Fixture::new();
    let (_, pull) = f.provider_held();
    f.issue(1, "read", json!({}));
    f.attempt().reply.send(Ok(held_reply())).unwrap();
    f.supervisor.held("r", 1, true).unwrap();
    let supervisor = f.supervisor.clone();
    let shutdown = spawn_bounded(move || supervisor.shutdown().unwrap());
    let first = f.custody_attempt("tool.withdraw");
    assert!(
        f.supervisor.0.drivers.try_lock().is_ok(),
        "shutdown must not lock drivers across withdrawal"
    );
    let second = f.custody_attempt("tool.withdraw");
    first.reply.send(Ok(json!({"answer":"withdrawn"}))).unwrap();
    second
        .reply
        .send(Ok(json!({"answer":"withdrawn"})))
        .unwrap();
    drop(pull);
    shutdown.join("shutdown after both held calls were withdrawn");
    assert!(
        f.calls()
            .iter()
            .all(|call| call.outcome == Outcome::Withdrawn)
    );
}

#[test]
fn withdrawal_retries_transient_refusals_and_records_a_retained_completed_result() {
    let f = Fixture::new();
    let (_, pull) = f.provider_held();
    let supervisor = f.supervisor.clone();
    let cancel = spawn_bounded(move || supervisor.cancel("r").unwrap());
    f.custody_attempt("tool.withdraw")
        .reply
        .send(Err(WireError::Refused {
            code: "module_warming".into(),
            message: "retry".into(),
        }))
        .unwrap();
    f.custody_attempt("tool.withdraw").reply.send(Ok(json!({"answer":"completed","result_retained":true,"result":{"content":[{"type":"text","text":"completed"}],"structuredContent":{"n":42}}}))).unwrap();
    cancel.join("cancel of the run holding a call");
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
    drop(pull);
}

#[test]
fn reconnect_then_incarnation_reset_reaches_restart_not_started() {
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    pull.reply.send(Ok(late_page("old", 0, vec![]))).unwrap();
    let reconnect = f.custody_attempt("late_results");
    reconnect
        .reply
        .send(Err(WireError::Unknown("connection restored".into())))
        .unwrap();
    let stale = f.custody_attempt("late_results");
    assert_eq!(stale.input["since"]["provider_incarnation"], "old");
    stale
        .reply
        .send(Err(WireError::Refused {
            code: "cursor_incarnation_changed".into(),
            message: "restarted".into(),
        }))
        .unwrap();
    let fresh = f.custody_attempt("late_results");
    assert_eq!(fresh.input["since"], Value::Null);
    let mut entry = late_entry(&call.key, "restart", Value::Null);
    entry["kind"] = json!("not_started");
    entry["reason"] = json!("restart_before_dispatch");
    fresh
        .reply
        .send(Ok(late_page("new", 1, vec![entry])))
        .unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Rejected);
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    f.finish("null");
    assert_eq!(f.terminal()["status"], "completed");
    assert_eq!(f.calls()[0].outcome, Outcome::Withdrawn);
    assert!(f.attempts.try_recv().is_err());
}

#[test]
fn consent_not_started_detail_allows_only_the_program_to_choose_a_fresh_call() {
    let f = Fixture::new();
    let (first, pull) = f.provider_held();
    let mut entry = late_entry(&first.key, "approved", Value::Null);
    entry["kind"] = json!("not_started");
    entry["reason"] = json!("consent");
    entry["detail"] = json!({"consent":"approved","site":"example.test"});
    pull.reply
        .send(Ok(late_page("i1", 1, vec![entry])))
        .unwrap();
    let rejected = f.delivery();
    assert_eq!(rejected.settlement, Settlement::Rejected);
    let error: Value = serde_json::from_str(rejected.value.as_str()).unwrap();
    assert_eq!(error["code"], "not_started");
    assert_eq!(
        error["detail"],
        json!({"consent":"approved","site":"example.test"})
    );
    assert_eq!(f.calls()[0].outcome, Outcome::Withdrawn);
    assert!(
        f.attempts.try_recv().is_err(),
        "basal must not retry the original action"
    );
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    f.issue(1, "read", json!({}));
    let fresh = f.attempt();
    assert_ne!(first.key, fresh.key);
    fresh.reply.send(Ok(json!({"content":[{"type":"text","text":"grant already in place"}],"structuredContent":{"answer":42}}))).unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    f.finish("42");
    assert_eq!(f.terminal()["status"], "completed");
    assert_eq!(f.calls()[1].outcome, Outcome::Ok);
}

#[test]
fn incarnation_change_during_ack_rereads_even_after_the_last_call_settled() {
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    let entry = late_entry(
        &call.key,
        "stable",
        json!({"content":[{"type":"text","text":"done"}],"structuredContent":{"n":1}}),
    );
    pull.reply
        .send(Ok(late_page("old", 1, vec![entry.clone()])))
        .unwrap();
    f.delivery();
    f.finish("1");
    f.custody_attempt("late_results.ack")
        .reply
        .send(Err(WireError::Refused {
            code: "cursor_incarnation_changed".into(),
            message: "provider restarted before ack".into(),
        }))
        .unwrap();
    let fresh = f.custody_attempt("late_results");
    assert_eq!(fresh.input["since"], Value::Null);
    assert_eq!(
        f.supervisor.result("r").unwrap().unwrap()["status"],
        "running"
    );
    fresh
        .reply
        .send(Ok(late_page("new", 1, vec![entry])))
        .unwrap();
    let ack = f.custody_attempt("late_results.ack");
    assert_eq!(ack.input["through"]["provider_incarnation"], "new");
    assert!(
        f.parent.try_recv().is_err(),
        "the reread must not deliver the value twice"
    );
    ack.reply.send(Ok(json!({}))).unwrap();
    assert_eq!(f.terminal()["status"], "completed");
    assert_eq!(f.calls()[0].outcome, Outcome::Ok);
}

#[test]
fn published_093_held_reply_vectors_are_consumed_without_local_wire_rules() {
    let vectors: Value = serde_json::from_str(
        cortexkit_role_tool_provider::vectors::file("held-reply-v1.json")
            .expect("published held-reply vectors"),
    )
    .unwrap();
    let valid = vectors["valid"].as_array().unwrap();
    let invalid = vectors["invalid"].as_array().unwrap();
    let decode_invalid = vectors["decode_invalid"].as_array().unwrap();
    assert_eq!(
        (valid.len(), invalid.len(), decode_invalid.len()),
        (5, 5, 3)
    );
    assert!(
        valid.iter().any(|case| case["name"] == "held_running"),
        "0.10.2 publishes a held reply for running work"
    );
    for case in valid {
        match held::classify(case["body"].clone(), Some(&ResultFormat::ToolResultV1)) {
            held::Reply::Held { detail, .. } => assert_eq!(
                detail,
                case["body"]["held"].get("detail").cloned(),
                "{}",
                case["name"]
            ),
            held::Reply::Final(_) => {
                panic!("published held reply settled as final: {}", case["name"])
            }
        }
    }
    for case in invalid.iter().chain(decode_invalid) {
        assert!(
            matches!(
                held::classify(case["body"].clone(), Some(&ResultFormat::ToolResultV1)),
                held::Reply::Final(Err(_))
            ),
            "published malformed body was accepted: {}",
            case["name"]
        );
    }
}
