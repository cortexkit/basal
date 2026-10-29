use super::*;
use crate::{ActivationEnd, ChannelError, WorkerChannel};
use basal_proto::{
    ActivationResult, Confinement, ParentMessage, PreludeHash, Welcome, WorkerMessage,
};
use std::time::Duration;

struct Worker {
    welcome: Welcome,
    result: Option<ActivationResult>,
    activation: u64,
    starts: usize,
    trigger: Option<JsonText>,
}
impl Worker {
    fn new(succeeds: bool) -> Self {
        Self {
            welcome: Welcome {
                protocol_version: basal_proto::PROTOCOL_VERSION,
                engine: "test".into(),
                prelude_hash: PreludeHash([0; 32]),
                codemode_prelude_hash: PreludeHash([0; 32]),
                confinement: Confinement::None,
            },
            result: Some(if succeeds {
                ActivationResult::Completed {
                    value: JsonText::null(),
                }
            } else {
                ActivationResult::Failed(basal_proto::Failure::Script {
                    message: "private detail must not leave basal".into(),
                })
            }),
            activation: 0,
            starts: 0,
            trigger: None,
        }
    }
}
impl WorkerChannel for Worker {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }
    fn send(&mut self, message: &ParentMessage) -> std::result::Result<(), ChannelError> {
        if let ParentMessage::Activate(request) = message {
            self.activation = request.activation_id;
            self.starts += 1;
            self.trigger = Some(request.trigger.clone());
        }
        Ok(())
    }
    fn recv(&mut self, _: Duration) -> std::result::Result<WorkerMessage, ChannelError> {
        Ok(WorkerMessage::Finished {
            activation_id: self.activation,
            result: self.result.take().expect("one activation"),
        })
    }
    fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
        panic!("flow driver uses timed receives")
    }
    fn kill(&mut self) {}
}

fn drive(f: &Fixture, n: u64, preamble_error: Option<&str>, succeeds: bool) -> String {
    f.rt.admit_events(&[notice(n)]).unwrap();
    let id =
        f.rt.startable()
            .unwrap()
            .into_iter()
            .next()
            .expect("real event admitted")
            .0;
    if let Some(code) = preamble_error {
        f.reply(
            json!({"error":{"code":code,"message":"private event detail"}}),
            Settlement::Rejected,
        );
    } else {
        let mut body = reply();
        body["result"]["event_key"] = json!(notice(n).event_key);
        f.reply(body, Settlement::Fulfilled);
    }
    let mut worker = Worker::new(succeeds);
    let end = f.rt.activate(&id, &mut worker).unwrap();
    if preamble_error.is_some() {
        assert!(matches!(end, ActivationEnd::Failed { .. }), "{end:?}");
        assert_eq!(worker.starts, 0, "preamble failures never start script");
    } else {
        assert_eq!(worker.starts, 1);
        assert_eq!(
            f.rt.run(&id).unwrap().state,
            if succeeds {
                crate::RunState::Succeeded
            } else {
                crate::RunState::Failed
            }
        );
    }
    id
}
fn setup() -> Fixture {
    let f = Fixture::new(1000);
    f.approve("flow");
    f
}
fn sends(f: &Fixture) -> Vec<serde_json::Value> {
    f.source.notices.lock().unwrap().clone()
}
fn drain(f: &Fixture) {
    f.rt.drain_owner_notices().unwrap();
}
fn failing_latched(f: &Fixture) -> bool {
    f.rt.store()
        .read(|c| {
            Ok(c.query_row(
        "SELECT failing_episode IS NOT NULL FROM flow_owner_notice_latches WHERE flow_id='flow'",
        [], |r| r.get(0),
    )?)
        })
        .unwrap()
}
fn delivery(f: &Fixture, id: &str) -> (Option<i64>, u32, Option<String>) {
    f.rt.store().read(|c| Ok(c.query_row(
        "SELECT delivered_at,attempts,failure_code FROM outbox WHERE kind='flow.owner_notice' AND json_extract(body,'$.notice_id')=?1",
        [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    )?)).unwrap()
}
fn restart(f: &mut Fixture) {
    let mut scratch = Fixture::new(1000);
    std::mem::swap(&mut scratch.rt, &mut f.rt);
    drop(scratch);
    f.rt = Runtime::new(
        Arc::new(Store::open(f.path.join("basal.db"), Durability::throwaway()).unwrap()),
        f.source.clone(),
        Arc::new(f.catalog.clone()),
        Arc::new(crate::NoHooks),
        None,
        f.rt.config().clone(),
    );
}

#[test]
fn threshold_counts_real_preamble_failures() {
    let f = setup();
    for n in 1..=3 {
        let id = drive(&f, n, Some("event_not_visible"), false);
        assert_eq!(
            f.rt.run(&id).unwrap().error_kind.as_deref(),
            Some("event_not_visible")
        );
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, n);
        drain(&f);
        assert_eq!(sends(&f).len(), usize::from(n == 3));
    }
    let sent = sends(&f).remove(0);
    assert_eq!(sent["phase"], "preamble");
    assert_eq!(sent["consecutive_failures"], 3);
    assert_eq!(sent["error_kind"], "event_not_visible");
    assert_eq!(sent["notice_id"], "flow:1:failing");
    assert!(!sent.to_string().contains("private"));
    assert!(f.rt.flow("flow").unwrap().unwrap().enabled);
}

