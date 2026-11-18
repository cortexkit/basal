//! The `agent-data/v1` participant end to end, on two computers that share
//! one door. Requests are Entorhinal's own published samples
//! (`test-vectors/entorhinal-agent-data-requests-v1`), replayed with basal's
//! ref kind.

use std::sync::Arc;

use basal_core::{Config, Durability, InstallRequest, NoHooks};
use basal_host::door::FakeDoor;
use basal_host::mock::MockHost;
use basal_host::{InstallStatus, MockCatalog, MockConsent};
use cortexkit_role_agent_data::Op;
use cortexkit_role_agent_data::errors::{ErrorBody, RefusalCode, allowed_refusals};
use serde_json::{Value, json};

use super::*;
use crate::caller::Caller;
use crate::dryrun::DryRunConfig;
use crate::engine::EngineConfig;
use crate::module::{Hosts, Module, ModuleConfig};
use crate::pool::{PoolConfig, Spawn};
use crate::process::WorkerLaunch;
use crate::process::{SpawnError, WorkerProcess};

const AGENT: &str = "agent_0123456789abcdef";
const SAMPLES: &str =
    include_str!("../../../test-vectors/entorhinal-agent-data-requests-v1/new-format.json");

struct NoWorkers;
impl Spawn for NoWorkers {
    fn spawn(&self) -> Result<WorkerProcess, SpawnError> {
        Err(SpawnError::Exec("no workers in agent-data tests".into()))
    }
}

struct Computer {
    module: Module,
    host: Arc<MockHost>,
    dir: std::path::PathBuf,
}

impl Drop for Computer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Computer {
    fn new(tag: &str, door: &Arc<FakeDoor>) -> Self {
        let dir = basal_core::test_support::timestamp_scratch("agent-data", tag);
        std::fs::create_dir_all(&dir).unwrap();
        let host = Arc::new(MockHost::new());
        let mut pool = PoolConfig::new("missing", WorkerLaunch::Plain);
        pool.warm_spares = 0;
        let module = Module::start(
            ModuleConfig {
                store_path: dir.join("store.db"),
                durability: Durability::throwaway(),
                runtime: Config {
                    clock: basal_core::Clock::manual(1_000_000),
                    install_gate: basal_core::InstallGate::Off,
                    ..Config::default()
                },
                pool,
                engine: EngineConfig::default(),
                dry_run: DryRunConfig::new(dir.join("dry")),
            },
            Hosts {
                transport: Arc::new(crate::unconfigured::UnconfiguredTransport),
                host: host.clone(),
                catalog: Arc::new(MockCatalog::standard()),
                consent: Arc::new(MockConsent::new()),
                hooks: Arc::new(NoHooks),
            },
            Arc::new(NoWorkers),
        )
        .unwrap();
        module.agent_data.set_door(door.clone());
        Self { module, host, dir }
    }

    fn call(&self, method: &str, params: Value) -> Result<Value, ErrorBody> {
        self.module
            .agent_data
            .handle(&self.module.rt, method, &params)
    }

    /// Replays one of Entorhinal's sample requests with basal's agent ref,
    /// and a cut and holder of the test's choosing.
    fn sample(&self, side: &str, method: &str, cut: &str) -> Result<Value, ErrorBody> {
        let (method, params) = sample(side, method, cut);
        self.call(&method, params)
    }

    fn sql(&self, sql: &str) -> Vec<String> {
        self.module
            .rt
            .store()
            .read(|c| {
                Ok(c.prepare(sql)?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<_>>()?)
            })
            .unwrap()
    }

    fn exec(&self, sql: &str) {
        self.module
            .rt
            .store()
            .write(|tx| {
                tx.execute_batch(sql)?;
                Ok(())
            })
            .unwrap();
    }

    /// Installs and approves a scheduled flow owned by the agent.
    fn flow(&self, id: &str) -> [u8; 32] {
        let manifest = json!({"id":id,"version":1,"state_epoch":1,
            "explanation":"Exercises a moved flow's schedule for a test, without other effects.",
            "purpose":"Agent move test","trigger":{"schedule":{"interval":"1m"}}});
        let installed = self
            .module
            .rt
            .install(&InstallRequest {
                script: format!("return '{id}';"),
                author: AGENT.into(),
                loop_override: false,
                manifest: manifest.to_string(),
            })
            .unwrap();
        self.module
            .rt
            .approve(id, 1, &installed.code_hash, "core:card")
            .unwrap();
        installed.code_hash
    }

    /// Two flows of the agent, one with `kv`, a finished run and a queued
    /// event, the other disabled by the operator.
    fn seed(&self) -> ([u8; 32], [u8; 32]) {
        let alpha = self.flow("f-alpha");
        let beta = self.flow("f-beta");
        self.exec(
            "INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('f-alpha','count','4',1,4);
             UPDATE flows SET state='disabled',disabled_by='operator:operator',disabled_reason='paused',disabled_at=5 WHERE flow_id='f-beta';
             INSERT INTO event_receipts(flow_id,subject,event_key,notice,state,received_at) VALUES ('f-alpha','s','k','{}','backlog',5);
             INSERT INTO run_snapshots(flow_id,flow_version,code_hash,script,manifest) SELECT flow_id,version,code_hash,script,manifest FROM installs WHERE flow_id='f-alpha';
             INSERT INTO run_history(run_id,flow_id,flow_version,snapshot_id,trigger_id,trigger_kind,trigger,admitted_at,admit_seq,ended_at,state,result_size,call_counts,writes_sent,tokens_charged,cost_microusd,broken,activation_count,script_started,phase,deadline_exempt,history_lost,commit_seq)
               SELECT 'run-a-1','f-alpha',1,snapshot_id,'t1','schedule','{}',10,1,11,'succeeded',0,'{}',0,0,0,0,1,1,'done',0,0,1 FROM run_snapshots;",
        );
        (alpha, beta)
    }

