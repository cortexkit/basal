//! When the engine should next wake, read without doing any of the work: the
//! earliest schedule, run deadline, deferred-call retry, retention pass
//! (codemode runs' 24-hour pruning included), install-gate retry or pool
//! timer, and whether any consent card is open.

use crate::{Result, Runtime};

const NEXT_SCHEDULE: &str = "SELECT MIN(next_due_ms) FROM schedules WHERE state='active'";
const NEXT_DEADLINE: &str = "SELECT MIN(at) FROM (SELECT MIN(deadline_at) AS at FROM runs WHERE state='pending' AND deadline_at IS NOT NULL UNION ALL SELECT MIN(deadline_at) FROM runs WHERE state='running' AND deadline_at IS NOT NULL UNION ALL SELECT MIN(deadline_at) FROM runs WHERE state='suspended' AND deadline_at IS NOT NULL)";
const NEXT_DEFERRED: &str = "SELECT MIN(retry_not_before) FROM journal WHERE dispatch='deferred' AND retry_not_before IS NOT NULL AND EXISTS (SELECT 1 FROM runs WHERE runs.run_id=journal.run_id AND runs.state='suspended')";

impl Runtime {
    /// Earliest durable timer, retention pass, install-gate retry, or pool
    /// timer, on the runtime clock. Pending work is signalled separately:
    /// including it here would spin while all activation slots are occupied.
    pub fn next_wake_at(&self, pool_next: Option<i64>) -> Result<Option<i64>> {
        let durable = self.store().read(|c| {
            let schedule: Option<i64> = c.query_row(NEXT_SCHEDULE, [], |r| r.get(0))?;
            let deadline: Option<i64> = c.query_row(NEXT_DEADLINE, [], |r| r.get(0))?;
            let retry: Option<i64> = c.query_row(NEXT_DEFERRED, [], |r| r.get(0))?;
            let codemode = crate::codemode::retention::next_sweep_at(c)?;
            let grant: Option<i64> = c.query_row("SELECT MIN(next_poll_at) FROM flow_grant_losses WHERE state='polling' AND echoable=1", [], |r| r.get(0))?;
            Ok([schedule, deadline, retry, codemode, grant]
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
        Ok([durable, retention.or(Some(now)), gate, pool_next]
            .into_iter()
            .flatten()
            .min())
    }

    /// Whether basal has any install card or operator decision card still
    /// waiting for an answer. The consent poller reads this from the store, not
    /// from memory, so it stays correct across a restart.
    pub fn has_open_cards(&self) -> Result<bool> {
        self.store().read(|c| Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM install_cards WHERE state='pending') OR EXISTS(SELECT 1 FROM decision_cards WHERE state='open')",
            [], |r| r.get(0),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Clock, RunState, StoredClass, journal, runs};
    use basal_host::flow_refusal::{FlowRefusal, RefusalReason};
    use basal_proto::{CallKind, JsonText};
    use std::sync::Arc;

    fn fixture() -> crate::runtime_authorization_regressions::TestRuntime {
        let mut f = crate::runtime_authorization_regressions::TestRuntime::new();
        Arc::make_mut(&mut f.rt.config).clock = Clock::manual(0);
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
        rt.store().write(|tx| {
            tx.execute("INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,script,manifest,code_hash,state,admitted_at,deadline_ms) VALUES ('run','flow','trigger',1,'null','return 1','{}',zeroblob(32),'pending',0,600000)", [])?;
            Ok(())
        }).unwrap();
    }

    #[test]
    fn next_wake_combines_schedule_deadline_deferred_pool_and_retention() {
        let f = fixture();
        let rt = &f.rt;
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(3_600_000));
        assert_eq!(rt.next_wake_at(Some(900_000)).unwrap(), Some(900_000));
        pending(rt);
        let lease = rt
            .store()
            .write(|tx| runs::claim(tx, "run", "owner", 0))
            .unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(600_000));
        rt.store()
            .write(|tx| {
                journal::insert_call(
                    tx,
                    &lease,
                    "flow",
                    &journal::NewCall {
                        position: 0,
                        kind: &CallKind::Op {
                            module: "mock".into(),
                            op: "send".into(),
                        },
                        args: &JsonText::null(),
                        class: StoredClass::KeyedMutation,
                        request: None,
                    },
                )?;
                journal::defer(
                    tx,
                    "run",
                    0,
                    &FlowRefusal::new(RefusalReason::ResourceBusy, "mock", "send"),
                    10_000,
                )?;
                runs::park(tx, &lease, &[0], 0)?;
                Ok(())
            })
            .unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(10_000));
        rt.config.clock.set(10_000);
        assert_eq!(rt.startable().unwrap().len(), 1);
        assert_eq!(rt.run("run").unwrap().state, RunState::Pending);
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(600_000));
        rt.store().write(|tx| {
            tx.execute("INSERT INTO schedules(flow_id,version,spec,state,anchor_ms,next_due_ms,updated_at) VALUES ('flow',1,'{}','active',0,20000,0)", [])?;
            Ok(())
        }).unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(20_000));
        rt.store()
            .write(|tx| {
                tx.execute("UPDATE schedules SET state='disabled'", [])?;
                Ok(())
            })
            .unwrap();
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(600_000));
        rt.config.clock.set(600_000);
        assert_eq!(rt.enforce_deadlines().unwrap(), ["run"]);
        assert_eq!(rt.next_wake_at(None).unwrap(), Some(3_600_000));
    }

    #[test]
    fn completed_runs_do_not_keep_deferred_or_deadline_timers_live() {
        let f = fixture();
        pending(&f.rt);
        f.rt.store()
            .write(|tx| {
                tx.execute(
                    "UPDATE runs SET state='failed', deadline_at=600000, ended_at=0",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(f.rt.next_wake_at(None).unwrap(), Some(3_600_000));
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

    #[test]
    fn timer_plans_use_the_ordered_wake_indexes() {
        // The full migrated schema: MIN must stop at the first live timer
        // instead of visiting every active run or every deferred call.
        let conn = crate::runtime_authorization_regressions::connection();
        for (query, index) in [
            (NEXT_DEADLINE, "runs_state_deadline"),
            (NEXT_DEFERRED, "journal_deferred_retry"),
        ] {
            let plan: Vec<String> = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
                .unwrap()
                .query_map([], |r| r.get(3))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert!(
                plan.iter()
                    .any(|line| line.contains("SEARCH") && line.contains(index)),
                "{plan:?}"
            );
        }
    }
}
