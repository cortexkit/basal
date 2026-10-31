use super::*;
use cortexkit_role_tool_provider::late_results::LateEntry;

fn vectors() -> Value {
    serde_json::from_str(cortexkit_role_tool_provider::vectors::HELD_UPDATE_V1_JSON).unwrap()
}

fn accounting_driver(f: &Fixture) -> Driver {
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
    driver
}

#[test]
fn published_held_update_vectors_map_and_account_in_cursor_order() {
    let vectors = vectors();
    let f = Fixture::new();
    let mut driver = accounting_driver(&f);
    let base = vectors["sequence"]["effective_at"][0].as_i64().unwrap();
    for (index, raw) in vectors["sequence"]["reply"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let entry: LateEntry = serde_json::from_value(raw.clone()).unwrap();
        let at = entry.settled_at as i64 - base + 100;
        f.clock.set(at);
        match held::map_entry(entry, None).unwrap() {
            held::Update::Held(held) => {
                assert_eq!(
                    cortexkit_role_tool_provider::result::is_person_wait(&held.reason),
                    vectors["sequence"]["person_wait"][index]
                );
                driver.update_hold_reason(0, held.reason, at).unwrap();
                assert_eq!(driver.in_flight, 1);
                assert!(driver.held_positions.contains(&0));
            }
            held::Update::Final(mapped) => {
                assert_eq!(mapped.outcome, Outcome::Ok);
                driver.mark_held(0, false).unwrap();
            }
            held::Update::Malformed(message) => panic!("{message}"),
        }
    }
    assert_eq!(driver.person_wait_ms, 60_000);
    for case in vectors["valid"].as_array().unwrap() {
        let entry: LateEntry = serde_json::from_value(case["entry"].clone()).unwrap();
        assert!(
            matches!(held::map_entry(entry, None), Some(held::Update::Held(_))),
            "{}",
            case["name"]
        );
    }
    for case in vectors["invalid"].as_array().unwrap() {
        assert!(
            serde_json::from_value::<LateEntry>(case["entry"].clone()).is_err(),
            "{}",
            case["name"]
        );
    }
    for case in vectors["gating"].as_array().unwrap() {
        let caps: Option<Vec<String>> =
            serde_json::from_value(case["consumer_capabilities"].clone()).unwrap();
        let may_send = cortexkit_role_tool_provider::consumer_reads_held_update_v1(caps.as_deref())
            && case["first_reply_held"].as_bool().unwrap();
        assert_eq!(may_send, case["may_send"], "{}", case["name"]);
    }
}

#[test]
fn late_running_update_refunds_time_and_recovery_preserves_the_total() {
    let f = Fixture::new();
    let mut driver = accounting_driver(&f);
    f.clock.set(180_100);
    driver.account_time();
    assert_eq!(driver.person_wait_ms, 180_000);
    driver
        .update_hold_reason(0, "running".into(), 60_100)
        .unwrap();
    assert_eq!(driver.person_wait_ms, 60_000);
    assert!(f.supervisor.holds("r").is_empty());
    f.supervisor.recover().unwrap();
    let result = f.supervisor.result("r").unwrap().unwrap();
    assert_eq!(result["status"], "interrupted");
    assert_eq!(result["person_wait_ms"], 60_000);
    assert_eq!(result["calls"][0]["waited_on_person"], true);
    assert_eq!(result["calls"][0]["outcome"], "outcome_unknown");
}

#[test]
fn held_updates_clamp_to_the_hold_start_and_local_clock() {
    let f = Fixture::new();
    let mut driver = accounting_driver(&f);
    f.clock.set(1_100);
    driver.update_hold_reason(0, "running".into(), 0).unwrap();
    assert_eq!(driver.person_wait_ms, 0);
    driver
        .update_hold_reason(0, "consent".into(), i64::MAX)
        .unwrap();
    assert_eq!(driver.person_wait_ms, 0);
    f.clock.set(2_100);
    driver.account_time();
    assert_eq!(driver.person_wait_ms, 1_000);
    driver
        .update_hold_reason(0, "running".into(), 1_600)
        .unwrap();
    assert_eq!(driver.person_wait_ms, 500);
    driver
        .update_hold_reason(0, "running".into(), 1_500)
        .unwrap();
    assert_eq!(driver.person_wait_ms, 400);
}

#[test]
fn consent_hold_with_an_unheld_flight_spends_five_minutes_of_wall_time() {
    let f = Fixture::new();
    let mut driver = accounting_driver(&f);
    driver.update_hold_reason(0, "consent".into(), 100).unwrap();
    f.host
        .store
        .write(|tx| {
            store::insert_call(tx, "r", 1, "read", 0, CallStart::Intent { at: 100 })?;
            Ok(())
        })
        .unwrap();
    driver.in_flight = 2;
    driver.flight_positions.insert(1);
    driver.start.wall_deadline_ms = 300_100;
    f.clock.set(300_100);
    driver.account_time();
    assert_eq!(driver.person_wait_ms, 0);
    assert!(!driver.person_only());
    assert!(driver.wall().unwrap());
    let result = f.supervisor.result("r").unwrap().unwrap();
    assert_eq!(result["status"], "budget_exhausted:wall");
    assert_eq!(result["person_wait_ms"], 0);
    assert_eq!(result["duration_ms"], 300_000);
}