    fn agent(&self) -> Caller {
        Caller::Agent {
            agent_id: AGENT.into(),
            scope_ref: "scope-1".into(),
        }
    }
}

/// The sample request for `method`, from `side` when that side sent one;
/// otherwise from any side, or (for `status`, which shares its shape) the
/// side's `prepare`.
fn sample(side: &str, method: &str, cut: &str) -> (String, Value) {
    let samples: Vec<Value> = serde_json::from_str(SAMPLES).unwrap();
    let find = |side: Option<&str>, method: &str| {
        samples
            .iter()
            .find(|r| side.is_none_or(|s| r["side"] == s) && r["request"]["method"] == method)
    };
    let record = find(Some(side), method)
        .or_else(|| find(None, method))
        .or_else(|| find(None, "agent_data.prepare"))
        .unwrap_or_else(|| panic!("no sample for {method}"));
    let mut params = record["request"]["params"].clone();
    if params.get("refs").is_some() {
        params["refs"] = json!([{"ref": AGENT, "ref_kind": REF_KIND}]);
    }
    if params.get("cutId").is_some() {
        params["cutId"] = json!(cut);
    }
    (method.to_owned(), params)
}

/// Seals a prepared cut on the door, as Entorhinal does after `export`,
/// beside another participant's item.
fn seal(door: &FakeDoor, cut: &str, prepared: &Value) {
    let mut items: Vec<Value> = prepared["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            json!({"sessionRef": item["sessionRef"], "stream": item["stream"],
            "endOffset": item["endOffset"], "digest": item["digest"]})
        })
        .collect();
    items.push(
        json!({"sessionRef": "9".repeat(32), "stream": "reserved:other",
        "endOffset": 3, "digest": hex(&Sha256::digest(b"abc"))}),
    );
    door.seal(cut, items);
}

const CUT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ABORTED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BACK: &str = "cccccccccccccccccccccccccccccccc";

/// Moves the agent from `a` to `b` through the sample requests, up to and
/// including activation, and returns `prepare`'s reply.
fn move_agent(door: &FakeDoor, a: &Computer, b: &Computer, cut: &str) -> Value {
    assert_eq!(
        a.sample("source", "agent_data.busy", cut).unwrap(),
        json!({"refs":[{"state":"idle"}]})
    );
    let prepared = a.sample("source", "agent_data.prepare", cut).unwrap();
    a.sample("source", "agent_data.export", cut).unwrap();
    seal(door, cut, &prepared);
    assert_eq!(
        b.sample("receiver", "agent_data.diverged", cut).unwrap(),
        json!({"refs":[{"state":"at_cut"}]})
    );
    b.sample("receiver", "agent_data.import", cut).unwrap();
    assert_eq!(
        b.sample("receiver", "agent_data.activate", cut).unwrap(),
        json!({})
    );
    prepared
}

fn code(result: Result<Value, ErrorBody>) -> RefusalCode {
    result.unwrap_err().code
}

#[test]
fn describe_names_one_agent_ref_and_one_agent_store() {
    let door = FakeDoor::new();
    let a = Computer::new("describe", &door);
    assert_eq!(
        a.sample("source", "agent_data.describe", CUT).unwrap(),
        json!({"ref_kinds":["agent"],"stores":[{"name":"basal-flows","sync_class":"agent"}],"external":false})
    );
}