#[test]
fn transient_body_deferrals_preserve_the_failure_streak_until_recovery() {
    let f = setup();
    drive(&f, 1, Some("event_not_visible"), false);
    drive(&f, 2, Some("event_not_visible"), false);
    f.rt.admit_events(&[notice(3)]).unwrap();
    let id = f.rt.startable().unwrap()[0].0.clone();
    let mut worker = Worker::new(true);
    for code in [
        "flow_audience_unavailable",
        "store_failure",
        "module_warming",
    ] {
        f.reply(json!({"error":{"code":code}}), Settlement::Rejected);
        assert_eq!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Requeued
        );
        assert_eq!(f.rt.run(&id).unwrap().state, crate::RunState::Pending);
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 2);
        assert!(!failing_latched(&f));
        drain(&f);
        assert!(sends(&f).is_empty());
        assert_eq!(worker.starts, 0);
        let retry_at: i64 =
            f.rt.store()
                .read(|c| {
                    Ok(c.query_row(
                        "SELECT retry_at FROM event_body_journal WHERE run_id=?1",
                        [&id],
                        |r| r.get(0),
                    )?)
                })
                .unwrap();
        f.rt.config.clock.set(retry_at);
    }
    let mut body = reply();
    body["result"]["event_key"] = json!(notice(3).event_key);
    f.reply(body, Settlement::Fulfilled);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(worker.starts, 1);
    let trigger: serde_json::Value =
        serde_json::from_str(worker.trigger.unwrap().as_str()).unwrap();
    assert_eq!(trigger["event"]["body"], json!({"changed":true}));
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    drain(&f);
    assert!(sends(&f).is_empty());
}

#[test]
fn deferred_body_expiry_counts_one_failure_with_the_provider_code_after_restart() {
    for enforce in [false, true] {
        let mut f = setup();
        drive(&f, 1, Some("event_not_visible"), false);
        drive(&f, 2, Some("event_not_visible"), false);
        f.rt.admit_events(&[notice(3)]).unwrap();
        let id = f.rt.startable().unwrap()[0].0.clone();
        let mut worker = Worker::new(true);
        f.reply(
            json!({"error":{"code":"flow_audience_unavailable"}}),
            Settlement::Rejected,
        );
        assert_eq!(
            f.rt.activate(&id, &mut worker).unwrap(),
            ActivationEnd::Requeued
        );
        let deadline = f.rt.run(&id).unwrap().deadline_at.unwrap();
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 2);
        assert!(!failing_latched(&f));
        drain(&f);
        assert!(sends(&f).is_empty());
        restart(&mut f);
        f.rt.config.clock.set(deadline);
        if enforce {
            assert_eq!(f.rt.enforce_deadlines().unwrap(), vec![id.clone()]);
        } else {
            assert_eq!(
                f.rt.activate(&id, &mut worker).unwrap(),
                ActivationEnd::NotRunnable {
                    state: crate::RunState::Failed
                }
            );
        }
        assert_eq!(
            f.rt.run(&id).unwrap().error_kind.as_deref(),
            Some("flow_audience_unavailable")
        );
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 3);
        assert!(failing_latched(&f));
        assert_eq!(worker.starts, 0);
        drain(&f);
        let sent = sends(&f);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["error_kind"], "flow_audience_unavailable");
        assert_eq!(sent[0]["consecutive_failures"], 3);
        assert!(f.rt.enforce_deadlines().unwrap().is_empty());
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 3);
    }
}

