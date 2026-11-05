use std::{path::PathBuf,sync::{Arc,Mutex,mpsc,atomic::{AtomicU64,Ordering}},time::{Duration,Instant}};
use basal_core::{Runtime,Store,Durability,Config,Clock,InstallGate,Admission,ActivationEnd,RevokeCause,RunState,NoHooks,Boundary,Hooks,Step};
use basal_core::run_driver::Issued;
use basal_core::run_records::Body;
use basal_host::{Host,CallClass,CallRequest,CompletionSink,Dispatched,HostOutcome,TransportError,catalog::{MockCatalog,OpDecl,OpKind}};
use basal_proto::{CallKind,HostCall,JsonText,Primitive};
use serde_json::{Value,json};
const OWNER:&str="agent_47120287c700722b";
static NEXT:AtomicU64=AtomicU64::new(0);
#[derive(Default)]
struct Observe {calls:Mutex<Vec<CallRequest>>,accept:std::sync::atomic::AtomicBool,retained:Mutex<Vec<std::collections::HashSet<String>>>,revokes:std::sync::atomic::AtomicUsize}
impl Host for Observe {
    fn classify(&self,_:&CallKind)->CallClass{CallClass::Query}
    fn dispatch(&self,call:&CallRequest)->Result<Dispatched,TransportError>{self.calls.lock().unwrap().push(call.clone());if self.accept.load(Ordering::SeqCst){return Ok(Dispatched::Accepted{handle:format!("handle:{}",call.run_id)});}Ok(Dispatched::Completed(HostOutcome::fulfilled(JsonText::new("7").unwrap())))}
    fn retain_flow_routes(&self,_:&str,live:&std::collections::HashSet<String>){self.retained.lock().unwrap().push(live.clone());}
    fn secret_revoke(&self,_:Value)->Result<Value,basal_host::transport::WireError>{self.revokes.fetch_add(1,Ordering::SeqCst);Ok(json!({"status":"revoked","grants":0}))}
    fn attach(&self,_:Arc<dyn CompletionSink>){}
    fn now_ms(&self)->f64{10_000.0}fn random(&self)->f64{0.5}
}
struct Fixture {rt:Option<Runtime>,store:Option<Arc<Store>>,host:Arc<Observe>,root:PathBuf,flow:String,manifest:String}
impl Fixture {
    fn new(hooks:Arc<dyn Hooks>)->Self {
        let root=PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/scratch").join(format!("removal-domain-{}-{}",std::process::id(),NEXT.fetch_add(1,Ordering::Relaxed)));
        std::fs::create_dir_all(&root).unwrap();let store=Arc::new(Store::open(root.join("store.db"),Durability::throwaway()).unwrap());
        let host=Arc::new(Observe::default());let catalog=Arc::new(MockCatalog::standard());catalog.add_named_agent(OWNER,"Owner");catalog.set_flow_capable("test",true);catalog.set_op("test","query",OpDecl{kind:Some(OpKind::Query),cause_echo:true,shell_capable:false});
        let rt=Runtime::new(store.clone(),host.clone(),catalog,hooks,None,Config{clock:Clock::manual(10_000),install_gate:InstallGate::Off,auto_resume:false,..Config::default()}).unwrap();
        let manifest=json!({"id":"watch","version":1,"purpose":"Fence removed template runs from replacements","trigger":{"schedule":{"interval":"1h"}},"ops":[{"module":"test","op":"query"}],"sinks":[{"agent":"$self","digest_max":"wake"}],"params":{},"template":{"max_per_agent":2,"max_lifetime":"1d"}}).to_string();
        rt.register_package("return 7;",&manifest).unwrap();let flow=rt.instantiate("watch",1,OWNER,&json!({}),"2h").unwrap()["flow_id"].as_str().unwrap().to_owned();
        Self{rt:Some(rt),store:Some(store),host,root,flow,manifest}
    }
    fn rt(&self)->&Runtime{self.rt.as_ref().unwrap()}
    fn admit(&self,key:&str)->String{match self.rt().admit_trigger(&self.flow,key,JsonText::null()).unwrap(){Admission::Admitted{run_id}=>run_id,other=>panic!("{other:?}")}}
    fn kv(&self)->Option<String>{use rusqlite::OptionalExtension;self.rt().store().read(|c|Ok(c.query_row("SELECT value FROM kv WHERE flow_id=?1 AND key='value'",[&self.flow],|r|r.get(0)).optional()?)).unwrap()}
    fn schedule(&self)->Value {self.rt().store().read(|c|Ok(c.query_row("SELECT version,spec,state,anchor_ms,next_due_ms,last_fired_ms,updated_at FROM schedules WHERE flow_id=?1",[&self.flow],|r|Ok(json!([r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,Option<i64>>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,i64>(6)?])))?)).unwrap()}
}
impl Drop for Fixture{fn drop(&mut self){self.rt.take();self.store.take();let _=std::fs::remove_dir_all(&self.root);}}
fn call(position:u64,primitive:Primitive,args:Value)->HostCall{HostCall{position,kind:CallKind::Primitive(primitive),args:JsonText::new(args.to_string()).unwrap()}}
struct NeverWorker;
impl basal_core::channel::WorkerChannel for NeverWorker {
    fn welcome(&self)->&basal_proto::Welcome{panic!("removed run requested a worker")}
    fn send(&mut self,_:&basal_proto::ParentMessage)->Result<(),basal_core::channel::ChannelError>{panic!("removed run sent a frame")}
    fn recv(&mut self,_:Duration)->Result<basal_proto::WorkerMessage,basal_core::channel::ChannelError>{panic!("removed run read a frame")}
    fn receiver(&mut self)->Box<dyn basal_core::channel::WorkerReceiver>{panic!("removed run created a receiver")}
    fn kill(&mut self){}
}
struct Pause {boundary:Boundary,reached:mpsc::Sender<()>,resume:Mutex<mpsc::Receiver<()>>}
impl Hooks for Pause{fn at(&self,_:&str,b:&Boundary)->Step{if *b==self.boundary{self.reached.send(()).unwrap();self.resume.lock().unwrap().recv_timeout(Duration::from_secs(10)).unwrap();}Step::Continue}}
fn pause(boundary:Boundary)->(Arc<Pause>,mpsc::Receiver<()>,mpsc::Sender<()>){let(reached,at)=mpsc::channel();let(resume,wait)=mpsc::channel();(Arc::new(Pause{boundary,reached,resume:Mutex::new(wait)}),at,resume)}
fn wait(mut condition:impl FnMut()->bool){let deadline=Instant::now()+Duration::from_secs(10);while !condition(){assert!(Instant::now()<deadline,"removal did not reach its deterministic barrier");std::thread::yield_now();}}