#[test]
fn a_two_flow_agent_moves_with_kv_history_and_its_queued_events_counted() {
    let door = FakeDoor::new();
    let a = Computer::new("round-trip-a", &door);
    let b = Computer::new("round-trip-b", &door);
    let (alpha, beta) = a.seed();
    let prepared = a.sample("source", "agent_data.prepare", CUT).unwrap();
    let item = &prepared["items"][0];
    assert_eq!(item["ref_kind"], "agent");
    assert_eq!(item["ref"], AGENT);
    assert_eq!(item["stream"], STREAM);
    assert_eq!(item["format"], FORMAT);
    assert_eq!(item["frozen"], true);
    assert_eq!(item["sessionRef"], session_ref(AGENT, CUT));
    // The door holds exactly the bytes the item describes.
    let bytes = door.stream(&session_ref(AGENT, CUT));
    assert_eq!(item["endOffset"], bytes.len());
    assert_eq!(item["digest"], hex(&Sha256::digest(&bytes)));
    // While frozen, the agent's flows can't change.
    let refused = a
        .module
        .handle(&a.agent(), "flow.enable", json!({"flow_id":"f-beta"}))
        .unwrap_err();
    assert_eq!(refused.code, "agent_moving");
    assert_eq!(
        refused.detail,
        Some(json!({"agent_id": AGENT, "retryable": true}))
    );

    assert_eq!(
        a.sample("source", "agent_data.export", CUT).unwrap(),
        json!({"items":[{"sessionRef": item["sessionRef"], "endOffset": item["endOffset"]}]})
    );
    seal(&door, CUT, &prepared);
    assert_eq!(
        b.sample("receiver", "agent_data.diverged", CUT).unwrap(),
        json!({"refs":[{"state":"at_cut"}]})
    );
    let imported = b.sample("receiver", "agent_data.import", CUT).unwrap();
    assert_eq!(
        imported,
        json!({"items":[{"sessionRef": item["sessionRef"], "endOffset": item["endOffset"],
            "digest": item["digest"], "dropped": {"queued_event": 1}}]})
    );
    assert!(
        b.sql("SELECT flow_id FROM flows").is_empty(),
        "import writes no live row"
    );
    assert_eq!(
        b.sample("receiver", "agent_data.activate", CUT).unwrap(),
        json!({})
    );
    assert_eq!(
        b.sample("receiver", "agent_data.status", CUT).unwrap(),
        json!({"step":"activated"})
    );
    assert_eq!(
        b.sql("SELECT flow_id||':'||state||':'||disabled_by FROM flows ORDER BY flow_id"),
        ["f-alpha:disabled:move", "f-beta:disabled:move"]
    );
    assert_eq!(b.sql("SELECT key||'='||value FROM kv"), ["count=4"]);
    assert_eq!(b.sql("SELECT run_id FROM run_history"), ["run-a-1"]);

    // After activate: f-alpha's code is already active here, f-beta waits
    // for a card, and nothing was approved without asking prefrontal.
    let hex_of = |h: [u8; 32]| hex(&h);
    b.host.set_install_status(
        "f-alpha",
        1,
        Some(InstallStatus::Active {
            code_hash: hex_of(alpha),
            scope: None,
        }),
    );
    b.host.set_install_status(
        "f-beta",
        1,
        Some(InstallStatus::Pending {
            code_hash: hex_of(beta),
        }),
    );
    let report = b.module.rt.check_due_moved_flows().unwrap();
    assert_eq!(report.approved, ["f-alpha"]);
    assert_eq!(report.waiting, ["f-beta"]);
    assert_eq!(
        b.sql("SELECT flow_id||':'||state||':'||COALESCE(disabled_by,'-')||':'||COALESCE(approved_version,'-') FROM flows ORDER BY flow_id"),
        ["f-alpha:enabled:-:1", "f-beta:disabled:move:-"]
    );
    assert_eq!(
        b.sql("SELECT state FROM schedules WHERE flow_id='f-alpha'"),
        ["active"]
    );
    // A definite answer waits for the next approval event, not a timer.
    assert!(
        b.module
            .rt
            .check_due_moved_flows()
            .unwrap()
            .waiting
            .is_empty()
    );
    // The approval arrives later: the source's own disable comes back.
    b.host.set_install_status(
        "f-beta",
        1,
        Some(InstallStatus::Active {
            code_hash: hex_of(beta),
            scope: None,
        }),
    );
    b.module
        .rt
        .recheck_moved_flows(Some("f-beta"), None)
        .unwrap();
    assert_eq!(
        b.module.rt.check_due_moved_flows().unwrap().approved,
        ["f-beta"]
    );
    assert_eq!(
        b.sql(
            "SELECT state||':'||disabled_by||':'||disabled_reason FROM flows WHERE flow_id='f-beta'"
        ),
        ["disabled:operator:operator:paused"]
    );
    assert!(b.sql("SELECT flow_id FROM agent_move_pending").is_empty());

    // The source releases: a tombstone, and local changes are refused.
    assert_eq!(
        a.sample("source", "agent_data.release", CUT).unwrap(),
        json!({"enforced": true})
    );
    for (method, caller, params) in [
        (
            "flow.install",
            a.agent(),
            json!({"script":"return 2;","manifest":json!({"id":"f-alpha","version":2,"state_epoch":1,
            "explanation":"Exercises a moved flow's schedule for a test, without other effects.",
            "changes":"A second version installed after the agent moved.",
            "purpose":"Agent move test","trigger":{"schedule":{"interval":"1m"}}}).to_string()}),
        ),
        ("flow.enable", a.agent(), json!({"flow_id":"f-alpha"})),
        (
            "flow.instance.ensure",
            Caller::Core,
            json!({"package":"p","version":1,"agent_id":AGENT,"generation":1}),
        ),
        (
            "flow.instantiate",
            a.agent(),
            json!({"template":"t","version":1,"params":{},"expires_in":"1h"}),
        ),
    ] {
        let refused = a.module.handle(&caller, method, params).unwrap_err();
        assert_eq!(refused.code, "agent_moved", "{method}");
        assert_eq!(
            refused.detail,
            Some(json!({"agent_id": AGENT, "holder": "02020202020202020202020202020202"}))
        );
    }
    assert_eq!(
        a.sql("SELECT flow_id||':'||removed_by FROM flows ORDER BY flow_id"),
        [
            "f-alpha:moved:02020202020202020202020202020202",
            "f-beta:moved:02020202020202020202020202020202"
        ]
    );
    assert_eq!(a.sql("SELECT state FROM event_receipts"), ["refused"]);
}

#[test]
fn an_unreachable_prefrontal_is_retried_with_a_growing_wait() {
    let door = FakeDoor::new();
    let a = Computer::new("unreachable-a", &door);
    let b = Computer::new("unreachable-b", &door);
    a.seed();
    move_agent(&door, &a, &b, CUT);
    // The mock's prefrontal answers nothing for these versions.
    let report = b.module.rt.check_due_moved_flows().unwrap();
    assert_eq!(report.unreachable, ["f-alpha", "f-beta"]);
    assert_eq!(b.sql("SELECT CAST(retries AS TEXT)||':'||CAST(retry_at-checked_at AS TEXT) FROM agent_move_pending ORDER BY flow_id"), ["1:2000", "1:2000"]);
    // Not due yet: nothing is asked again.
    assert!(
        b.module
            .rt
            .check_due_moved_flows()
            .unwrap()
            .unreachable
            .is_empty()
    );
}

#[test]
fn a_retried_prepare_returns_the_same_item_and_resumes_a_lost_upload() {
    let door = FakeDoor::new();
    let a = Computer::new("retry", &door);
    a.seed();
    door.fail_next("session.cover");
    assert_eq!(
        code(a.sample("source", "agent_data.prepare", CUT)),
        RefusalCode::Unavailable
    );
    // The freeze and the staged bytes survived the refusal; a retry
    // resumes the upload instead of meeting its own step.
    assert_eq!(
        a.sample("source", "agent_data.status", CUT).unwrap(),
        json!({"step":"prepared"})
    );
    let first = a.sample("source", "agent_data.prepare", CUT).unwrap();
    let again = a.sample("source", "agent_data.prepare", CUT).unwrap();
    assert_eq!(first, again);
    let bytes = door.stream(&session_ref(AGENT, CUT));
    assert_eq!(
        first["items"][0]["endOffset"],
        bytes.len(),
        "no byte admitted twice"
    );
    // Another request under the same cut and op is refused.
    let (method, mut params) = sample("source", "agent_data.prepare", CUT);
    params["refs"] = json!([{"ref": "agent_other", "ref_kind": "agent"}]);
    assert_eq!(code(a.call(&method, params)), RefusalCode::IdReused);
}

