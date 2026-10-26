//! Health retained after a readiness rejection, independently of whether the
//! script caught it. The call's actual outcome still uses the issue log/mailbox.
use crate::error::Result;
use basal_host::flow_refusal::FlowRefusal;
use rusqlite::{Transaction, params};

const LIVE_RUN_VERSIONS_SQL: &str = "SELECT DISTINCT flow_version FROM runs WHERE flow_id=?1 AND state IN ('pending','running','suspended','needs_reconcile') AND flow_version IS NOT NULL";
// CROSS JOIN keeps runs on the left: SQLite first restricts work to this flow,
// then looks up its Broca calls by run_id instead of scanning all pending calls.
const PENDING_MODEL_VERSIONS_SQL: &str = "SELECT DISTINCT r.flow_version FROM runs r CROSS JOIN broca_calls b WHERE r.flow_id=?1 AND b.run_id=r.run_id AND r.flow_version IS NOT NULL AND json_extract(b.snapshot,'$.acknowledged')=0 AND COALESCE(json_extract(b.snapshot,'$.deferred'),0)=0";

impl crate::runtime::Runtime {
    pub(crate) fn configure_flow_version(
        &self,
        flow: &str,
        version: u32,
        scope: Option<basal_host::flow_scope::RegisteredScope>,
    ) {
        let _routes = self
            .shared
            .flow_routes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        self.shared.host.configure_flow(
            &basal_host::flow_scope::flow_route_key(flow, version),
            scope,
        );
    }
    /// Recovery is the only caller that has no particular flow to retire.
    pub(crate) fn release_unused_flow_routes(&self) -> Result<()> {
        let flows = self.store().read(|c| {
            Ok(c.prepare("SELECT flow_id FROM flows")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        for flow in flows {
            self.release_unused_flow_routes_for(&flow)?;
        }
        Ok(())
    }

    pub(crate) fn release_unused_flow_routes_for(&self, flow: &str) -> Result<()> {
        let _routes = self
            .shared
            .flow_routes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut live = self.store().read(|c| {
            let mut live = std::collections::HashSet::new();
            let installed = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM flows WHERE flow_id=?1 AND removed=0)",
                [flow],
                |r| r.get::<_, bool>(0),
            )?;
            if installed
                && let Some(approved) = crate::install::approved(c, flow)?
                && crate::install::revocation(c, flow, approved.version)?.is_none()
            {
                live.insert(basal_host::flow_scope::flow_route_key(
                    flow,
                    approved.version,
                ));
            }
            for sql in [LIVE_RUN_VERSIONS_SQL, PENDING_MODEL_VERSIONS_SQL] {
                for version in c.prepare(sql)?.query_map([flow], |r| r.get::<_, u32>(0))? {
                    live.insert(basal_host::flow_scope::flow_route_key(flow, version?));
                }
            }
            Ok(live)
        })?;
        // Read dispatch ownership after durable liveness. A cancellation may
        // have ended a run while its provider thread still owns the route.
        // Registration can run under the store writer, so the in-flight mutex
        // must not be held while querying SQLite.
        for route in self
            .shared
            .inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            if route.flow_id == flow
                && let Some(version) = route.version
            {
                live.insert(basal_host::flow_scope::flow_route_key(flow, version));
            }
        }
        // Configuration is serialized with retirement, but daemon closes hold
        // neither the store nor the dispatch mutex.
        self.shared.host.retain_flow_routes(flow, &live);
        Ok(())
    }
}

#[cfg(test)]
#[path = "flow_scope_tests.rs"]
mod tests;

pub(crate) fn health(tx: &Transaction, flow: &str, refusal: &FlowRefusal) -> Result<()> {
    let value = serde_json::json!({"reason":refusal.reason,"provider":refusal.provider,"action":refusal.action,"message":refusal.message(),"detail":refusal.detail});
    tx.execute(
        "UPDATE flows SET scope_health=?2 WHERE flow_id=?1",
        params![flow, value.to_string()],
    )?;
    Ok(())
}
