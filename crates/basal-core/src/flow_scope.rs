//! Health retained after a readiness rejection, independently of whether the
//! script caught it. The call's actual outcome still uses the issue log/mailbox.
use crate::error::Result;
use basal_host::flow_refusal::FlowRefusal;
use rusqlite::{Transaction, params};

const LIVE_RUN_VERSIONS_SQL: &str = "SELECT DISTINCT flow_version FROM live_runs WHERE flow_id=?1";
// CROSS JOIN restricts identities to the flow before looking up saved calls.
const PENDING_MODEL_VERSIONS_SQL: &str = "SELECT DISTINCT r.flow_version FROM run_identities r CROSS JOIN broca_calls b WHERE r.flow_id=?1 AND b.run_id=r.run_id AND json_extract(b.snapshot,'$.acknowledged')=0 AND COALESCE(json_extract(b.snapshot,'$.deferred'),0)=0";
const OPEN_OBLIGATION_VERSIONS_SQL: &str = "SELECT DISTINCT h.flow_version FROM run_history h JOIN obligations o USING(run_id) WHERE h.flow_id=?1 AND o.state<>'settled'";

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
        let live = self.store().read(|c| {
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
            let versions = [
                LIVE_RUN_VERSIONS_SQL,
                PENDING_MODEL_VERSIONS_SQL,
                OPEN_OBLIGATION_VERSIONS_SQL,
            ];
            for sql in versions {
                for version in c.prepare(sql)?.query_map([flow], |r| r.get::<_, u32>(0))? {
                    live.insert(basal_host::flow_scope::flow_route_key(flow, version?));
                }
            }
            Ok(live)
        })?;
        // RunStore::close_impl drains every dispatch entrant before the terminal
        // transaction removes live_runs. No entrant can outlive its live row.
        // After close, obligations and unacknowledged Broca calls retain routes.
        // Route configuration is serialized here; host calls hold no SQLite lock.
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