#[test]
fn a_live_run_makes_the_agent_busy_and_prepare_refuses() {
    let door = FakeDoor::new();
    let a = Computer::new("busy", &door);
    a.seed();
    a.exec(
        "INSERT INTO live_runs(run_id,flow_id,flow_version,snapshot_id,trigger_id,trigger_kind,trigger,attempt,admitted_at,admit_seq,admit_epoch,admit_segment,admit_offset,deadline_ms,self_input,params_input,grant_guards,end_instance)
           SELECT 'run-live','f-alpha',1,snapshot_id,'t2','schedule','{}',1,20,2,0,0,0,600000,'null','null','null',0 FROM run_snapshots",
    );
    let busy = a.sample("source", "agent_data.busy", CUT).unwrap();
    assert_eq!(busy["refs"][0]["state"], "busy");
    assert_eq!(busy["refs"][0]["reason_code"], "run_active");
    let refused = a.sample("source", "agent_data.prepare", CUT).unwrap_err();
    assert_eq!(refused.code, RefusalCode::Busy);
    assert_eq!(
        refused.detail.unwrap()["refs"][0]["reason_code"],
        "run_active"
    );
    assert!(
        a.sql("SELECT agent_id FROM agent_move_holds").is_empty(),
        "nothing frozen"
    );
    assert_eq!(
        a.sample("source", "agent_data.status", CUT).unwrap(),
        json!({"step":"none"})
    );
}

#[test]
fn activate_reports_run_active_and_retries_the_same_imported_cut() {
    let door = FakeDoor::new();
    let first = Computer::new("activate-busy-first", &door);
    let receiver = Computer::new("activate-busy-receiver", &door);
    let incoming = Computer::new("activate-busy-incoming", &door);
    first.seed();
    move_agent(&door, &first, &receiver, CUT);

    incoming.flow("f-alpha");
    incoming.exec(
        "INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('f-alpha','count','5',1,5)",
    );
    let prepared = incoming
        .sample("source", "agent_data.prepare", BACK)
        .unwrap();
    incoming
        .sample("source", "agent_data.export", BACK)
        .unwrap();
    seal(&door, BACK, &prepared);
    assert_eq!(
        receiver
            .sample("receiver", "agent_data.diverged", BACK)
            .unwrap(),
        json!({"refs":[{"state":"at_cut"}]})
    );
    receiver
        .sample("receiver", "agent_data.import", BACK)
        .unwrap();

    receiver.exec(
        "INSERT INTO live_runs(run_id,flow_id,flow_version,snapshot_id,trigger_id,trigger_kind,trigger,attempt,admitted_at,admit_seq,admit_epoch,admit_segment,admit_offset,deadline_ms,self_input,params_input,grant_guards,end_instance)
           SELECT 'run-live','f-alpha',1,snapshot_id,'t2','schedule','{}',1,20,2,0,0,0,600000,'null','null','null',0 FROM run_snapshots",
    );
    let refused = receiver
        .sample("receiver", "agent_data.activate", BACK)
        .unwrap_err();
    assert_eq!(refused.code, RefusalCode::Busy);
    assert_eq!(
        refused.detail,
        Some(json!({"refs":[{"ref_kind": REF_KIND, "ref": AGENT,
            "reason_code":"run_active", "reason":"flow f-alpha has a run in progress"}]}))
    );
    assert_eq!(
        receiver
            .sample("receiver", "agent_data.status", BACK)
            .unwrap(),
        json!({"step":"imported"})
    );
    assert_eq!(receiver.sql("SELECT value FROM kv"), ["4"]);

    receiver.exec("DELETE FROM live_runs WHERE run_id='run-live'");
    assert_eq!(
        receiver
            .sample("receiver", "agent_data.activate", BACK)
            .unwrap(),
        json!({})
    );
    assert_eq!(receiver.sql("SELECT value FROM kv"), ["5"]);
}

#[test]
fn abort_lifts_the_freeze_and_refuses_a_late_prepare() {
    let door = FakeDoor::new();
    let a = Computer::new("abort", &door);
    a.seed();
    a.sample("source-abort", "agent_data.prepare", ABORTED)
        .unwrap();
    a.sample("source-abort", "agent_data.export", ABORTED)
        .unwrap();
    assert_eq!(a.sql("SELECT kind FROM agent_move_holds"), ["frozen"]);
    assert_eq!(
        a.sample("source-abort", "agent_data.abort", ABORTED)
            .unwrap(),
        json!({})
    );
    assert!(a.sql("SELECT kind FROM agent_move_holds").is_empty());
    assert!(
        !a.dir.join("agent-move").join(ABORTED).exists(),
        "staging is gone"
    );
    // The agent's flows are its own again.
    a.module
        .handle(
            &Caller::Operator,
            "flow.enable",
            json!({"flow_id":"f-beta"}),
        )
        .unwrap();
    // Aborting a cut never seen succeeds and is remembered, so a delayed
    // prepare for it is refused rather than freezing the agent again.
    assert_eq!(
        a.sample("source-abort", "agent_data.abort", BACK).unwrap(),
        json!({})
    );
    assert_eq!(
        a.sample("source-abort", "agent_data.status", BACK).unwrap(),
        json!({"step":"aborted"})
    );
    let late = a
        .sample("source-abort", "agent_data.prepare", BACK)
        .unwrap_err();
    assert_eq!(late.code, RefusalCode::InvalidState);
    assert_eq!(late.detail, Some(json!({"step":"aborted"})));
    assert!(a.sql("SELECT kind FROM agent_move_holds").is_empty());
}

