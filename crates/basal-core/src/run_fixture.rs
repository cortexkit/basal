//! Fixture construction through the real run-log and terminal interfaces.
//! These helpers seed recovered facts; no SQLite execution state is recreated.
use std::sync::Arc;
use serde_json::{Value,json};
use crate::{CoreError,Result,Runtime,run_records::{Body,Record,Records,State},run_log::RecoveredRecord,run_store_sql as sql};

/// Rig-only identity evidence, projected from the new call records.
pub struct CallInspection {pub position:u64,pub flow_id:String,pub op:String,pub args_digest:[u8;32],pub outcome:String}

impl Runtime {
    /// Transaction-rollback fixtures must not publish an actor for rolled-back
    /// admissions, or commit the returned unpublished live admission.
    #[cfg(test)]
    pub(crate) fn fixture_rollback_admission(&self,tx:&rusqlite::Transaction<'_>,flow:&str,id:&str,trigger:basal_proto::JsonText,ctx:&crate::admission::AdmitContext)->Result<crate::Admission>{self.staged_admission().fixture_current(tx,flow,id,trigger,ctx)}

    /// Closed successes have no production detail. Rigs may inspect their own
    /// disposable projection after a restart without changing execution reads.
    pub fn fixture_calls(&self,id:&str)->Result<Vec<crate::CallRow>> {
        let hosts=self.shared.staged_hosts.as_ref().expect("recovered registry");
        if !hosts.registry().retains_detail(){return Err(CoreError::Invalid("rig inspection is not enabled".into()));}
        let _=self.run(id)?;
        let calls=self.calls(id)?;
        if !calls.is_empty()||self.staged_snapshot(id).is_some(){return Ok(calls);}
        self.store().read(|c| {
            if sql::history_lost(c,id)?||sql::history(c,id)?.is_some_and(|h|h.history_lost){return Ok(Vec::new());}
            let mut statement=c.prepare("SELECT detail FROM rig_calls WHERE run_id=?1 ORDER BY position")?;
            let values=statement.query_map([id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            values.into_iter().map(|text|{let call:crate::run_records::CallState=serde_json::from_str(&text).map_err(|e|CoreError::Corrupt(e.to_string()))?;crate::run_readers::call(&call)}).collect()
        })
    }
    pub fn fixture_call_identities(&self,id:&str)->Result<Vec<CallInspection>> {
        let flow=self.run(id)?.flow_id;
        self.fixture_calls(id)?.into_iter().map(|call| {
            let outcome=if call.dispatch==crate::DispatchState::None {call.outcome.as_ref().filter(|o|o.settlement==basal_proto::Settlement::Rejected).and_then(|o|serde_json::from_str::<Value>(o.value.as_str()).ok()).and_then(|v|v["code"].as_str().map(str::to_owned)).unwrap_or_else(||"allowed".into())}else{"allowed".into()};
            Ok(CallInspection{position:call.position,flow_id:flow.clone(),op:call.kind.to_string(),args_digest:call.args_digest.0,outcome})
        }).collect()
    }
    /// Claim through the normal install/slot gate without a worker process.
    pub fn fixture_activation(&self,id:&str)->Result<crate::run_driver::Activation> {
        let lease=self.staged_claim(id)?.map_err(|end|CoreError::Invalid(format!("fixture could not claim run: {end:?}")))?;
        let live=self.store().read(|c|sql::live(c,id))?.ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        self.staged_driver(id)?.owned(live,lease.generation)
    }

    pub fn fixture_owned(&self,lease:&crate::runs::Lease)->Result<crate::run_driver::Activation> {
        if lease.owner!=self.config.owner{return Err(lease.lost());}
        let live=self.store().read(|c|sql::live(c,&lease.run_id))?.ok_or_else(||CoreError::NoSuchRun(lease.run_id.clone()))?;
        self.staged_driver(&lease.run_id)?.owned(live,lease.generation)
    }
    pub fn fixture_release_routes(&self,flow:&str)->Result<()> {self.release_unused_flow_routes_for(flow)}
    pub fn fixture_routing(&self,id:&str)->Option<crate::run_store::Routing> {
        self.shared.staged_hosts.as_ref()?.registry().get(id).map(|r|r.routing())
    }

    pub fn retain_detail_for_test(&self)->Result<()> {
        self.shared.staged_hosts.as_ref().expect("recovered registry").registry().retain_detail_for_test(self.shared.store.clone())
    }

    /// Seed an already-admitted run for recovery and control-plane fixtures.
    /// Its snapshot is durable before admission is published, just as cutover's.
    pub fn fixture_run(&self,id:&str,flow:&str,version:u32,script:&str,manifest:&str,state:State)->Result<sql::LiveRun> {
        let hosts=self.shared.staged_hosts.as_ref().expect("recovered registry");
        if self.store().read(|c|sql::identity(c,id))?.is_some(){return Err(CoreError::Invalid("fixture run already exists".into()));}
        let generation=state.generation;
        let active=state.claim.is_some()&&!state.parked;
        let base=hosts.watermark.synced_lsn();
        let identity=self.store().write(|tx| {
            let hash=crate::ids::code_hash(script,manifest);
            let snapshot=crate::runs::snapshot(tx,flow,Some(version),&hash,script,manifest)?;
            Ok(sql::RunIdentity{run_id:id.into(),flow_id:flow.into(),flow_version:version,code_hash:hash,snapshot_id:snapshot})
        })?;
        let seq=self.store().write(|tx| {
            let n:i64=tx.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='next_run'",[],|r|r.get(0))?;
            let next=n.checked_add(1).ok_or_else(||CoreError::Corrupt("fixture sequence exhausted".into()))?;
            tx.execute("UPDATE meta SET value=?1 WHERE key='next_run'",[next.to_string()])?;Ok(n)
        })?;
        let live=sql::LiveRun{run_id:id.into(),flow_id:flow.into(),flow_version:version,trigger_id:id.into(),trigger_kind:"manual".into(),trigger:json!({"kind":"manual"}),attempt:0,admitted_at:state.admitted_ms,admit_seq:seq,admit_lsn:base,deadline_ms:i64::try_from(self.config.limits.default_deadline.as_millis()).map_err(|_|CoreError::Invalid("fixture deadline overflow".into()))?,self_input:Value::Null,params_input:Value::Null,grant_guards:json!([]),template_incarnation:None,end_instance:false};
        let record=Record{run_id:id.into(),generation,body:Body::Snapshot{cutoff_lsn:base.lsn,next_delivery_order:state.next_delivery_order,next_arrival_seq:state.next_arrival_seq,state:Box::new(state)}};
        let lsn=hosts.log.append(record.clone())?;hosts.log.sync_through(lsn)?;
        self.store().write(|tx| {sql::insert_live(tx,&live,&identity)?;sql::advance_log_hwm(tx,sql::EpochLsn{log_epoch:base.log_epoch,lsn})})?;
        let records=Records::recover(id.to_owned(),live.admitted_at,hosts.log.clone(),&[RecoveredRecord{lsn,record}])?;
        self.staged_driver(id)?.register(&live,records)?;
        if active {self.shared.staged_owners.lock().unwrap().insert(id.into(),(generation,self.config.owner.clone()));}
        Ok(live)
    }

    pub fn fixture_append(&self,id:&str,body:Body)->Result<()> {
        let hosts=self.shared.staged_hosts.as_ref().expect("recovered registry");
        let budgets=hosts.budgets_for(id)?;
        let actor=hosts.registry().get(id).ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        actor.enter().map_err(|_|CoreError::Unavailable("fixture run is closing".into()))?.account(|records| {
            records.append(records.state().generation,body)?;
            Ok(((),crate::run_driver::accounting::rows(records.state(),&budgets)?))
        })
    }

    pub fn fixture_prepare(&self,id:&str,call:&basal_proto::HostCall)->Result<crate::run_driver::Prepared> {
        let live=self.store().read(|c|sql::live(c,id))?.ok_or_else(||CoreError::NoSuchRun(id.into()))?;let state=self.staged_snapshot(id).ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        let services=super::Services{shared:Arc::downgrade(&self.shared),config:self.config.clone(),acknowledgements:Default::default(),audit_actor:None};
        crate::run_driver::Services::prepare(&services,&live,call,&state)
    }
    /// Recover and send only calls whose durable identities authorize a retry.
    pub fn fixture_dispatch_pending(&self,id:&str)->Result<()> {
        let live=self.store().read(|c|sql::live(c,id))?.ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        let driver=self.staged_driver(id)?;
        let generation=self.staged_snapshot(id).ok_or_else(||CoreError::NoSuchRun(id.into()))?.generation;
        let activation=driver.owned(live,generation)?;
        loop {
            let dispatches=activation.recover_dispatches()?;
            if dispatches.is_empty(){break;}
            for dispatch in dispatches {dispatch.send()?;}
        }
        Ok(())
    }

    pub fn fixture_control_failure(&self,id:&str)->Result<bool> {
        let state=self.staged_snapshot(id).ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        if let Some((kind,detail))=crate::run_driver::control_failure(&state){self.fixture_finish(id,crate::RunState::Failed,None,Some((kind,&detail)))?;return Ok(true);}
        Ok(false)
    }
    /// Exercise the terminal projection directly for controlled summary fixtures.
    pub fn fixture_finish(&self,id:&str,state:crate::RunState,result:Option<Value>,error:Option<(&str,&str)>)->Result<()> {
        self.fixture_close(id,state,result,error)?;self.flush_refusals()
    }
    pub fn fixture_close(&self,id:&str,state:crate::RunState,result:Option<Value>,error:Option<(&str,&str)>)->Result<()> {
        let hosts=self.shared.staged_hosts.as_ref().expect("recovered registry");
        let live=self.store().read(|c|sql::live(c,id))?.ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        let actor=hosts.registry().get(id).ok_or_else(||CoreError::NoSuchRun(id.into()))?;
        let services=Arc::new(super::Services{shared:Arc::downgrade(&self.shared),config:self.config.clone(),acknowledgements:Default::default(),audit_actor:None});
        let mut terminal=crate::run_driver::Terminal::new(live,state,self.config.clock.now_ms(),hosts.budgets_for(id)?,services.clone());
        terminal.result=result;
        if let Some((kind,detail))=error{terminal.error_kind=Some(kind.into());terminal.error_detail=Some(detail.into());}
        hosts.registry().close(&actor,&terminal,services.as_ref())?;
        actor.closed_hint(terminal.commit_seq());
        Ok(())
    }
}
