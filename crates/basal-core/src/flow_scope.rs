//! Health retained after a readiness rejection, independently of whether the
//! script caught it. The call's actual outcome still uses the issue log/mailbox.
use crate::error::Result;
use basal_host::flow_refusal::FlowRefusal;
use rusqlite::{Transaction, params};

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
    pub(crate) fn release_unused_flow_routes(&self) -> Result<()> {
        let _routes = self
            .shared
            .flow_routes
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut live = self.store().read(|c| {
            let mut live = std::collections::HashSet::new();
            let mut flows = c.prepare("SELECT flow_id FROM flows WHERE removed=0")?;
            for flow in flows.query_map([], |r| r.get::<_, String>(0))? {
                let flow = flow?;
                if let Some(approved) = crate::install::approved(c, &flow)?
                    && crate::install::revocation(c, &flow, approved.version)?.is_none()
                {
                    live.insert(basal_host::flow_scope::flow_route_key(&flow, approved.version));
                }
            }
            let mut runs = c.prepare("SELECT flow_id,flow_version FROM runs r WHERE flow_version IS NOT NULL AND (state NOT IN ('succeeded','failed','engine_mismatch','cancelled') OR EXISTS(SELECT 1 FROM broca_calls b WHERE b.run_id=r.run_id AND json_extract(b.snapshot,'$.acknowledged')=0 AND COALESCE(json_extract(b.snapshot,'$.deferred'),0)=0))")?;
            for row in runs.query_map([], |r| Ok((r.get::<_, String>(0)?,r.get::<_, u32>(1)?)))? {
                let (flow, version) = row?;
                live.insert(basal_host::flow_scope::flow_route_key(&flow, version));
            }
            Ok(live)
        })?;
        // Read dispatch threads after the durable run snapshot: cancellation
        // can end a run while its provider call still owns a route. Never hold
        // the in-flight mutex while reading the store; dispatch registers under
        // the store writer in some paths.
        let inflight: std::collections::HashSet<_> = self
            .shared
            .inflight
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, _)| id.clone())
            .collect();
        for run in inflight {
            let run = self.run(&run)?;
            if let Some(version) = run.flow_version {
                live.insert(basal_host::flow_scope::flow_route_key(
                    &run.flow_id,
                    version,
                ));
            }
        }
        // Configuration is serialized with this snapshot, but closing daemon
        // handles must not hold either the store or the dispatch mutex.
        self.shared.host.retain_flow_routes(&live);
        Ok(())
    }
}

pub(crate) fn health(tx: &Transaction, flow: &str, refusal: &FlowRefusal) -> Result<()> {
    let value = serde_json::json!({"reason":refusal.reason,"provider":refusal.provider,"action":refusal.action,"message":refusal.message(),"detail":refusal.detail});
    tx.execute(
        "UPDATE flows SET scope_health=?2 WHERE flow_id=?1",
        params![flow, value.to_string()],
    )?;
    Ok(())
}