#[test]
fn a_move_back_replaces_the_tombstone() {
    let door = FakeDoor::new();
    let a = Computer::new("back-a", &door);
    let b = Computer::new("back-b", &door);
    a.seed();
    move_agent(&door, &a, &b, CUT);
    a.sample("source", "agent_data.release", CUT).unwrap();
    // The agent writes on B, then moves back to A.
    b.exec("UPDATE kv SET value='5',revision=5");
    let back = b.sample("source", "agent_data.prepare", BACK).unwrap();
    b.sample("source", "agent_data.export", BACK).unwrap();
    seal(&door, BACK, &back);
    assert_eq!(
        a.sample("receiver", "agent_data.diverged", BACK).unwrap(),
        json!({"refs":[{"state":"at_cut"}]})
    );
    a.sample("receiver", "agent_data.import", BACK).unwrap();
    a.sample("receiver", "agent_data.activate", BACK).unwrap();
    b.sample("source", "agent_data.release", BACK).unwrap();
    assert!(a.sql("SELECT kind FROM agent_move_holds").is_empty());
    assert_eq!(a.sql("SELECT value FROM kv"), ["5"]);
    assert_eq!(
        a.sql("SELECT flow_id||':'||COALESCE(removed_by,'-')||':'||disabled_by FROM flows ORDER BY flow_id"),
        ["f-alpha:-:move", "f-beta:-:move"]
    );
    assert_eq!(b.sql("SELECT kind FROM agent_move_holds"), ["moved"]);
}

#[test]
fn a_live_local_copy_with_newer_data_is_a_fork() {
    let door = FakeDoor::new();
    let a = Computer::new("fork-a", &door);
    let b = Computer::new("fork-b", &door);
    a.seed();
    move_agent(&door, &a, &b, CUT);
    a.sample("source", "agent_data.release", CUT).unwrap();
    // B's copy gains data no cut holds; a second cut arriving at B (here
    // an empty one from the tombstoned A) would overwrite it.
    b.exec("INSERT INTO kv(flow_id,key,value,bytes,revision) VALUES ('f-alpha','new','1',1,1)");
    let other = a.sample("source", "agent_data.prepare", BACK).unwrap();
    a.sample("source", "agent_data.export", BACK).unwrap();
    seal(&door, BACK, &other);
    assert_eq!(
        b.sample("receiver", "agent_data.diverged", BACK).unwrap(),
        json!({"refs":[{"state":"advanced"}]})
    );
    assert_eq!(
        code(b.sample("receiver", "agent_data.import", BACK)),
        RefusalCode::LocalAdvanced
    );
    assert_eq!(b.sql("SELECT key FROM kv ORDER BY key"), ["count", "new"]);
}

#[test]
fn a_flow_id_held_by_another_owner_is_refused_naming_both() {
    let door = FakeDoor::new();
    let a = Computer::new("clash-a", &door);
    let b = Computer::new("clash-b", &door);
    a.seed();
    b.exec("INSERT INTO flows(flow_id,owner,created_at) VALUES ('f-beta','agent_other',1)");
    let prepared = a.sample("source", "agent_data.prepare", CUT).unwrap();
    a.sample("source", "agent_data.export", CUT).unwrap();
    seal(&door, CUT, &prepared);
    let refused = b.sample("receiver", "agent_data.import", CUT).unwrap_err();
    assert_eq!(refused.code, RefusalCode::LocalAdvanced);
    assert!(
        refused.message.contains("f-beta") && refused.message.contains("agent_other"),
        "{}",
        refused.message
    );
    assert_eq!(
        b.sample("receiver", "agent_data.status", CUT).unwrap(),
        json!({"step":"none"})
    );
}

#[test]
fn damaged_or_unreadable_cuts_are_refused_before_anything_is_written() {
    let door = FakeDoor::new();
    let a = Computer::new("damaged-a", &door);
    let b = Computer::new("damaged-b", &door);
    a.seed();
    let prepared = a.sample("source", "agent_data.prepare", CUT).unwrap();
    a.sample("source", "agent_data.export", CUT).unwrap();
    // Not sealed yet: retry within the step.
    assert_eq!(
        code(b.sample("receiver", "agent_data.import", CUT)),
        RefusalCode::CutNotReady
    );
    // Sealed under another digest than the bytes hash to.
    let mut wrong = prepared.clone();
    wrong["items"][0]["digest"] = json!("0".repeat(64));
    seal(&door, CUT, &wrong);
    let refused = b.sample("receiver", "agent_data.import", CUT).unwrap_err();
    assert_eq!(refused.code, RefusalCode::DigestMismatch);
    // A format this build doesn't read.
    let session = session_ref(AGENT, CUT);
    let bytes = door.stream(&session);
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    let mut header: Value = serde_json::from_slice(&bytes[4..4 + length]).unwrap();
    header["format"] = json!("basal-flows/2");
    let header = serde_json::to_vec(&header).unwrap();
    let mut newer = (header.len() as u32).to_be_bytes().to_vec();
    newer.extend(header);
    newer.extend(&bytes[4 + length..]);
    door.replace(&session, newer.clone());
    let mut sealed = prepared.clone();
    sealed["items"][0]["endOffset"] = json!(newer.len());
    sealed["items"][0]["digest"] = json!(hex(&Sha256::digest(&newer)));
    seal(&door, CUT, &sealed);
    let refused = b.sample("receiver", "agent_data.import", CUT).unwrap_err();
    assert_eq!(refused.code, RefusalCode::IncompatibleVersion);
    assert_eq!(
        refused.detail,
        Some(json!({"found":"basal-flows/2","required":"basal-flows/1"}))
    );
    assert!(b.sql("SELECT flow_id FROM flows").is_empty());
    assert_eq!(
        b.sample("receiver", "agent_data.status", CUT).unwrap(),
        json!({"step":"none"})
    );
}