fn routed(mut entry: Value, key: &str, at: i64) -> Value {
    entry["owner"] = json!("reserved:core");
    entry["ref"] = json!("scope-r");
    entry["scope_epoch"] = json!(7);
    entry["custodian"] = json!("reserved:basal");
    entry["call_key"] = json!(key);
    entry["settled_at"] = json!(at);
    entry
}

#[test]
fn consent_then_running_for_twenty_minutes_finishes_with_one_minute_person_wait() {
    let vectors = vectors();
    let f = Fixture::new();
    let (call, pull) =
        f.provider_held_with(vectors["sequence"]["first_reply"].clone(), FAR_DEADLINE);
    f.clock.set(60_100);
    let running = routed(
        vectors["sequence"]["reply"]["entries"][1].clone(),
        &call.key,
        60_100,
    );
    pull.reply
        .send(Ok(late_page("updates", 1, vec![running])))
        .unwrap();
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    let next = f.custody_attempt("late_results");
    f.host.custody_controlled.store(false, Ordering::SeqCst);
    f.clock.set(1_200_100);
    let result = routed(
        vectors["sequence"]["reply"]["entries"][2].clone(),
        &call.key,
        1_200_100,
    );
    next.reply
        .send(Ok(late_page("updates", 2, vec![result])))
        .unwrap();
    let finished = f.worker.send(WorkerMessage::Finished {
        activation_id: 1,
        result: ActivationResult::Completed {
            value: JsonText::new("42").unwrap(),
        },
    });
    let result = f.terminal();
    assert_eq!(result["status"], "completed", "{result}");
    finished.unwrap();
    assert_eq!(f.delivery().settlement, Settlement::Fulfilled);
    assert_eq!(result["person_wait_ms"], 60_000);
    assert_eq!(result["calls"][0]["outcome"], "ok");
}

#[test]
fn malformed_held_entries_are_recorded_acked_and_leave_the_reason_unchanged() {
    let f = Fixture::new();
    let (call, mut pull) = f.provider_held();
    for (index, case) in vectors()["invalid"].as_array().unwrap().iter().enumerate() {
        let entry = routed(case["entry"].clone(), &call.key, 100);
        pull.reply
            .send(Ok(late_page("malformed", index as u64 + 1, vec![entry])))
            .unwrap();
        f.custody_attempt("late_results.ack")
            .reply
            .send(Ok(json!({})))
            .unwrap();
        assert_eq!(
            f.supervisor.holds("r")[0].reason.as_deref(),
            Some("consent")
        );
        assert_eq!(f.calls()[0].outcome, Outcome::Pending);
        assert!(f.parent.try_recv().is_err());
        pull = f.custody_attempt("late_results");
    }
    let result = late_entry(&call.key, "final", json!({"content":[]}));
    pull.reply
        .send(Ok(late_page("malformed", 100, vec![result])))
        .unwrap();
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    f.delivery();
    f.finish("null");
    let result = f.terminal();
    assert_eq!(
        result["warnings"].as_array().unwrap().len(),
        vectors()["invalid"].as_array().unwrap().len(),
        "{result}"
    );
    assert_eq!(result["person_wait_ms"], 0);
}

#[test]
fn map_entry_does_not_settle_a_constructed_malformed_held_update() {
    let mut entry: LateEntry =
        serde_json::from_value(vectors()["valid"][0]["entry"].clone()).unwrap();
    entry.extra.insert("held".into(), json!({"reason":42}));
    assert!(matches!(
        held::map_entry(entry, None),
        Some(held::Update::Malformed(_))
    ));
}

#[test]
fn replayed_older_held_event_does_not_restore_a_person_wait() {
    let vectors = vectors();
    let f = Fixture::new();
    let (call, pull) = f.provider_held();
    f.clock.set(60_100);
    let consent = routed(
        vectors["sequence"]["reply"]["entries"][0].clone(),
        &call.key,
        100,
    );
    let running = routed(
        vectors["sequence"]["reply"]["entries"][1].clone(),
        &call.key,
        60_100,
    );
    pull.reply
        .send(Ok(late_page("first", 2, vec![consent.clone(), running])))
        .unwrap();
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    f.custody_attempt("late_results")
        .reply
        .send(Ok(late_page("replay", 1, vec![consent])))
        .unwrap();
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    assert!(f.supervisor.holds("r").is_empty());
    let result = late_entry(&call.key, "final", json!({"content":[]}));
    f.custody_attempt("late_results")
        .reply
        .send(Ok(late_page("replay", 2, vec![result])))
        .unwrap();
    f.custody_attempt("late_results.ack")
        .reply
        .send(Ok(json!({})))
        .unwrap();
    f.delivery();
    f.finish("null");
    assert_eq!(f.terminal()["person_wait_ms"], 60_000);
}