#[test]
fn removal_before_claim_cancels_without_claiming_and_reports_removed(){
    let f=Fixture::new(Arc::new(NoHooks));let id=f.admit("old");assert_eq!(f.rt().end_instance(&f.flow).unwrap()["ended"],true);
    let run=f.rt().run(&id).unwrap();assert_eq!(run.state,RunState::Cancelled);assert_eq!(run.error_kind.as_deref(),Some("removed"));assert_eq!(f.rt().activation_count(&id).unwrap(),0);
    assert!(matches!(f.rt().activate(&id,&mut NeverWorker).unwrap(),ActivationEnd::Revoked{cause:RevokeCause::Removed,..}));assert!(f.host.calls.lock().unwrap().is_empty());
}

#[test]
fn removal_seals_a_held_activation_before_any_later_value_or_digest_effect(){
    let (hooks,at,resume)=pause(Boundary::DriverSynced{position:0});let f=Fixture::new(hooks);let id=f.admit("held");let activation=f.rt().fixture_activation(&id).unwrap();f.rt().fixture_append(&id,Body::ScriptStarted).unwrap();
    std::thread::scope(|scope|{
        let work=scope.spawn(||{
            let Issued::Dispatch(dispatch)=activation.issue(HostCall{position:0,kind:CallKind::Op{module:"test".into(),op:"query".into()},args:JsonText::new("{}").unwrap()}).unwrap() else{panic!("missing query dispatch")};dispatch.send().unwrap();
            assert!(activation.issue(call(1,Primitive::KvSet,json!({"key":"value","value":7}))).is_err());
            assert!(activation.issue(call(2,Primitive::SinkDigest,json!({"agent":OWNER,"item":{"title":"must not send"},"action":"wake"}))).is_err());
        });
        at.recv_timeout(Duration::from_secs(10)).unwrap();let remove=scope.spawn(||f.rt().end_instance(&f.flow));
        wait(||f.rt().fixture_routing(&id)==Some(basal_core::run_store::Routing::Closing));resume.send(()).unwrap();work.join().unwrap();remove.join().unwrap().unwrap();
    });
    assert_eq!(f.rt().run(&id).unwrap().state,RunState::Cancelled);assert_eq!(f.rt().run(&id).unwrap().error_kind.as_deref(),Some("removed"));assert_eq!(f.kv(),None);assert!(f.host.calls.lock().unwrap().is_empty());
}