#[test]
fn ops_out_of_order_and_repeats_follow_the_contract() {
    let door = FakeDoor::new();
    let a = Computer::new("order", &door);
    a.seed();
    assert_eq!(
        code(a.sample("source", "agent_data.export", CUT)),
        RefusalCode::CutUnknown
    );
    assert_eq!(
        code(a.sample("source", "agent_data.release", CUT)),
        RefusalCode::CutUnknown
    );
    a.sample("source", "agent_data.prepare", CUT).unwrap();
    let early = a.sample("source", "agent_data.release", CUT).unwrap_err();
    assert_eq!(early.code, RefusalCode::InvalidState);
    a.sample("source", "agent_data.export", CUT).unwrap();
    let released = a.sample("source", "agent_data.release", CUT).unwrap();
    assert_eq!(
        a.sample("source", "agent_data.release", CUT).unwrap(),
        released
    );
    let (method, mut params) = sample("source", "agent_data.release", CUT);
    params["holder"] = json!("another-holder");
    assert_eq!(code(a.call(&method, params)), RefusalCode::IdReused);
    let late = a.sample("source", "agent_data.abort", CUT).unwrap_err();
    assert_eq!(late.code, RefusalCode::InvalidState);
    // A ref kind basal doesn't hold.
    let (method, mut params) = sample("source", "agent_data.busy", CUT);
    params["refs"] = json!([{"ref":"x","ref_kind":"opencode-session"}]);
    let refused = a.call(&method, params).unwrap_err();
    assert_eq!(refused.code, RefusalCode::UnknownRef);
    assert_eq!(
        refused.detail,
        Some(json!({"ref_kind":"opencode-session","ref":"x"}))
    );
}

#[test]
fn every_contract_request_vector_gets_a_reply_or_a_refusal_from_its_row() {
    let door = FakeDoor::new();
    let a = Computer::new("vectors", &door);
    let requests: Value =
        serde_json::from_str(cortexkit_role_agent_data::vectors::file("requests.json").unwrap())
            .unwrap();
    for vector in requests["requests"].as_array().unwrap() {
        let method = vector["method"].as_str().unwrap();
        let op = Op::from_method(method).unwrap();
        if let Err(refusal) = a.call(method, vector["params"].clone()) {
            assert!(
                allowed_refusals(op).contains(&refusal.code),
                "{}: {:?} is outside {method}'s row",
                vector["name"],
                refusal.code
            );
        }
    }
    assert_eq!(
        code(a.call("agent_data.describe", json!({"extra": 1}))),
        RefusalCode::InvalidRequest
    );
    assert_eq!(
        code(a.call("agent_data.resume", json!({}))),
        RefusalCode::InvalidRequest
    );
}

/// Every change to a flow that a move carries, as the op and caller that
/// make it.
fn changes(a: &Computer) -> Vec<(&'static str, Caller, Value)> {
    vec![
        ("flow.disable", a.agent(), json!({"flow_id":"f-alpha"})),
        (
            "flow.uninstall",
            Caller::Operator,
            json!({"flow_id":"f-alpha"}),
        ),
        (
            "flow.instance.end",
            Caller::Operator,
            json!({"flow_id":"f-alpha"}),
        ),
        (
            "flow.instance.remove",
            Caller::Core,
            json!({"package":"p","agent_id":AGENT,"generation":1}),
        ),
    ]
}

fn hints(alpha: [u8; 32]) -> Vec<(&'static str, Value)> {
    vec![
        (
            "flow.install_decided",
            json!({"flow_id":"f-alpha","version":1,"attempt":1}),
        ),
        (
            "flow.install_revoked",
            json!({"flow_id":"f-alpha","version":1,"code_hash":hex(&alpha)}),
        ),
    ]
}

#[test]
fn a_frozen_agents_flows_refuse_every_change_and_a_moved_agents_ignore_core_hints() {
    let door = FakeDoor::new();
    let a = Computer::new("frozen-writes", &door);
    let (alpha, _) = a.seed();
    a.sample("source", "agent_data.prepare", CUT).unwrap();
    for (method, caller, params) in changes(&a) {
        let refused = a.module.handle(&caller, method, params).unwrap_err();
        assert_eq!(refused.code, "agent_moving", "{method}");
        assert_eq!(
            refused.detail,
            Some(json!({"agent_id": AGENT, "retryable": true})),
            "{method}"
        );
    }
    for (method, params) in hints(alpha) {
        let refused = a.module.handle(&Caller::Core, method, params).unwrap_err();
        assert_eq!(refused.code, "agent_moving", "{method}");
    }
    // Nothing changed the frozen rows: the cut still describes them.
    assert_eq!(
        a.sql("SELECT flow_id||':'||state||':'||removed FROM flows ORDER BY flow_id"),
        ["f-alpha:enabled:0", "f-beta:disabled:0"]
    );
    a.sample("source", "agent_data.export", CUT).unwrap();
    a.sample("source", "agent_data.release", CUT).unwrap();
    for (method, caller, params) in changes(&a) {
        let refused = a.module.handle(&caller, method, params).unwrap_err();
        assert_eq!(refused.code, "agent_moved", "{method}");
    }
    let before = a.sql("SELECT flow_id||':'||COALESCE(approved_version,'-')||':'||removed_by FROM flows ORDER BY flow_id");
    for (method, params) in hints(alpha) {
        assert_eq!(
            a.module.handle(&Caller::Core, method, params).unwrap(),
            json!({}),
            "{method}"
        );
    }
    assert_eq!(
        a.sql("SELECT flow_id||':'||COALESCE(approved_version,'-')||':'||removed_by FROM flows ORDER BY flow_id"),
        before,
        "a hint for a flow that lives elsewhere changes nothing"
    );
}

