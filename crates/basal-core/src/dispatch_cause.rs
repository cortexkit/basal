//! Bounded diagnostics, never authority for retrying or settling a call.
use super::*;

#[derive(Clone, serde::Serialize)]
pub(super) struct DispatchCause {
    code: &'static str,
    detail: String,
}

impl DispatchCause {
    fn new(code: &'static str, detail: &str) -> Self {
        let mut end = detail.len().min(300);
        while !detail.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            code,
            detail: detail[..end].into(),
        }
    }

    pub(super) fn refusal(refusal: &basal_host::flow_refusal::FlowRefusal) -> Self {
        // Provider prose can echo arguments or secrets. Only store basal's own
        // description of the decoded reason, not the provider's message/detail.
        Self::new(refusal.reason.as_str(), refusal.message())
    }

    pub(super) fn transport(error: &TransportError) -> Self {
        match error {
            TransportError::Refused(refusal) => Self::refusal(refusal),
            TransportError::ScopeCut { .. } => {
                Self::new("scope_cut", "the scope closed during the call")
            }
            TransportError::Unavailable {
                sent: Sent::Never, ..
            } => Self::new(
                "never_sent",
                "the host could not be reached; no request was sent",
            ),
            TransportError::Unavailable {
                sent: Sent::Maybe(reason),
                ..
            } => Self::new(reason.as_str(), "the send ended without a readable outcome"),
        }
    }

    fn runtime(error: &CoreError) -> Self {
        let code = match error {
            CoreError::Store(_) => "dispatch_store_error",
            CoreError::Cut => "dispatch_store_cut",
            CoreError::Corrupt(_) => "dispatch_corrupt",
            CoreError::Unavailable(_) => "dispatch_unavailable",
            _ => "dispatch_recording_error",
        };
        Self::new(
            code,
            "basal could not durably finish dispatch; recovery must inspect the unresolved call",
        )
    }

    fn warn(&self, request: &CallRequest) {
        tracing::warn!(target: "dispatch", flow_id=%request.flow_id, run_id=%request.run_id,
            position=request.position, op=%basal_host::op_label(&request.kind),
            code=self.code, reason=%self.detail, "call dispatch needs attention; retaining the last cause");
    }
}

impl Runtime {
    /// Persist diagnostics with the existing outcome/attempt transaction. The
    /// comparison suppresses repeated warnings, including across activations.
    pub(super) fn write_dispatch_cause<T>(
        &self,
        request: &CallRequest,
        cause: &DispatchCause,
        write: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let encoded = serde_json::to_string(cause).expect("dispatch cause JSON");
        let (value, changed) = self.write_execution(|tx| {
            let value = write(tx)?;
            let changed = tx.execute(
                "UPDATE journal SET last_dispatch_cause=?3 WHERE run_id=?1 AND position=?2 AND last_dispatch_cause IS NOT ?3",
                rusqlite::params![request.run_id, request.position, encoded],
            )? > 0;
            Ok((value, changed))
        })?;
        if changed {
            cause.warn(request);
        }
        Ok(value)
    }

    pub(super) fn dispatch_failed(&self, request: &CallRequest, error: &CoreError) {
        let cause = DispatchCause::runtime(error);
        // A failed outcome transaction has rolled back. Record that failure in
        // its recovery transaction, without pretending a remote write was refused.
        if self
            .write_dispatch_cause(request, &cause, |_| Ok(()))
            .is_err()
        {
            let encoded = serde_json::to_string(&cause).expect("dispatch cause JSON");
            let key = (request.run_id.clone(), request.position);
            let mut warnings = lock(&self.shared.dispatch_warnings);
            if warnings.get(&key) != Some(&encoded) {
                // Bound memory when even the store is unavailable. One repeat
                // after eviction or restart is preferable to an unbounded log.
                if warnings.len() >= 4096 {
                    warnings.clear();
                }
                warnings.insert(key, encoded);
                cause.warn(request);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_48_adds_nullable_diagnostics_without_rebuilding_populated_v47_journal() {
        let c = rusqlite::Connection::open_in_memory().unwrap();
        for migration in crate::schema::MIGRATIONS.iter().filter(|m| m.version <= 47) {
            c.execute_batch(migration.statements).unwrap();
        }
        c.execute_batch("INSERT INTO run_snapshots VALUES (1,'f',1,zeroblob(32),'return 1','{}');
            INSERT INTO runs(run_id,flow_id,trigger_id,attempt,trigger,snapshot_id,code_hash,state,admitted_at)
            VALUES ('r','f','t',1,'{}',1,zeroblob(32),'pending',0);
            INSERT INTO journal(run_id,position,kind_code,args_digest,idempotency_key,class,dispatch,attempts,issued_generation,args_bytes)
            VALUES ('r',0,26,zeroblob(32),'key','keyed_mutation','sent',15,1,2);
            INSERT INTO journal_payloads VALUES ('r',0,'{}',NULL,NULL);").unwrap();
        let root: i64 = c
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name='journal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let migration = crate::schema::MIGRATIONS
            .iter()
            .find(|m| m.version == 48)
            .unwrap();
        assert_eq!(
            migration.statements,
            "ALTER TABLE journal ADD COLUMN last_dispatch_cause TEXT;"
        );
        c.execute_batch(migration.statements).unwrap();
        let after: i64 = c
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name='journal'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, root);
        let row: (i64, Option<String>, String) = c.query_row("SELECT attempts,last_dispatch_cause,args FROM journal_with_payloads WHERE run_id='r'", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(row, (15, None, "{}".into()));
        assert!(
            c.execute(
                "UPDATE journal SET refusal='sink_store_failed' WHERE run_id='r'",
                []
            )
            .is_err(),
            "existing deferral safety constraint changed"
        );
    }

    #[test]
    fn dispatch_cause_caps_detail_on_a_utf8_boundary() {
        let cause = DispatchCause::new("code", &format!("é{}", "🦀".repeat(100)));
        assert_eq!(cause.detail.len(), 298);
        assert!(cause.detail.ends_with('🦀'));
    }
}