#[test]
fn once_per_episode_survives_delivery_and_restart() {
    let mut f = setup();
    for n in 1..=10 {
        drive(&f, n, Some("event_not_visible"), false);
        drain(&f);
        if n == 5 {
            restart(&mut f);
        }
    }
    assert_eq!(sends(&f).len(), 1);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 10);
}
#[test]
fn recovery_rearms_a_new_episode() {
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
        drain(&f);
    }
    let recovered = drive(&f, 4, None, true);
    drain(&f);
    drive(&f, 5, None, true);
    drain(&f);
    for n in 6..=8 {
        drive(&f, n, Some("event_not_visible"), false);
        drain(&f);
    }
    let sent = sends(&f);
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1]["kind"], "recovered");
    assert_eq!(sent[1]["run_id"], recovered);
    assert!(sent[1]["error_kind"].is_null());
    assert!(sent[1]["phase"].is_null());
    assert_eq!(sent[1]["notice_id"], "flow:1:recovered");
    assert_eq!(sent[2]["notice_id"], "flow:2:failing");
}
#[test]
fn capability_gate_holds_queued_rows_without_a_timer() {
    let f = setup();
    f.source.notice_capability.store(false, Ordering::SeqCst);
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
        drain(&f);
    }
    assert!(sends(&f).is_empty());
    assert_eq!(f.rt.owner_notice_next_wake().unwrap(), None);
    f.source.notice_capability.store(true, Ordering::SeqCst);
    assert_eq!(f.rt.owner_notice_next_wake().unwrap(), Some(1000));
    drain(&f);
    assert_eq!(sends(&f).len(), 1);
}
#[test]
fn interleaved_success_and_exempt_refusals_do_not_alert() {
    let f = setup();
    drive(&f, 1, Some("event_not_visible"), false);
    drive(&f, 2, Some("event_not_visible"), false);
    drive(&f, 3, None, true);
    for (n, kind) in [
        (4, "readiness_rejection"),
        (5, "agent_retired"),
        (6, "version_revoked"),
    ] {
        drive(&f, n, Some(kind), false);
    }
    drive(&f, 7, Some("event_not_visible"), false);
    drive(&f, 8, Some("event_not_visible"), false);
    drain(&f);
    assert!(sends(&f).is_empty());
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 2);
}
#[test]
fn script_failures_use_the_same_health_streak() {
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, None, false);
    }
    drain(&f);
    assert_eq!(sends(&f)[0]["phase"], "script");
    assert_eq!(sends(&f)[0]["error_kind"], "script");
}
#[test]
fn retry_uses_same_id_and_final_refusal_unblocks_recovery() {
    use basal_host::transport::WireError;
    for final_code in [
        "flow_not_installed",
        "owner_unknown",
        "flow_owner_notice_invalid",
    ] {
        let f = setup();
        for n in 1..=3 {
            drive(&f, n, Some("event_not_visible"), false);
        }
        f.source.notice_replies.lock().unwrap().extend([
            Err(WireError::Unknown("lost response".into())),
            Err(WireError::Refused {
                code: final_code.into(),
                message: "do not log this detail".into(),
            }),
        ]);
        drain(&f);
        let id = sends(&f)[0]["notice_id"].clone();
        drive(&f, 4, None, true);
        drain(&f);
        assert_eq!(
            sends(&f).len(),
            1,
            "later notice cannot overtake backed-off head"
        );
        assert_eq!(f.rt.owner_notice_next_wake().unwrap(), Some(2000));
        f.rt.config().clock.advance(1000);
        drain(&f);
        drain(&f);
        drain(&f);
        assert_eq!(sends(&f).len(), 3);
        assert_eq!(sends(&f)[1]["notice_id"], id);
        assert_eq!(sends(&f)[2]["kind"], "recovered");
        assert!(f.rt.outbox().unwrap().is_empty());
        let stored: String = f.rt.store().read(|c| Ok(c.query_row(
            "SELECT failure_code FROM outbox WHERE kind='flow.owner_notice' ORDER BY seq LIMIT 1", [], |r| r.get(0)
        )?)).unwrap();
        assert_eq!(stored, final_code);
    }
}
#[test]
fn restart_between_enqueue_and_send_keeps_the_notice() {
    let mut f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    assert!(sends(&f).is_empty());
    restart(&mut f);
    drain(&f);
    drain(&f);
    assert_eq!(sends(&f).len(), 1);
}
#[test]
fn rate_breaker_enqueues_one_independent_auto_disable() {
    let mut f = setup();
    Arc::make_mut(&mut f.rt.config).rate = crate::RateLimits {
        max_runs: 1,
        saturated_windows_to_disable: 1,
        ..Default::default()
    };
    drive(&f, 1, Some("event_not_visible"), false);
    f.rt.admit_events(&[notice(2)]).unwrap();
    f.rt.admit_events(&[notice(3)]).unwrap();
    drain(&f);
    assert_eq!(sends(&f).len(), 1);
    assert_eq!(sends(&f)[0]["kind"], "auto_disabled");
    assert!(sends(&f)[0]["phase"].is_null());
    assert_eq!(f.rt.decision_cards().unwrap().len(), 1);
}

