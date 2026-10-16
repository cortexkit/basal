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
    for final_code in ["flow_not_installed", "owner_unknown"] {
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
fn unavailable_refusals_and_unaccepted_replies_retry_without_a_sqlite_lock() {
    use basal_host::transport::WireError;
    let f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    for response in [
        Err(WireError::Refused {
            code: "reloading".into(),
            message: "reload".into(),
        }),
        Err(WireError::Refused {
            code: "future_refusal".into(),
            message: "unknown".into(),
        }),
        Ok(json!({"accepted":false})),
        Ok(json!({})),
    ] {
        f.source.notice_replies.lock().unwrap().push(response);
        let store = f.rt.shared.store.clone();
        *f.source.on_notice.lock().unwrap() = Some(Box::new(move || {
            store.write(|tx| { tx.execute("INSERT OR REPLACE INTO meta(key,value) VALUES ('during_notice','unlocked')",[])?; Ok(()) }).unwrap();
        }));
        drain(&f);
        assert_eq!(f.rt.outbox().unwrap().len(), 1);
        let retry = f.rt.owner_notice_next_wake().unwrap().unwrap();
        assert!(retry > f.rt.config().clock.now_ms());
        f.rt.config()
            .clock
            .advance(retry - f.rt.config().clock.now_ms());
    }
    drain(&f);
    let sent = sends(&f);
    assert_eq!(sent.len(), 5);
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