#[test]
fn an_unapproved_flow_belongs_to_its_author_for_the_freeze() {
    let door = FakeDoor::new();
    let a = Computer::new("draft-owner", &door);
    a.seed();
    // Installed by the agent but never approved: no owner yet.
    let manifest = json!({"id":"f-draft","version":1,"state_epoch":1,
        "explanation":"Exercises a moved flow's schedule for a test, without other effects.",
        "purpose":"Agent move test","trigger":{"schedule":{"interval":"1m"}}});
    a.module
        .rt
        .install(&InstallRequest {
            script: "return 'draft';".into(),
            author: AGENT.into(),
            loop_override: false,
            manifest: manifest.to_string(),
        })
        .unwrap();
    assert_eq!(
        a.sql("SELECT COALESCE(owner,'-') FROM flows WHERE flow_id='f-draft'"),
        ["-"]
    );
    // The draft's card is still open, which a move can't carry: settle it.
    a.exec(
        "UPDATE install_cards SET state='declined',decided_by='core:card' WHERE flow_id='f-draft'",
    );
    a.sample("source", "agent_data.prepare", CUT).unwrap();
    let refused = a
        .module
        .handle(
            &Caller::Operator,
            "flow.disable",
            json!({"flow_id":"f-draft"}),
        )
        .unwrap_err();
    assert_eq!(refused.code, "agent_moving");
}

#[test]
fn a_freeze_committed_without_its_checkpoint_resumes_or_aborts_cleanly() {
    let door = FakeDoor::new();
    let a = Computer::new("half-prepared", &door);
    a.seed();
    // A crash after the freeze committed and before the checkpoint did.
    a.module
        .rt
        .store()
        .write(|tx| {
            basal_core::agent_move::freeze(tx, AGENT, CUT, 1)?
                .map_err(|b| basal_core::CoreError::Invalid(b.reason))
        })
        .unwrap();
    assert_eq!(
        a.sample("source", "agent_data.status", CUT).unwrap(),
        json!({"step":"none"})
    );
    let prepared = a.sample("source", "agent_data.prepare", CUT).unwrap();
    assert_eq!(prepared["items"][0]["sessionRef"], session_ref(AGENT, CUT));
    assert_eq!(
        a.sample("source", "agent_data.status", CUT).unwrap(),
        json!({"step":"prepared"})
    );

    // The same crash, then the driver gives up: abort lifts the freeze.
    a.module
        .rt
        .store()
        .write(|tx| {
            basal_core::agent_move::freeze(tx, "agent_other", ABORTED, 1)?
                .map_err(|b| basal_core::CoreError::Invalid(b.reason))
        })
        .unwrap();
    let (method, mut params) = sample("source-abort", "agent_data.abort", ABORTED);
    params["refs"] = json!([{"ref":"agent_other","ref_kind":"agent"}]);
    a.call(&method, params).unwrap();
    assert_eq!(a.sql("SELECT agent_id FROM agent_move_holds"), [AGENT]);
}

#[test]
fn startup_sweeps_staging_only_for_finished_or_unknown_cuts() {
    let door = FakeDoor::new();
    let a = Computer::new("sweep", &door);
    a.seed();
    a.sample("source", "agent_data.prepare", CUT).unwrap();
    let staging = a.dir.join("agent-move");
    let finished = "dddddddddddddddddddddddddddddddd";
    let unknown = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let freezing = "ffffffffffffffffffffffffffffffff";
    for cut in [finished, unknown, freezing] {
        std::fs::create_dir_all(staging.join(cut)).unwrap();
    }
    std::fs::create_dir_all(staging.join("not-a-cut")).unwrap();
    a.sample("source-abort", "agent_data.abort", finished)
        .unwrap();
    std::fs::create_dir_all(staging.join(finished)).unwrap();
    a.exec(&format!(
        "INSERT INTO agent_move_holds(agent_id,kind,cut_id,at) VALUES ('agent_other','frozen','{freezing}',1)"
    ));
    let mut swept = a.module.agent_data.sweep(&a.module.rt);
    swept.sort();
    assert_eq!(swept, [finished, unknown]);
    assert!(
        staging.join(CUT).exists(),
        "a prepared cut keeps its staging"
    );
    assert!(
        staging.join(freezing).exists(),
        "a freeze without a checkpoint is in progress"
    );
    assert!(staging.join("not-a-cut").exists());
}

#[test]
fn approval_events_open_no_write_on_a_computer_without_moved_flows() {
    let door = FakeDoor::new();
    let a = Computer::new("quiet-recheck", &door);
    let before = a.module.rt.store().write_transactions_for_test();
    a.module
        .rt
        .recheck_moved_flows(None, Some("remind-at"))
        .unwrap();
    a.module
        .rt
        .recheck_moved_flows(Some("f-alpha"), None)
        .unwrap();
    assert_eq!(a.module.rt.store().write_transactions_for_test(), before);
}

