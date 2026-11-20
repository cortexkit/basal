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
    fn fixture_inline_recv(&self) -> bool {
        true
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
    // The approval creates attribution facts, which share the bus outbox.
    for event in f.rt.pending_bus_events(10).unwrap() {
        assert_eq!(event.name, "attribution_fact");
        f.rt.bus_event_published(&event).unwrap();
    }
    f
}
#[derive(Default)]
struct NoticeBus {
    sent: Vec<serde_json::Value>,
}
impl NoticeBus {
    fn drain(&mut self, f: &Fixture) {
        for event in f.rt.pending_bus_events(10).unwrap() {
            assert_eq!(event.name, "flow_owner_notice");
            let body: serde_json::Value = serde_json::from_str(&event.body).unwrap();
            assert_eq!(event.message_id, body["notice_id"]);
            f.rt.bus_event_published(&event).unwrap();
            self.sent.push(body);
        }
    }
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
    )
    .expect("runtime startup");
}

#[test]
fn threshold_counts_real_preamble_failures() {
    let f = setup();
    let mut bus = NoticeBus::default();
    for n in 1..=3 {
        let id = drive(&f, n, Some("event_not_visible"), false);
        assert_eq!(
            f.rt.run(&id).unwrap().error_kind.as_deref(),
            Some("event_not_visible")
        );
        assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, n);
        bus.drain(&f);
        assert_eq!(bus.sent.len(), usize::from(n == 3));
    }
    let sent = bus.sent.remove(0);
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
    let mut bus = NoticeBus::default();
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
        bus.drain(&f);
        assert!(bus.sent.is_empty());
        assert_eq!(worker.starts, 0);
        let retry_at = f.rt.saved_event_body(&id).unwrap().unwrap().2.unwrap();
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
    bus.drain(&f);
    assert!(bus.sent.is_empty());
}

#[test]
fn deferred_body_expiry_counts_one_failure_with_the_provider_code_after_restart() {
    for enforce in [false, true] {
        let mut f = setup();
        let mut bus = NoticeBus::default();
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
        bus.drain(&f);
        assert!(bus.sent.is_empty());
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
        bus.drain(&f);
        let sent = bus.sent;
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
    let mut bus = NoticeBus::default();
    for n in 1..=10 {
        drive(&f, n, Some("event_not_visible"), false);
        bus.drain(&f);
        if n == 5 {
            restart(&mut f);
        }
    }
    assert_eq!(bus.sent.len(), 1);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 10);
}
#[test]
fn recovery_rearms_a_new_episode() {
    let f = setup();
    let mut bus = NoticeBus::default();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
        bus.drain(&f);
    }
    let recovered = drive(&f, 4, None, true);
    bus.drain(&f);
    drive(&f, 5, None, true);
    bus.drain(&f);
    for n in 6..=8 {
        drive(&f, n, Some("event_not_visible"), false);
        bus.drain(&f);
    }
    let sent = bus.sent;
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[1]["kind"], "recovered");
    assert_eq!(sent[1]["run_id"], recovered);
    assert!(sent[1]["error_kind"].is_null());
    assert!(sent[1]["phase"].is_null());
    assert_eq!(sent[1]["notice_id"], "flow:1:recovered");
    assert_eq!(sent[2]["notice_id"], "flow:2:failing");
}
#[test]
fn interleaved_success_and_exempt_refusals_do_not_alert() {
    let f = setup();
    let mut bus = NoticeBus::default();
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
    bus.drain(&f);
    assert!(bus.sent.is_empty());
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 2);
}
#[test]
fn script_failures_use_the_same_health_streak() {
    let f = setup();
    let mut bus = NoticeBus::default();
    for n in 1..=3 {
        drive(&f, n, None, false);
    }
    bus.drain(&f);
    assert_eq!(bus.sent[0]["phase"], "script");
    assert_eq!(bus.sent[0]["error_kind"], "script");
}

#[test]
fn bus_retry_preserves_notice_and_blocks_only_the_same_flow_until_acknowledged() {
    let mut f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    let first = f.rt.pending_bus_events(10).unwrap().remove(0);
    assert_eq!(first.message_id, "flow:1:failing");
    f.rt.bus_event_retry(&first).unwrap();
    drive(&f, 4, None, true);
    assert!(f.rt.uninstall_flow("flow").unwrap());
    let other = json!({"flow_id":"other","notice_id":"other:1:failing"}).to_string();
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO outbox(at,kind,flow_id,body) VALUES(1000,'flow.owner_notice','other',?1)",
                [other],
            )?;
            Ok(())
        })
        .unwrap();
    restart(&mut f);

    let mut bus = NoticeBus::default();
    bus.drain(&f);
    assert_eq!(bus.sent.len(), 1, "only the other flow can publish now");
    assert_eq!(bus.sent[0]["notice_id"], "other:1:failing");
    assert_eq!(f.rt.next_bus_event_at().unwrap(), Some(1250));
    f.rt.config().clock.set(1249);
    assert!(f.rt.pending_bus_events(10).unwrap().is_empty());
    f.rt.config().clock.set(1250);
    let retry = f.rt.pending_bus_events(10).unwrap();
    assert_eq!(
        retry.len(),
        1,
        "recovery must wait for the failing notice's ack"
    );
    assert_eq!(retry[0].seq, first.seq);
    assert_eq!(retry[0].message_id, first.message_id);
    assert_eq!(
        retry[0].body, first.body,
        "a retry preserves every payload byte"
    );
    bus.drain(&f);
    bus.drain(&f);
    bus.drain(&f);
    assert_eq!(bus.sent.len(), 3);
    assert_eq!(bus.sent[1]["notice_id"], "flow:1:failing");
    assert_eq!(bus.sent[2]["notice_id"], "flow:1:recovered");
    assert!(f.rt.pending_bus_events(10).unwrap().is_empty());
    assert_eq!(f.rt.next_bus_event_at().unwrap(), None);
}