#[test]
fn auto_disabled_notice_carries_the_latest_admitted_run_id() {
    let mut f = setup();
    Arc::make_mut(&mut f.rt.config).rate = crate::RateLimits {
        max_runs: 2,
        saturated_windows_to_disable: 1,
        ..Default::default()
    };
    drive(&f, 1, Some("event_not_visible"), false);
    let latest = drive(&f, 2, None, true);
    f.rt.admit_events(&[notice(3)]).unwrap();
    drain(&f);
    assert_eq!(sends(&f).len(), 1);
    assert_eq!(sends(&f)[0]["kind"], "auto_disabled");
    assert_eq!(sends(&f)[0]["run_id"], latest);
}

#[test]
fn auto_disabled_without_an_admitted_run_is_not_enqueued() {
    let f = setup();
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    f.rt.store()
        .write(|tx| crate::owner_notice::auto_disabled(tx, "flow", 1000))
        .unwrap();
    assert!(f.rt.outbox().unwrap().is_empty());
    assert!(
        logs.0
            .lock()
            .unwrap()
            .iter()
            .any(|(level, fields)| *level == tracing::Level::WARN
                && fields.contains("run_missing")
                && fields.contains("flow_id=\"flow\""))
    );
}

#[test]
fn nonfinal_refusal_persists_code_and_warns_only_when_it_changes() {
    use basal_host::transport::WireError;
    let mut f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    for (index, code) in ["reloading", "reloading", "future_refusal"]
        .into_iter()
        .enumerate()
    {
        f.source
            .notice_replies
            .lock()
            .unwrap()
            .push(Err(WireError::RefusedDetails {
                code: code.into(),
                message: "PRIVATE message".into(),
                detail: json!({"private":"PRIVATE detail"}),
            }));
        drain(&f);
        let (delivered_at, attempts, stored_code) = delivery(&f, "flow:1:failing");
        assert_eq!(delivered_at, None);
        assert_eq!(attempts, index as u32 + 1);
        assert_eq!(stored_code.as_deref(), Some(code));
        let warnings = logs
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(warnings.len(), if index == 2 { 2 } else { 1 });
        let fields = &warnings.last().unwrap().1;
        assert!(fields.contains("seq="));
        assert!(fields.contains("notice_id=\"flow:1:failing\""));
        assert!(fields.contains(&format!("code={code:?}")));
        assert!(!fields.contains("PRIVATE"));
        if index == 0 {
            restart(&mut f);
        }
        let retry = f.rt.owner_notice_next_wake().unwrap().unwrap();
        f.rt.config()
            .clock
            .advance(retry - f.rt.config().clock.now_ms());
    }
}

