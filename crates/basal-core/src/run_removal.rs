//! Durable removal work captured with the control-plane removal transaction.
use crate::{Boundary, CoreError, Result, Runtime, Step, run_store_sql as sql};
use rusqlite::{Connection, Transaction, params};

pub(crate) const KIND: &str = "run.removed";

fn queue(tx: &Transaction<'_>, flow: &str, id: &str, now: i64) -> Result<()> {
    tx.execute("INSERT INTO outbox(at,kind,flow_id,body,retry_at) SELECT ?1,?2,?3,json_object('run_id',?4),?1 WHERE NOT EXISTS(SELECT 1 FROM outbox WHERE kind='run.removed' AND delivered_at IS NULL AND CAST(json_extract(body,'$.run_id') AS TEXT)=?4)",params![now,KIND,flow,id])?;
    Ok(())
}

pub(crate) fn capture(tx: &Transaction<'_>, flow: &str, now: i64) -> Result<Vec<String>> {
    let ids = tx
        .prepare("SELECT run_id FROM live_runs WHERE flow_id=?1 ORDER BY admit_seq,run_id")?
        .query_map([flow], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in &ids {
        queue(tx, flow, id, now)?;
    }
    Ok(ids)
}

pub(crate) fn removed(c: &Connection, live: &sql::LiveRun) -> Result<bool> {
    let pending:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE kind='run.removed' AND delivered_at IS NULL AND CAST(json_extract(body,'$.run_id') AS TEXT)=?1)",[&live.run_id],|r|r.get(0))?;
    if pending || crate::packages::removed(c, &live.flow_id)? {
        return Ok(true);
    }
    if let Some(expected) = live.template_incarnation {
        use rusqlite::OptionalExtension;
        let current: Option<i64> = c
            .query_row(
                "SELECT incarnation FROM template_instances WHERE flow_id=?1",
                [&live.flow_id],
                |r| r.get(0),
            )
            .optional()?;
        return Ok(current != Some(expected));
    }
    Ok(false)
}