#[test]
fn an_already_closing_old_incarnation_cannot_change_its_replacement(){
    let (hooks,at,resume)=pause(Boundary::RunStoreBuilt);let f=Fixture::new(hooks);
    // Two prior failures make an unguarded old completion open an owner notice.
    // They are written before arming the close barrier used by the old run.
    // The barrier's receiver is released for each of these controlled closes.
    for key in ["first","second"] {
        let id=f.admit(key);
        std::thread::scope(|scope|{let close=scope.spawn(||f.rt().fixture_finish(&id,RunState::Failed,None,Some(("bad","old failure"))));at.recv_timeout(Duration::from_secs(10)).unwrap();resume.send(()).unwrap();close.join().unwrap().unwrap();});
    }
    let id=f.admit("old");let activation=f.rt().fixture_activation(&id).unwrap();
    assert!(matches!(activation.issue(call(0,Primitive::KvSet,json!({"key":"value","value":7}))).unwrap(),Issued::Pending));
    assert!(activation.release(&[0]).unwrap().is_some());
    assert!(matches!(activation.issue(call(1,Primitive::InstanceEnd,json!({}))).unwrap(),Issued::Pending));assert!(activation.release(&[1]).unwrap().is_some());
    std::thread::scope(|scope|{
        let close=scope.spawn(||f.rt().fixture_finish(&id,RunState::Failed,None,Some(("bad","old failure"))));at.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(f.rt().end_instance(&f.flow).unwrap()["ended"],true);
        f.rt().instantiate("watch",1,OWNER,&json!({}),"2h").unwrap();
        f.rt().store().write(|tx|{tx.execute("INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES(?1,'value','99',7,1)",[&f.flow])?;Ok(())}).unwrap();
        let schedule=f.schedule();let event_seq:i64=f.rt().store().read(|c|Ok(c.query_row("SELECT MAX(seq) FROM template_event_outbox WHERE flow_id=?1",[&f.flow],|r|r.get(0))?)).unwrap();
        let notices:i64=f.rt().store().read(|c|Ok(c.query_row("SELECT COUNT(*) FROM outbox WHERE kind='flow.owner_notice'",[],|r|r.get(0))?)).unwrap();
        resume.send(()).unwrap();close.join().unwrap().unwrap();f.rt().startable().unwrap();
        assert_eq!(f.kv().as_deref(),Some("99"));assert_eq!(f.schedule(),schedule);
        f.rt().store().read(|c|{
            assert!(!c.query_row("SELECT removed FROM flows WHERE flow_id=?1",[&f.flow],|r|r.get::<_,bool>(0))?);
            assert_eq!(c.query_row("SELECT incarnation FROM template_instances WHERE flow_id=?1",[&f.flow],|r|r.get::<_,i64>(0))?,2);
            assert_eq!(c.query_row("SELECT COUNT(*) FROM outbox WHERE kind='flow.owner_notice'",[],|r|r.get::<_,i64>(0))?,notices);
            // No late end event means the Core consumer receives no instruction
            // to resolve the replacement's board waits.
            assert_eq!(c.query_row("SELECT COUNT(*) FROM template_event_outbox WHERE flow_id=?1 AND seq>?2 AND event_name='flow_instance_ended'",rusqlite::params![f.flow,event_seq],|r|r.get::<_,i64>(0))?,0);
            Ok(())
        }).unwrap();
    });
    assert!(activation.issue(call(2,Primitive::KvSet,json!({"key":"value","value":0}))).is_err());assert!(f.host.calls.lock().unwrap().is_empty());
}

