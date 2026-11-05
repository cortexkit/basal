//! Disposable SQLite evidence for rigs. No production constructor enables it.

use crate::{Result, Store, run_records::State};
use rusqlite::params;

pub(crate) fn create(store: &Store) -> Result<()> {
    store.write(|tx| {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS rig_runs(run_id TEXT PRIMARY KEY,flow_id TEXT NOT NULL,admit_seq INTEGER NOT NULL,state TEXT NOT NULL,result TEXT,error_kind TEXT,error_detail TEXT);
            CREATE TABLE IF NOT EXISTS rig_calls(run_id TEXT NOT NULL,position INTEGER NOT NULL,kind_code INTEGER NOT NULL,dispatch TEXT NOT NULL,attempts INTEGER NOT NULL,settlement TEXT,value TEXT,request TEXT,PRIMARY KEY(run_id,position));
            CREATE TABLE IF NOT EXISTS rig_activations(run_id TEXT NOT NULL,activation INTEGER NOT NULL,PRIMARY KEY(run_id,activation));")?;
        Ok(())
    })
}

pub(crate) fn write(store: &Store, id: &str, state: &State) -> Result<()> {
    store.write(|tx| {
        let run = crate::run_readers::load(tx, id, Some(state), matches!(state.phase.as_str(), "claimed" | "script"), "rig")?;
        tx.execute("INSERT INTO rig_runs VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(run_id) DO UPDATE SET state=excluded.state,result=excluded.result,error_kind=excluded.error_kind,error_detail=excluded.error_detail", params![id,run.flow_id,run.admit_seq,run.state.as_str(),run.result,run.error_kind,run.error_detail])?;
        for call in state.calls.values() {
            let row = crate::run_readers::call(call)?;
            let kind = crate::model::kind_columns(&row.kind)?.0;
            tx.execute("INSERT INTO rig_calls VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(run_id,position) DO UPDATE SET dispatch=excluded.dispatch,attempts=excluded.attempts,settlement=excluded.settlement,value=excluded.value,request=excluded.request", params![id,crate::model::position_i64(row.position)?,kind,row.dispatch.as_str(),row.attempts,row.outcome.as_ref().map(|o| crate::model::settlement_str(o.settlement)),row.outcome.as_ref().map(|o| o.value.as_str()),row.request.as_ref().map(|r|r.as_str())])?;
        }
        // The activation ordinal is taken from Activated records, never claims.
        for activation in 1..=state.activation_count {
            tx.execute("INSERT OR IGNORE INTO rig_activations VALUES(?1,?2)", params![id,crate::model::position_i64(activation)?])?;
        }
        Ok(())
    })
}