#[test]
fn release_records_every_backlogged_event_it_discards() {
    let door = FakeDoor::new();
    let a = Computer::new("discarded", &door);
    a.seed();
    a.sample("source", "agent_data.prepare", CUT).unwrap();
    // An event queued during the freeze, after the cut counted one.
    a.exec("INSERT INTO event_receipts(flow_id,subject,event_key,notice,state,received_at) VALUES ('f-alpha','s','k2','{}','backlog',6)");
    a.sample("source", "agent_data.export", CUT).unwrap();
    a.sample("source", "agent_data.release", CUT).unwrap();
    assert_eq!(
        a.sql(&format!(
            "SELECT dropped FROM agent_move_cuts WHERE cut_id='{CUT}'"
        )),
        [r#"{"discarded_at_release":2,"queued_event":1}"#]
    );
}

/// Installs a flow as the agent, from the agent's own route.
fn install_as_agent(a: &Computer, id: &str) -> Result<Value, crate::ops::OpError> {
    let manifest = json!({"id":id,"version":1,"state_epoch":1,
        "explanation":"Exercises a moved flow's schedule for a test, without other effects.",
        "purpose":"Agent move test","trigger":{"schedule":{"interval":"1m"}}});
    a.module.handle(
        &a.agent(),
        "flow.install",
        json!({"script": format!("return '{id}';"), "manifest": manifest.to_string()}),
    )
}

#[test]
fn an_agent_with_nothing_here_ships_no_item_and_still_freezes_and_tombstones() {
    let door = FakeDoor::new();
    let a = Computer::new("nothing-a", &door);
    let b = Computer::new("nothing-b", &door);
    let prepared = a.sample("source", "agent_data.prepare", CUT).unwrap();
    assert_eq!(prepared, json!({"items": [], "external": []}));
    assert_eq!(
        a.sample("source", "agent_data.prepare", CUT).unwrap(),
        prepared,
        "a repeat replays the empty reply"
    );
    assert!(door.stream(&session_ref(AGENT, CUT)).is_empty());
    assert!(
        !door.calls().iter().any(|c| c.starts_with("session.")),
        "no stream is admitted or covered: {:?}",
        door.calls()
    );
    // The freeze holds all the same.
    assert_eq!(code_of(install_as_agent(&a, "f-new")), "agent_moving");
    assert_eq!(
        a.sample("source", "agent_data.export", CUT).unwrap(),
        json!({"items": []})
    );
    seal(&door, CUT, &prepared);
    assert_eq!(
        b.sample("receiver", "agent_data.diverged", CUT).unwrap(),
        json!({"refs":[{"state":"at_cut"}]})
    );
    assert_eq!(
        b.sample("receiver", "agent_data.import", CUT).unwrap(),
        json!({"items": []})
    );
    assert_eq!(
        b.sample("receiver", "agent_data.activate", CUT).unwrap(),
        json!({})
    );
    assert_eq!(
        b.sample("receiver", "agent_data.status", CUT).unwrap(),
        json!({"step":"activated"})
    );
    assert!(b.sql("SELECT flow_id FROM flows").is_empty());
    assert!(b.sql("SELECT agent_id FROM agent_move_holds").is_empty());
    assert!(!b.dir.join("agent-move").join(CUT).exists());
    // The tombstone still refuses a later local install on the source.
    assert_eq!(
        a.sample("source", "agent_data.release", CUT).unwrap(),
        json!({"enforced": true})
    );
    assert_eq!(code_of(install_as_agent(&a, "f-new")), "agent_moved");
    // ...and not where the agent now lives.
    assert_not_moved(install_as_agent(&b, "f-new"));
}

#[test]
fn an_agent_with_one_flow_still_ships_its_item() {
    let door = FakeDoor::new();
    let a = Computer::new("one-flow", &door);
    a.flow("f-alpha");
    let prepared = a.sample("source", "agent_data.prepare", CUT).unwrap();
    let items = prepared["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["stream"], STREAM);
    assert!(items[0]["endOffset"].as_u64().unwrap() > 0);
    let exported = a.sample("source", "agent_data.export", CUT).unwrap();
    assert_eq!(exported["items"].as_array().unwrap().len(), 1);
}

#[test]
fn a_cut_without_a_basal_item_is_a_fork_against_live_local_flows() {
    let door = FakeDoor::new();
    let a = Computer::new("empty-fork-a", &door);
    let b = Computer::new("empty-fork-b", &door);
    a.seed();
    move_agent(&door, &a, &b, CUT);
    a.sample("source", "agent_data.release", CUT).unwrap();
    // B's live flows match the cut it activated, but the next cut (from the
    // tombstoned A, which holds nothing live) carries no basal item.
    let other = a.sample("source", "agent_data.prepare", BACK).unwrap();
    assert_eq!(other["items"], json!([]));
    a.sample("source", "agent_data.export", BACK).unwrap();
    seal(&door, BACK, &other);
    assert_eq!(
        b.sample("receiver", "agent_data.diverged", BACK).unwrap(),
        json!({"refs":[{"state":"advanced"}]})
    );
    assert_eq!(
        code(b.sample("receiver", "agent_data.import", BACK)),
        RefusalCode::LocalAdvanced
    );
    assert_eq!(
        b.sample("receiver", "agent_data.status", BACK).unwrap(),
        json!({"step":"none"})
    );
    // Not readable yet: retry within the step rather than guessing.
    let unsealed = "dddddddddddddddddddddddddddddddd";
    assert_eq!(
        code(b.sample("receiver", "agent_data.diverged", unsealed)),
        RefusalCode::CutNotReady
    );
}

#[test]
fn an_empty_cut_moving_back_replaces_the_tombstone() {
    let door = FakeDoor::new();
    let a = Computer::new("empty-back-a", &door);
    let c = Computer::new("empty-back-c", &door);
    // The agent left A earlier and has no flows anywhere now.
    a.exec(&format!(
        "INSERT INTO agent_move_holds(agent_id,kind,cut_id,holder,at) VALUES ('{AGENT}','moved','{ABORTED}','holder-c',1)"
    ));
    assert_eq!(code_of(install_as_agent(&a, "f-new")), "agent_moved");
    let prepared = c.sample("source", "agent_data.prepare", BACK).unwrap();
    c.sample("source", "agent_data.export", BACK).unwrap();
    seal(&door, BACK, &prepared);
    assert_eq!(
        a.sample("receiver", "agent_data.diverged", BACK).unwrap(),
        json!({"refs":[{"state":"at_cut"}]})
    );
    a.sample("receiver", "agent_data.import", BACK).unwrap();
    assert_eq!(
        a.sql("SELECT kind FROM agent_move_holds"),
        ["moved"],
        "import writes nothing"
    );
    a.sample("receiver", "agent_data.activate", BACK).unwrap();
    assert!(a.sql("SELECT kind FROM agent_move_holds").is_empty());
    assert_not_moved(install_as_agent(&a, "f-new"));
}

/// The install may still fail here for reasons of its own (this harness has
/// no workers for its dry run), but never because the agent moved.
fn assert_not_moved(result: Result<Value, crate::ops::OpError>) {
    if let Err(error) = result {
        assert!(!error.code.starts_with("agent_mov"), "{error:?}");
    }
}

fn code_of(result: Result<Value, crate::ops::OpError>) -> String {
    result.unwrap_err().code
}