#[test]
fn restart_between_enqueue_and_send_keeps_the_notice() {
    let mut f = setup();
    let mut bus = NoticeBus::default();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    assert!(bus.sent.is_empty());
    restart(&mut f);
    bus.drain(&f);
    bus.drain(&f);
    assert_eq!(bus.sent.len(), 1);
}

#[test]
fn restart_uses_bus_state_regardless_of_legacy_call_backoff() {
    let mut f = setup();
    for n in 1..=3 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    let mut bus = NoticeBus::default();
    bus.drain(&f);
    drive(&f, 4, None, true);
    f.rt.store()
        .write(|tx| {
            tx.execute(
                "UPDATE outbox SET attempts=22,retry_at=100000,failure_code='transport' \
                 WHERE kind='flow.owner_notice'",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    restart(&mut f);

    assert_eq!(f.rt.outbox().unwrap().len(), 1);
    bus.drain(&f);
    bus.drain(&f);
    assert_eq!(bus.sent.len(), 2);
    assert_eq!(bus.sent[0]["notice_id"], "flow:1:failing");
    assert_eq!(bus.sent[1]["notice_id"], "flow:1:recovered");
    assert!(f.rt.outbox().unwrap().is_empty());
}
#[test]
fn rate_breaker_enqueues_one_independent_auto_disable() {
    let mut f = setup();
    let mut bus = NoticeBus::default();
    Arc::make_mut(&mut f.rt.config).rate = crate::RateLimits {
        max_runs: 1,
        saturated_windows_to_disable: 1,
        ..Default::default()
    };
    drive(&f, 1, Some("event_not_visible"), false);
    f.rt.admit_events(&[notice(2)]).unwrap();
    f.rt.admit_events(&[notice(3)]).unwrap();
    bus.drain(&f);
    assert_eq!(bus.sent.len(), 1);
    assert_eq!(bus.sent[0]["kind"], "auto_disabled");
    assert!(bus.sent[0]["phase"].is_null());
    assert_eq!(f.rt.decision_cards().unwrap().len(), 1);
}

#[test]
fn auto_disabled_notice_carries_the_latest_admitted_run_id() {
    let mut f = setup();
    let mut bus = NoticeBus::default();
    Arc::make_mut(&mut f.rt.config).rate = crate::RateLimits {
        max_runs: 2,
        saturated_windows_to_disable: 1,
        ..Default::default()
    };
    drive(&f, 1, Some("event_not_visible"), false);
    let latest = drive(&f, 2, None, true);
    f.rt.admit_events(&[notice(3)]).unwrap();
    bus.drain(&f);
    assert_eq!(bus.sent.len(), 1);
    assert_eq!(bus.sent[0]["kind"], "auto_disabled");
    assert_eq!(bus.sent[0]["run_id"], latest);
}

#[test]
fn auto_disabled_without_an_admitted_run_is_not_enqueued() {
    let f = setup();
    let logs = CapturedLogs::default();
    f.rt.store()
        .write(|tx| crate::owner_notice::auto_disabled_committed(tx, "flow", 1000))
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
fn typed_rejection_deadline_is_exempt_at_the_real_driver_boundary() {
    use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
    let f = setup();
    let mut bus = NoticeBus::default();
    for n in 1..=2 {
        drive(&f, n, Some("event_not_visible"), false);
    }
    f.rt.admit_events(&[notice(3)]).unwrap();
    let id = f.rt.startable().unwrap()[0].0.clone();
    let refusal = FlowRefusal::new(RefusalReason::NoFlowScope, "plexus", "events_get");
    let value = refusal.outcome().value;
    use crate::run_records::{Arrival, ArrivalDurability, Body, CallIdentity};
    f.rt.fixture_append(
        &id,
        Body::Issued(CallIdentity::new(
            basal_proto::HostCall {
                position: 0,
                kind: CallKind::Op {
                    module: "plexus".into(),
                    op: "events_get".into(),
                },
                args: JsonText::new("{}").unwrap(),
            },
            crate::StoredClass::Query,
        )),
    )
    .unwrap();
    f.rt.fixture_append(
        &id,
        Body::Arrived(Arrival {
            position: 0,
            arrival_seq: 0,
            outcome: "rejected".into(),
            settlement: Settlement::Rejected,
            value_hash: crate::ids::payload_hash(Settlement::Rejected, &value),
            value,
            handle: None,
            usage: None,
            cost_micros: None,
            kv_effect: None,
            durability: ArrivalDurability {
                refusal_committed: true,
                ..Default::default()
            },
        }),
    )
    .unwrap();
    f.rt.fixture_append(
        &id,
        Body::ScopeCut {
            deadline_exempt: true,
        },
    )
    .unwrap();
    f.reply(json!({"error":{"code":"deadline"}}), Settlement::Rejected);
    let mut worker = Worker::new(false);
    f.rt.activate(&id, &mut worker).unwrap();
    assert_eq!(worker.starts, 0);
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 2);
    bus.drain(&f);
    assert!(bus.sent.is_empty());
}