#[test]
fn stuck_head_is_superseded_at_threshold_and_its_sibling_is_sent_next() {
    use crate::owner_notice::OWNER_NOTICE_SUPERSEDE_ATTEMPTS;
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    drive(&f, 4, None, true);
    f.rt.store().write(|tx| {
        tx.execute("UPDATE outbox SET attempts=?1 WHERE kind='flow.owner_notice' AND json_extract(body,'$.kind')='failing'", [OWNER_NOTICE_SUPERSEDE_ATTEMPTS - 1])?;
        Ok(())
    }).unwrap();
    f.source
        .notice_replies
        .lock()
        .unwrap()
        .push(Err(basal_host::transport::WireError::Refused {
            code: "reloading".into(),
            message: "private".into(),
        }));
    drain(&f);
    assert_eq!(
        sends(&f).len(),
        1,
        "head is still retried below the threshold"
    );
    assert_eq!(delivery(&f, "flow:1:failing").0, None);
    let retry = f.rt.owner_notice_next_wake().unwrap().unwrap();
    f.rt.config()
        .clock
        .advance(retry - f.rt.config().clock.now_ms());
    drain(&f);
    assert_eq!(sends(&f).len(), 1, "superseded head is not sent again");
    let (delivered_at, attempts, code) = delivery(&f, "flow:1:failing");
    assert_eq!(delivered_at, Some(f.rt.config().clock.now_ms()));
    assert_eq!(attempts, OWNER_NOTICE_SUPERSEDE_ATTEMPTS);
    assert_eq!(code.as_deref(), Some("superseded"));
    drain(&f);
    assert_eq!(sends(&f).len(), 2);
    assert_eq!(sends(&f)[1]["kind"], "recovered");
    assert!(f.rt.outbox().unwrap().is_empty());
}

#[test]
fn lone_stuck_head_is_not_superseded_even_with_a_delivered_newer_row() {
    use crate::owner_notice::OWNER_NOTICE_SUPERSEDE_ATTEMPTS;
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    f.rt.store().write(|tx| {
        tx.execute("UPDATE outbox SET attempts=?1 WHERE kind='flow.owner_notice'", [OWNER_NOTICE_SUPERSEDE_ATTEMPTS])?;
        tx.execute("INSERT INTO outbox(at,kind,flow_id,body,delivered_at) VALUES (1000,'flow.owner_notice','flow','{}',1000)", [])?;
        tx.execute("INSERT INTO outbox(at,kind,flow_id,body) VALUES (1000,'flow.owner_notice','other-flow','{}')", [])?;
        Ok(())
    }).unwrap();
    f.source
        .notice_replies
        .lock()
        .unwrap()
        .push(Err(basal_host::transport::WireError::Unknown(
            "private".into(),
        )));
    drain(&f);
    assert_eq!(sends(&f)[0]["notice_id"], "flow:1:failing");
    assert_eq!(
        delivery(&f, "flow:1:failing"),
        (
            None,
            OWNER_NOTICE_SUPERSEDE_ATTEMPTS + 1,
            Some("transport".into())
        )
    );
}

