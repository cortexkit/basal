//! When the engine should next wake, read without doing any of the work: the
//! earliest schedule, run deadline, deferred-call retry, retention pass
//! (codemode runs' 24-hour pruning included), install-gate retry or pool
//! timer, and whether any consent card is open.

use crate::{Result, Runtime};

// A flow frozen by a move waits without waking the engine.
const NEXT_SCHEDULE: &str = "SELECT MIN(next_due_ms) FROM schedules WHERE state='active' \
    AND flow_id NOT IN (SELECT flow_id FROM agent_move_held_flows)";

impl Runtime {
    /// Earliest durable timer, retention pass, install-gate retry, or pool
    /// timer, on the runtime clock. Pending work is signalled separately:
    /// including it here would spin while all activation slots are occupied.
    pub fn next_wake_at(&self, pool_next: Option<i64>) -> Result<Option<i64>> {
        let timers = self.live_run_timers()?;
        // A failed close is retried by retention, not by that run's stale timers.
        let timers = || {
            timers
                .iter()
                .map(|(_, _, timer)| timer)
                .filter(|timer| !timer.close_failed)
        };
        let deadline = timers().filter_map(|timer| timer.deadline_at).min();
        let retry = timers()
            .flat_map(|timer| [timer.deferral_retry_at, timer.body_retry_at])
            .flatten()
            .min();
        let durable = self.store().read(|c| {
            let schedule: Option<i64> = c.query_row(NEXT_SCHEDULE, [], |r| r.get(0))?;
            let expiry: Option<i64> = c.query_row("SELECT MIN(expires_at) FROM template_instances t JOIN flows f USING(flow_id) WHERE f.removed=0 AND t.expiry_notified=0", [], |r| r.get(0))?;
            let codemode = crate::codemode::retention::next_sweep_at(c)?;
            let grant: Option<i64> = c.query_row("SELECT MIN(next_poll_at) FROM flow_grant_losses WHERE state='polling' AND echoable=1", [], |r| r.get(0))?;
            let moved = crate::agent_move::next_check_at(c)?;
            Ok([schedule, deadline, retry, codemode, grant, expiry, moved]
                .into_iter()
                .flatten()
                .min())
        })?;
        let retention = *self
            .shared
            .retention_next
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let now = self.config.clock.now_ms();
        let gate = self
            .shared
            .gate_backoff
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|b| b.retry_at_ms)
            .filter(|at| *at > now)
            .min();
        let owner_notice = self.owner_notice_next_wake()?;
        Ok([
            durable,
            retention.or(Some(now)),
            gate,
            pool_next,
            owner_notice,
            self.secret_revocation_next_wake()?,
        ]
        .into_iter()
        .flatten()
        .min())
    }

    /// Whether basal has any operator decision card still waiting for an
    /// answer. Install cards instead read status when core signals a committed
    /// decision. The operator-answer poller reads durable inventory so it stays
    /// correct across a restart.
    pub fn has_open_cards(&self) -> Result<bool> {
        self.store().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM decision_cards WHERE state='open')",
                [],
                |r| r.get(0),
            )?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Clock, RunState, StoredClass,
        run_records::{Body, CallIdentity, Claim, Deferred, Intent, State},
    };
    use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
    use basal_proto::{CallKind, JsonText};
    use std::sync::Arc;

    fn fixture() -> crate::runtime_authorization_regressions::TestRuntime {
        let mut f = crate::runtime_authorization_regressions::TestRuntime::new();
        Arc::make_mut(&mut f.rt.config).clock = Clock::manual(0);
        Arc::make_mut(&mut f.rt.config).install_gate = crate::InstallGate::Off;
        Arc::make_mut(&mut f.rt.config).limits.default_deadline =
            std::time::Duration::from_millis(600000);
        *f.rt.shared.retention_next.lock().unwrap() = Some(3_600_000);
        f.rt.store()
            .write(|tx| {
                tx.execute(
                    "INSERT INTO flows(flow_id,created_at) VALUES ('flow',0)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        f
    }

    fn pending(rt: &Runtime) {
        rt.fixture_run("run", "flow", 1, "return 1", "{}", State::new(0))
            .unwrap();
    }

    #[test]
    fn next_wake_combines_schedule_deadline_deferred_pool_and_retention() {
        let f = fixture();
        let rt = &f.rt;
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(3_600_000));
        assert_eq!(rt.next_wake_at(Some(900000)).unwrap(), Some(900000));
        pending(rt);
        let activation = rt.fixture_activation("run").unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(600000));
        let call = basal_proto::HostCall {
            position: 0,
            kind: CallKind::Op {
                module: "mock".into(),
                op: "send".into(),
            },
            args: JsonText::null(),
        };
        rt.fixture_append(
            "run",
            Body::Intent(Intent {
                call: CallIdentity::new(call, StoredClass::KeyedMutation),
                idempotency_key: Some("key".into()),
                frozen_request: JsonText::null(),
                attempts: 1,
                dispatch_count_window: None,
                audit_at: 0,
                reservation: None,
            }),
        )
        .unwrap();
        rt.fixture_append(
            "run",
            Body::Deferred(Deferred {
                position: 0,
                retry_not_before: 10000,
                cause: serde_json::to_string(&FlowRefusal::new(
                    RefusalReason::ResourceBusy,
                    "mock",
                    "send",
                ))
                .unwrap(),
                audit_at: 0,
                proven_unsent: true,
                refund_dispatch: true,
                attempt: None,
            }),
        )
        .unwrap();
        activation.park(&[0]).unwrap();
        rt.shared.staged_owners.lock().unwrap().remove("run");
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(10000));
        rt.config.clock.set(10000);
        assert_eq!(rt.startable().unwrap().len(), 1);
        assert_eq!(rt.run("run").unwrap().state, RunState::Pending);
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(600000));
        rt.store().write(|tx|{tx.execute("INSERT INTO schedules(flow_id,version,spec,state,anchor_ms,next_due_ms,updated_at) VALUES('flow',1,'{}','active',0,20000,0)",[])?;Ok(())}).unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(20000));
        rt.store()
            .write(|tx| {
                tx.execute("UPDATE schedules SET state='disabled'", [])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(600000));
        rt.config.clock.set(600000);
        assert_eq!(rt.enforce_deadlines().unwrap(), ["run"]);
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(3_600_000));
    }

    #[test]
    fn completed_runs_do_not_keep_deferred_or_deadline_timers_live() {
        let f = fixture();
        pending(&f.rt);
        f.rt.fixture_finish("run", RunState::Failed, None, Some(("test", "ended")))
            .unwrap();
        assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(3_600_000));
    }

    fn failed_deadline_close_fixture() -> crate::runtime_authorization_regressions::TestRuntime {
        let f = fixture();
        *f.rt.shared.retention_next.lock().unwrap() = None;
        let mut state = State::new(0);
        state.claim = Some(Claim {
            first_claim_ms: 0,
            deadline_at: 10,
        });
        for id in ["bad", "good"] {
            f.rt.fixture_run(id, "flow", 1, "return 1", "{}", state.clone())
                .unwrap();
        }
        f.rt.store()
            .write(|tx| {
                tx.execute_batch(
                    "CREATE TRIGGER reject_bad_deadline_close BEFORE INSERT ON run_history
                WHEN NEW.run_id='bad' BEGIN SELECT RAISE(ABORT,'bad run projection'); END;",
                )?;
                Ok(())
            })
            .unwrap();
        f.rt.config.clock.set(10);
        f
    }

    fn deadline_failure_audits(rt: &Runtime) -> Vec<(i64, String)> {
        rt.store().read(|c| {
            Ok(c.prepare("SELECT at,detail FROM audit WHERE action='run.deadline_close_failed' AND run_id='bad'")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        }).unwrap()
    }

    #[test]
    fn failed_deadline_close_leaves_other_runs_and_timers_unblocked() {
        let f = failed_deadline_close_fixture();
        assert_eq!(f.rt.enforce_deadlines().unwrap(), ["good"]);
        assert_eq!(f.rt.run("good").unwrap().state, RunState::Failed);
        let due = f.rt.shared.retention_next.lock().unwrap().unwrap();
        assert_eq!(due, 3_600_010);
        assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(due));
        assert!(f.rt.staged_startable().unwrap().is_empty());
        let audits = deadline_failure_audits(&f.rt);
        assert_eq!(audits.len(), 1);
        assert!(audits[0].1.contains("bad run projection"));
    }

    #[test]
    fn failed_deadline_close_retries_only_on_retention_without_reauditing() {
        let f = failed_deadline_close_fixture();
        assert_eq!(f.rt.enforce_deadlines().unwrap(), ["good"]);
        let audits = deadline_failure_audits(&f.rt);
        assert_eq!(audits.len(), 1);
        f.rt.config.clock.set(3_600_010);
        assert!(f.rt.enforce_deadlines().unwrap().is_empty());
        assert_eq!(deadline_failure_audits(&f.rt), audits);
        f.rt.store()
            .write(|tx| {
                tx.execute_batch("DROP TRIGGER reject_bad_deadline_close")?;
                Ok(())
            })
            .unwrap();
        f.rt.config.clock.set(3_600_011);
        assert!(f.rt.enforce_deadlines().unwrap().is_empty());
        assert!(!f.rt.run("bad").unwrap().state.is_terminal());
        f.rt.config.clock.set(7_200_010);
        assert_eq!(f.rt.enforce_deadlines().unwrap(), ["bad"]);
        assert_eq!(deadline_failure_audits(&f.rt), audits);
    }

    #[test]
    fn next_schedule_uses_the_due_index() {
        let conn = crate::runtime_authorization_regressions::connection();
        let plan: Vec<String> = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {NEXT_SCHEDULE}"))
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH schedules USING COVERING INDEX schedules_due")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|line| line.contains("SCAN schedules")),
            "{plan:?}"
        );
    }
}