#[test]
fn same_incarnation_failure_still_persists_its_kv_overlay(){
    let f=Fixture::new(Arc::new(NoHooks));let id=f.admit("failure");let activation=f.rt().fixture_activation(&id).unwrap();assert!(matches!(activation.issue(call(0,Primitive::KvSet,json!({"key":"value","value":7}))).unwrap(),Issued::Pending));assert!(activation.release(&[0]).unwrap().is_some());
    f.rt().fixture_finish(&id,RunState::Failed,None,Some(("bad","failure"))).unwrap();assert_eq!(f.kv().as_deref(),Some("7"));
}

#[test]
fn live_dispatch_and_closed_obligation_hold_routes_and_retired_secret_grants(){
    let (hooks,at,resume)=pause(Boundary::DriverSynced{position:0});let f=Fixture::new(hooks);f.host.accept.store(true,Ordering::SeqCst);let id=f.admit("old-version");let activation=f.rt().fixture_activation(&id).unwrap();
    let route=basal_host::flow_scope::flow_route_key(&f.flow,1);
    std::thread::scope(|scope|{
        let send=scope.spawn(||{let Issued::Dispatch(dispatch)=activation.issue(HostCall{position:0,kind:CallKind::Op{module:"test".into(),op:"query".into()},args:JsonText::new("{}").unwrap()}).unwrap() else{panic!("missing dispatch")};dispatch.send().unwrap();});
        at.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut next:Value=serde_json::from_str(&f.manifest).unwrap();next["version"]=json!(2);f.rt().register_package("return 7;",&next.to_string()).unwrap();f.rt().instantiate("watch",2,OWNER,&json!({}),"2h").unwrap();
        f.rt().store().write(|tx|{tx.execute("INSERT INTO outbox(at,kind,flow_id,body,retry_at) VALUES(?1,'secret.revoke_flow_grants',?2,?3,?1)",rusqlite::params![f.rt().config().clock.now_ms(),f.flow,json!({"request":{"flow_install_id":"synthetic-old-install"},"retirement":true,"version":1}).to_string()])?;Ok(())}).unwrap();
        f.rt().fixture_release_routes(&f.flow).unwrap();f.rt().drain_secret_revocations().unwrap();
        let kept=f.host.retained.lock().unwrap().last().unwrap().contains(&route);let revoked=f.host.revokes.load(Ordering::SeqCst);
        resume.send(()).unwrap();send.join().unwrap();
        assert_eq!((kept,revoked),(true,0),"an authorised entrant must retain both route and secret grant through its live row");
    });
    f.rt().cancel(&id,"operator").unwrap();
    let retry=||f.rt().store().read(|c|Ok(c.query_row("SELECT retry_at FROM outbox WHERE kind='secret.revoke_flow_grants'",[],|r|r.get::<_,i64>(0))?)).unwrap();
    f.rt().config().clock.set(retry());f.rt().fixture_release_routes(&f.flow).unwrap();f.rt().drain_secret_revocations().unwrap();
    assert!(f.host.retained.lock().unwrap().last().unwrap().contains(&route),"the closed run's unsettled obligation still owns its route");assert_eq!(f.host.revokes.load(Ordering::SeqCst),0);
    assert_eq!(f.rt().complete(&basal_host::Completion{run_id:id.clone(),position:0,handle:format!("handle:{id}"),outcome:HostOutcome::fulfilled(JsonText::new("7").unwrap())}).unwrap(),basal_host::CompletionAck::Accepted);
    f.rt().config().clock.set(retry());f.rt().fixture_release_routes(&f.flow).unwrap();f.rt().drain_secret_revocations().unwrap();
    assert!(!f.host.retained.lock().unwrap().last().unwrap().contains(&route));assert_eq!(f.host.revokes.load(Ordering::SeqCst),1);
}