#[test]
fn stale_notices_are_discarded_once_before_sending_fresh_health() {
    use crate::owner_notice::OWNER_NOTICE_MAX_AGE_MS;
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    drive(&f, 4, None, true);
    f.rt.config().clock.advance(OWNER_NOTICE_MAX_AGE_MS + 1);
    let now = f.rt.config().clock.now_ms();
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE outbox SET retry_at=?1 WHERE kind='flow.owner_notice'",
                [now + 256_000],
            )?;
            Ok(())
        })
        .unwrap();
    for n in 5..=7 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    let logs = CapturedLogs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    drain(&f);
    drain(&f);
    assert_eq!(sends(&f).len(), 1);
    assert_eq!(sends(&f)[0]["notice_id"], "flow:2:failing");
    for id in ["flow:1:failing", "flow:1:recovered"] {
        assert_eq!(delivery(&f, id), (Some(now), 0, Some("stale".into())));
    }
    let warnings = logs
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| *level == tracing::Level::WARN)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 2);
    for (_, fields) in warnings {
        assert!(fields.contains("code=\"stale\""));
        assert!(fields.contains("notice_id=\"flow:1:"));
        assert!(fields.contains("seq="));
    }
    assert!(f.rt.outbox().unwrap().is_empty());
}

#[test]
fn unavailable_refusals_and_unaccepted_replies_retry_without_a_sqlite_lock() {
    use basal_host::transport::WireError;
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    for (response, code) in [
        (
            Err(WireError::Unknown("private transport error".into())),
            "transport",
        ),
        (
            Err(WireError::NeverSent("private route error".into())),
            "transport",
        ),
        (
            Err(WireError::Refused {
                code: "reloading".into(),
                message: "reload".into(),
            }),
            "reloading",
        ),
        (
            Err(WireError::Refused {
                code: "future_refusal".into(),
                message: "unknown".into(),
            }),
            "future_refusal",
        ),
        (Ok(json!({"accepted":false})), "not_accepted"),
        (Ok(json!({})), "not_accepted"),
    ] {
        f.source.notice_replies.lock().unwrap().push(response);
        let store = f.rt.shared.store.clone();
        *f.source.on_notice.lock().unwrap() = Some(Box::new(move || {
            store.write(|tx| { tx.execute("INSERT OR REPLACE INTO meta(key,value) VALUES ('during_notice','unlocked')",[])?; Ok(()) }).unwrap();
        }));
        drain(&f);
        assert_eq!(f.rt.outbox().unwrap().len(), 1);
        assert_eq!(delivery(&f, "flow:1:failing").2.as_deref(), Some(code));
        let retry = f.rt.owner_notice_next_wake().unwrap().unwrap();
        assert!(retry > f.rt.config().clock.now_ms());
        f.rt.config()
            .clock
            .advance(retry - f.rt.config().clock.now_ms());
    }
    drain(&f);
    let sent = sends(&f);
    assert_eq!(sent.len(), 7);
    assert!(
        sent.iter().all(|v| v == &sent[0]),
        "retry preserves every request byte"
    );
    assert!(f.rt.outbox().unwrap().is_empty());
}

#[test]
fn typed_rejection_deadline_is_exempt_at_the_real_driver_boundary() {
    use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
    let f = setup();
    for n in 1..=2 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    f.rt.admit_events(&[notice(3)]).unwrap();
    let id = f.rt.startable().unwrap()[0].0.clone();
    let refusal = FlowRefusal::new(RefusalReason::NoFlowScope, "plexus", "events_get");
    let value = refusal.outcome().value.as_str().to_owned();
    f.rt.store().write(|tx| {
        tx.execute("INSERT INTO journal(run_id,position,kind_code,module,op,args,args_digest,idempotency_key,class,dispatch,settlement,value,payload_hash,delivery_order,issued_generation) VALUES (?1,0,0,'plexus','events_get','{}',zeroblob(32),?1,'query','none','rejected',?2,zeroblob(32),0,0)",rusqlite::params![id,value])?;
        Ok(())
    }).unwrap();
    f.reply(json!({"error":{"code":"deadline"}}), Settlement::Rejected);
    let mut worker = Worker::new(false);
    f.rt.activate(&id, &mut worker).unwrap();
    assert_eq!(worker.starts, 0);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 2);
    drain(&f);
    assert!(sends(&f).is_empty());
}