impl Runtime {
    /// Older stores predate the durable removal queue. Before attachment, bind
    /// their removed/stale-incarnation rows to immutable run IDs as well.
    pub(crate) fn capture_existing_removals(&self) -> Result<()> {
        let rows=self.store().read(|c|Ok(c.prepare("SELECT r.run_id,r.flow_id FROM live_runs r JOIN flows f USING(flow_id) LEFT JOIN template_instances t USING(flow_id) WHERE (f.removed=1 OR (r.template_incarnation IS NOT NULL AND (t.incarnation IS NULL OR t.incarnation<>r.template_incarnation))) AND NOT EXISTS(SELECT 1 FROM outbox q WHERE q.kind='run.removed' AND q.delivered_at IS NULL AND CAST(json_extract(q.body,'$.run_id') AS TEXT)=r.run_id) ORDER BY r.admit_seq,r.run_id")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?))?;
        if !rows.is_empty() {
            self.store().write(|tx| {
                for (id, flow) in &rows {
                    queue(tx, flow, id, self.config.clock.now_ms())?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }
    pub(crate) fn removal_captured(&self, ids: &[String]) -> Result<()> {
        if let Some(id) = ids.first()
            && self.shared.hooks.at(id, &Boundary::RemovalCaptured) == Step::Crash
        {
            self.store().cut();
            return Err(CoreError::Cut);
        }
        Ok(())
    }

    /// Removal owns the captured immutable run IDs, not the current removed
    /// flag. No pre-seal recheck of that mutable flag can undo a committed
    /// removal decision. A later re-instantiation cannot redirect it to new runs.
    /// RunStore closes/drains the old actors; terminal incarnation
    /// fences protect a replacement's values and lifecycle effects.
    pub(crate) fn resume_run_removals(&self) -> Result<()> {
        let _drain = match self.shared.removal_drain.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(()),
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        loop {
            let pending=self.store().read(|c|Ok(c.prepare("SELECT seq,json_extract(body,'$.run_id') FROM outbox WHERE kind='run.removed' AND delivered_at IS NULL ORDER BY seq LIMIT 64")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?))?;
            if pending.is_empty() {
                return Ok(());
            }
            let mut deferred = false;
            for (seq, id) in pending {
                let live = self
                    .store()
                    .read(|c| Ok((sql::live(c, &id)?, sql::history_lost(c, &id)?)))?;
                if live.0.is_some() {
                    if live.1 {
                        self.shared
                            .staged_hosts
                            .as_ref()
                            .expect("recovered registry")
                            .cancel_history_lost(&id, "removal", self.config.clock.now_ms())?;
                    } else {
                        match self.staged_finish(
                            &id,
                            crate::RunState::Cancelled,
                            Some("removed"),
                            Some("the flow instance was removed"),
                        ) {
                            Ok(()) => {}
                            Err(error) => {
                                if self.store().read(|c| sql::history(c, &id))?.is_none() {
                                    let busy = self
                                        .shared
                                        .staged_hosts
                                        .as_ref()
                                        .expect("recovered registry")
                                        .registry()
                                        .get(&id)
                                        .map(|r| r.close_busy())
                                        .transpose()?
                                        .unwrap_or(false);
                                    if busy {
                                        deferred = true;
                                        continue;
                                    }
                                    if self.store().read(|c| sql::history(c, &id))?.is_none() {
                                        return Err(error);
                                    }
                                }
                            }
                        }
                    }
                }
                self.store().write(|tx|{tx.execute("UPDATE outbox SET delivered_at=?2 WHERE seq=?1 AND kind=?3 AND delivered_at IS NULL",params![seq,self.config.clock.now_ms(),KIND])?;Ok(())})?;
            }
            if deferred {
                return Ok(());
            }
        }
    }
    pub(crate) fn removals_complete(&self) -> Result<bool> {
        self.store().read(|c|Ok(!c.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE kind='run.removed' AND delivered_at IS NULL)",[],|r|r.get::<_,bool>(0))?))
    }
}

pub(crate) fn same_incarnation(c: &Connection, flow: &str, expected: Option<i64>) -> Result<bool> {
    use rusqlite::OptionalExtension;
    let Some(expected) = expected else {
        return Ok(true);
    };
    let current: Option<i64> = c
        .query_row(
            "SELECT incarnation FROM template_instances WHERE flow_id=?1",
            [flow],
            |r| r.get(0),
        )
        .optional()?;
    Ok(current == Some(expected))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Arc;
    struct Cut;
    impl crate::Hooks for Cut {
        fn at(&self, _: &str, b: &Boundary) -> Step {
            if *b == Boundary::RemovalCaptured {
                Step::Crash
            } else {
                Step::Continue
            }
        }
    }
    pub(crate) fn crash_case(boundary: Boundary) {
        assert_eq!(boundary, Boundary::RemovalCaptured);
        let root = crate::test_support::timestamp_scratch("removal", "captured");
        let path = root.join("store.db");
        let config = crate::Config {
            clock: crate::Clock::manual(100),
            install_gate: crate::InstallGate::Off,
            auto_resume: false,
            ..crate::Config::default()
        };
        let store = Arc::new(crate::Store::open(&path, crate::Durability::throwaway()).unwrap());
        let rt = Runtime::new(
            store.clone(),
            Arc::new(basal_host::mock::MockHost::new()),
            Arc::new(basal_host::MockCatalog::standard()),
            Arc::new(Cut),
            None,
            config.clone(),
        )
        .unwrap();
        let flow = crate::packages::instance_id("pkg", "owner");
        rt.store().write(|tx|{tx.execute("INSERT INTO flows(flow_id,owner,package,created_at) VALUES(?1,'owner','pkg',0)",[&flow])?;Ok(())}).unwrap();
        rt.fixture_run(
            "old",
            &flow,
            1,
            "return 1",
            "{}",
            crate::run_records::State::new(0),
        )
        .unwrap();
        assert!(matches!(
            rt.remove_instance("pkg", "owner", 1),
            Err(crate::packages::PackageError::Store(CoreError::Cut))
        ));
        drop(rt);
        drop(store);
        let store = Arc::new(crate::Store::open(&path, crate::Durability::throwaway()).unwrap());
        let rt = Runtime::new(
            store.clone(),
            Arc::new(basal_host::mock::MockHost::new()),
            Arc::new(basal_host::MockCatalog::standard()),
            Arc::new(crate::NoHooks),
            None,
            config,
        )
        .unwrap();
        assert_eq!(rt.run("old").unwrap().state, crate::RunState::Cancelled);
        assert_eq!(
            rt.run("old").unwrap().error_kind.as_deref(),
            Some("removed")
        );
        assert_eq!(rt.activation_count("old").unwrap(), 0);
        rt.store().read(|c|{assert_eq!(c.query_row("SELECT COUNT(*) FROM outbox WHERE kind='run.removed' AND delivered_at IS NULL",[],|r|r.get::<_,i64>(0))?,0);crate::run_cutover::check_broca_ownership(c)}).unwrap();
        drop(rt);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn committed_removal_resumes_after_a_crash_before_actor_closes() {
        crash_case(Boundary::RemovalCaptured);
    }
}
