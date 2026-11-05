use super::*;
use basal_host::{
    Sent, UnknownReason,
    flow_refusal::{FlowRefusal, RefusalReason},
};

#[test]
fn keyed_transport_retries_keep_the_key_and_acknowledge_the_actual_attempt() {
    let f = Fixture::new(true);
    f.install("retry", 1, json!({"ops":[{"module":"test","op":"keyed"}]}));
    let id = f.admit("retry", "first");
    f.host.transports.lock().unwrap().extend([
        Err(TransportError::Unavailable {
            sent: Sent::Maybe(UnknownReason::ConnectionLost),
            detail: "route restarting".into(),
        }),
        Err(TransportError::Unavailable {
            sent: Sent::Maybe(UnknownReason::ConnectionLost),
            detail: "route restarting".into(),
        }),
        Ok(Dispatched::Completed(HostOutcome::fulfilled(
            JsonText::new("7").unwrap(),
        ))),
    ]);
    let mut worker = ScriptedWorker::new(vec![
        op(0, "keyed", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    let sent = f.host.sent.lock().unwrap();
    assert_eq!(
        sent.iter().map(|r| r.attempt).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        sent.iter()
            .all(|r| r.idempotency_key == sent[0].idempotency_key)
    );
    assert_eq!(f.host.committed.lock().unwrap()[0].attempt, 3);
}

#[test]
fn ambiguous_unkeyed_transport_is_not_retried_and_readers_show_reconciliation() {
    let f = Fixture::new(false);
    f.install(
        "uncertain",
        1,
        json!({"ops":[{"module":"test","op":"write"}]}),
    );
    let id = f.admit("uncertain", "first");
    f.host
        .transports
        .lock()
        .unwrap()
        .push_back(Err(TransportError::Unavailable {
            sent: Sent::Maybe(UnknownReason::ConnectionLost),
            detail: "route closed".into(),
        }));
    let mut worker = ScriptedWorker::new(vec![
        op(0, "write", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert_eq!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::NeedsReconcile { positions: vec![0] }
    );
    assert_eq!(f.host.sent.lock().unwrap().len(), 1);
    assert_eq!(f.rt.run(&id).unwrap().state, RunState::NeedsReconcile);
    assert_eq!(
        f.rt.unknown_reason(&id, 0).unwrap(),
        Some(UnknownReason::ConnectionLost)
    );
    assert_eq!(f.rt.health().unwrap().unknown_calls, vec![(id.clone(), 0)]);
    assert_eq!(f.rt.flow_health().unwrap()[0].needs_reconcile, vec![id]);
    assert_eq!(f.rt.decisions_due().unwrap().len(), 1);
}

#[test]
fn exhausted_query_transport_is_a_typed_rejection_not_an_unknown_write() {
    let f = Fixture::new(true);
    f.install("unavailable", 1, json!({}));
    let id = f.admit("unavailable", "first");
    for _ in 0..=f.rt.config.unavailable_retries {
        f.host
            .transports
            .lock()
            .unwrap()
            .push_back(Err(TransportError::Unavailable {
                sent: Sent::Maybe(UnknownReason::ReplyTimeout),
                detail: "route timed out".into(),
            }));
    }
    let mut worker = ScriptedWorker::new(vec![
        op(0, "query", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    let calls = f.rt.calls(&id).unwrap();
    let outcome = calls[0].outcome.as_ref().unwrap();
    assert_eq!(outcome.settlement, Settlement::Rejected);
    assert_eq!(
        serde_json::from_str::<Value>(outcome.value.as_str()).unwrap()["code"],
        "unavailable"
    );
    assert!(f.rt.health().unwrap().unknown_calls.is_empty());
    assert!(f.host.committed.lock().unwrap().is_empty());
}

#[test]
fn cancelling_a_deferred_provider_call_commits_and_delivers_its_refusal_receipt() {
    let f = Fixture::new(false);
    f.install("deferred", 1, json!({}));
    let id = f.admit("deferred", "first");
    f.host
        .transports
        .lock()
        .unwrap()
        .push_back(Err(TransportError::Refused(FlowRefusal::new(
            RefusalReason::ResourceBusy,
            "test",
            "query",
        ))));
    let mut worker = ScriptedWorker::new(vec![
        op(0, "query", json!({})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        suspended(vec![0]),
    ]);
    assert_eq!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Suspended { awaited: vec![0] }
    );
    assert!(f.host.refused.lock().unwrap().is_empty());
    f.rt.cancel(&id, "operator").unwrap();
    let refused = f.host.refused.lock().unwrap();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].run_id, id);
    assert_eq!(refused[0].position, 0);
    assert_eq!(f.rt.run(&id).unwrap().state, RunState::Cancelled);
    assert!(matches!(
        f.rt.cancel(&id, "operator"),
        Err(CoreError::WrongState {
            state: RunState::Cancelled,
            operation: "cancel",
            ..
        })
    ));
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT actor FROM audit WHERE action='cancel' AND run_id=?1",
                    [&id],
                    |r| r.get::<_, String>(0)
                )?,
                "operator"
            );
            Ok(())
        })
        .unwrap();
    f.rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM outbox WHERE kind='refusal_committed'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn owner_failure_streak_uses_history_and_its_deadline_exemption_flag() {
    struct ExpiresAfterDelivery {
        inner: ScriptedWorker,
        clock: Clock,
    }
    impl WorkerChannel for ExpiresAfterDelivery {
        fn welcome(&self) -> &basal_proto::Welcome {
            self.inner.welcome()
        }
        fn send(
            &mut self,
            message: &ParentMessage,
        ) -> std::result::Result<(), crate::ChannelError> {
            self.inner.send(message)?;
            if matches!(message,ParentMessage::Deliver(o) if o.position==0) {
                self.clock.advance(600_001);
            }
            Ok(())
        }
        fn recv(
            &mut self,
            timeout: Duration,
        ) -> std::result::Result<WorkerMessage, crate::ChannelError> {
            match self.inner.recv(timeout) {
                Err(crate::ChannelError::Closed) => {
                    std::thread::sleep(timeout.min(Duration::from_millis(1)));
                    Err(crate::ChannelError::Timeout)
                }
                other => other,
            }
        }
        fn receiver(&mut self) -> Box<dyn crate::channel::WorkerReceiver> {
            self.inner.receiver()
        }
        fn kill(&mut self) {
            self.inner.kill();
        }
    }
    let f = Fixture::new(false);
    f.install("streak", 1, json!({}));
    for trigger in ["failure-1", "failure-2", "failure-3"] {
        let id = f.admit("streak", trigger);
        let mut worker = ScriptedWorker::new(vec![WorkerMessage::Finished {
            activation_id: 0,
            result: ActivationResult::Failed(basal_proto::Failure::Script {
                message: "script failed".into(),
            }),
        }]);
        assert!(
            matches!(f.rt.activate(&id,&mut worker).unwrap(),ActivationEnd::Failed {kind,..} if kind=="script")
        );
    }
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 3);
    f.rt.store()
        .read(|c| {
            assert!(
                c.query_row(
                    "SELECT failing_episode FROM flow_owner_notice_latches WHERE flow_id='streak'",
                    [],
                    |r| r.get::<_, Option<i64>>(0)
                )?
                .is_some()
            );
            Ok(())
        })
        .unwrap();
    let id = f.admit("streak", "scope-deadline");
    f.host
        .transports
        .lock()
        .unwrap()
        .push_back(Err(TransportError::Refused(FlowRefusal::new(
            RefusalReason::NoFlowScope,
            "test",
            "query",
        ))));
    let mut worker = ExpiresAfterDelivery {
        inner: ScriptedWorker::new(vec![
            op(0, "query", json!({})),
            WorkerMessage::Blocked { awaiting: vec![0] },
        ]),
        clock: f.clock.clone(),
    };
    assert!(
        matches!(f.rt.activate(&id,&mut worker).unwrap(),ActivationEnd::Failed {kind,..} if kind=="deadline")
    );
    assert!(
        f.rt.store()
            .read(|c| Ok(sql::history(c, &id)?.unwrap().deadline_exempt))
            .unwrap()
    );
    assert_eq!(
        f.rt.flow_health().unwrap()[0].consecutive_failures,
        3,
        "an exempt deadline neither increments nor resets the streak"
    );
    let id = f.admit("streak", "recovered");
    assert!(matches!(
        f.rt.activate(&id, &mut ScriptedWorker::new(vec![finished()]))
            .unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.rt.flow_health().unwrap()[0].consecutive_failures, 0);
    f.rt.store()
        .read(|c| {
            assert!(
                c.query_row(
                    "SELECT failing_episode FROM flow_owner_notice_latches WHERE flow_id='streak'",
                    [],
                    |r| r.get::<_, Option<i64>>(0)
                )?
                .is_none()
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn secret_retirement_waits_for_live_runs_and_releases_after_terminal_commit() {
    let f = Fixture::new(false);
    f.install("secret-reader", 1, json!({}));
    let id = f.admit("secret-reader", "first");
    // The synthetic vault request carries no credentials. Retirement liveness
    // is exercised by a run admitted through the staged runtime.
    f.rt.store().write(|tx|{tx.execute("INSERT INTO outbox(at,kind,flow_id,body,retry_at) VALUES(?1,'secret.revoke_flow_grants','secret-reader',?2,?1)",params![f.clock.now_ms(),json!({"request":{"flow_install_id":"synthetic-install"},"retirement":true,"version":1}).to_string()])?;Ok(())}).unwrap();
    f.rt.drain_secret_revocations().unwrap();
    assert_eq!(f.host.revokes.load(Ordering::SeqCst), 0);
    let retry =
        f.rt.store()
            .read(|c| {
                Ok(c.query_row(
                    "SELECT retry_at FROM outbox WHERE kind='secret.revoke_flow_grants'",
                    [],
                    |r| r.get::<_, i64>(0),
                )?)
            })
            .unwrap();
    f.clock.set(retry);
    assert!(matches!(
        f.rt.activate(&id, &mut ScriptedWorker::new(vec![finished()]))
            .unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    f.rt.drain_secret_revocations().unwrap();
    assert_eq!(f.host.revokes.load(Ordering::SeqCst), 1);
    f.rt.store()
        .read(|c| {
            assert!(
                c.query_row(
                    "SELECT delivered_at FROM outbox WHERE kind='secret.revoke_flow_grants'",
                    [],
                    |r| r.get::<_, Option<i64>>(0)
                )?
                .is_some()
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_zero_classifier_ceiling_does_not_hide_measured_spend() {
    let f = Fixture::new(false);
    f.install("measured",1,json!({"ops":[{"module":"test","op":"classify"}],"model_cost_usd":{"per_run":1.0,"usd":100.0,"window":"1d"}}));
    let id = f.admit("measured", "first");
    let mut worker = ScriptedWorker::new(vec![
        op(0, "classify", json!({"max_cost_usd":0.0})),
        WorkerMessage::Blocked { awaiting: vec![0] },
        finished(),
    ]);
    assert!(matches!(
        f.rt.activate(&id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert!((f.rt.classify_cost_spent(&id).unwrap() - 0.9).abs() < 0.000001);
}

#[test]
fn explicit_null_optional_caps_keep_the_manifest_none_semantics() {
    let f = Fixture::new(false);
    f.install("null-caps", 1, json!({"llm":null,"model_cost_usd":null}));
    let id = f.admit("null-caps", "first");
    assert!(matches!(
        f.rt.activate(&id, &mut ScriptedWorker::new(vec![finished()]))
            .unwrap(),
        ActivationEnd::Succeeded { .. }
    ));
    assert_eq!(f.rt.classify_cost_spent(&id).unwrap(), 0.0);
}
