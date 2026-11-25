use std::path::PathBuf;

use basal_rig::stores::{BasalStore, BrocaStore, CoreStore};
use rusqlite::Connection;
use serde_json::json;

// These fixtures build partial schemas, not basal-core stores. They are
// disposable and need no power-loss safety, just like Durability::throwaway.
fn throwaway_connection(path: impl AsRef<std::path::Path>) -> Connection {
    let connection = Connection::open(path).unwrap();
    connection
        .pragma_update(None, "synchronous", "OFF")
        .unwrap();
    connection
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = loop {
            let dir = std::env::temp_dir().join(format!(
                "basal-rig-store-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create isolated store directory: {error}"),
            }
        };
        Self(dir.join("store.db"))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

#[test]
fn broca_run_count_observes_real_insertions_and_store_errors() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    let store = BrocaStore {
        path: scratch.0.clone(),
    };
    assert!(store.run_count().is_err());
    c.execute("CREATE TABLE run_index(run_id TEXT)", [])
        .unwrap();
    assert_eq!(store.run_count().unwrap(), 0);
    c.execute("INSERT INTO run_index VALUES ('previous')", [])
        .unwrap();
    assert_eq!(store.run_count().unwrap(), 1);
    c.execute("INSERT INTO run_index VALUES ('new')", [])
        .unwrap();
    assert_eq!(store.run_count().unwrap(), 2);
}

#[test]
fn codemode_evidence_reads_actual_keyed_run_and_owner_scope() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE codemode_runs(run_id TEXT,agent_id TEXT,status TEXT);
        CREATE TABLE codemode_tool_calls(run_id TEXT,call_key TEXT);
        CREATE TABLE codemode_run_scope(run_id TEXT,agent_id TEXT,scope_ref TEXT,scope_epoch INTEGER,catalog_json TEXT,lifecycle TEXT,end_settled INTEGER);
        INSERT INTO codemode_runs VALUES ('actual-run','agent-1','completed'),('other','agent-2','running');
        INSERT INTO codemode_tool_calls VALUES ('actual-run','key-1'),('other','key-2');
        INSERT INTO codemode_run_scope VALUES ('actual-run','agent-1','opaque-scope',9,'[{\"tool\":\"probe_echo\",\"module\":\"ckdev-runscope-probe\",\"op\":\"probe_echo\"}]','terminal',1);").unwrap();
    let basal = BasalStore {
        path: scratch.0.clone(),
    };
    let core = CoreStore {
        path: scratch.0.clone(),
    };
    assert_eq!(basal.codemode_call("missing").unwrap(), None);
    assert_eq!(
        basal.codemode_call("key-1").unwrap().unwrap(),
        json!({"run_id":"actual-run","agent_id":"agent-1","status":"completed"})
    );
    let scope = core.codemode_scope("actual-run").unwrap().unwrap();
    assert_eq!(scope["agent_id"], "agent-1");
    assert_eq!(
        scope["selector"],
        json!({"owner":{"kind":"reserved","module_id":"prefrontal"},"ref":"opaque-scope","scope_epoch":9})
    );
    assert_eq!(scope["catalog"][0]["tool"], "probe_echo");
    let selector: subc_protocol::scope::ScopeSelector =
        serde_json::from_value(scope["selector"].clone()).unwrap();
    assert_eq!(selector.scope_epoch, Some(9));
    assert_eq!(scope["lifecycle"], "terminal");
    assert_eq!(scope["end_settled"], true);
    assert_eq!(core.codemode_scope("other").unwrap(), None);
    c.execute(
        "UPDATE codemode_run_scope SET lifecycle='running',end_settled=0",
        [],
    )
    .unwrap();
    let changed = core.codemode_scope("actual-run").unwrap().unwrap();
    assert_eq!(changed["lifecycle"], "running");
    assert_eq!(changed["end_settled"], false);
}

#[test]
fn journal_evidence_rejects_negative_positions() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch(
        "CREATE TABLE runs(run_id TEXT, flow_id TEXT);
        CREATE TABLE journal(run_id TEXT, position INTEGER, kind_code INTEGER, dispatch TEXT,
            attempts INTEGER, settlement TEXT, value TEXT, request TEXT);
        INSERT INTO runs VALUES ('run', 'flow');
        INSERT INTO journal VALUES ('run', -1, 7, 'sent', 1, 'fulfilled', '{}', '{}'); CREATE VIEW journal_with_payloads AS SELECT *,0 AS payload_pruned FROM journal;",
    )
    .unwrap();
    let store = BasalStore {
        path: scratch.0.clone(),
    };
    assert!(
        store
            .calls("flow")
            .unwrap_err()
            .contains("negative journal position")
    );
    c.execute("UPDATE journal SET position=0", []).unwrap();
    c.execute_batch("DROP VIEW journal_with_payloads; CREATE VIEW journal_with_payloads AS SELECT *,1 AS payload_pruned FROM journal;").unwrap();
    let calls = store.calls("flow").unwrap();
    assert_eq!(calls[0].value, Some(json!("payload pruned")));
    assert_eq!(calls[0].request, Some(json!("payload pruned")));
}

#[test]
fn rig_readers_observe_a_real_retained_staged_run() {
    use basal_core::{
        ActivationEnd, Admission, Clock, Durability, InstallGate, InstallRequest, NoHooks, Runtime,
        Store,
    };
    use basal_host::{CallClass, CallRequest, CompletionSink, Dispatched, Host, TransportError};
    use basal_proto::{ActivationResult, CallKind, HostCall, JsonText, Primitive, WorkerMessage};
    use std::sync::Arc;
    struct LocalHost;
    impl Host for LocalHost {
        fn classify(&self, _: &CallKind) -> CallClass {
            CallClass::Query
        }
        fn dispatch(&self, _: &CallRequest) -> Result<Dispatched, TransportError> {
            panic!("local-only flow must not dispatch")
        }
        fn attach(&self, _: Arc<dyn CompletionSink>) {}
        fn now_ms(&self) -> f64 {
            100.0
        }
        fn random(&self) -> f64 {
            0.5
        }
    }
    let scratch = Scratch::new();
    let rt = Runtime::new_staged_scratch(
        Arc::new(Store::open(&scratch.0, Durability::throwaway()).unwrap()),
        Arc::new(LocalHost),
        Arc::new(basal_host::catalog::MockCatalog::standard()),
        Arc::new(NoHooks),
        None,
        basal_core::runtime::Config {
            clock: Clock::manual(100),
            install_gate: InstallGate::Off,
            ..Default::default()
        },
        true,
    )
    .unwrap();
    let manifest = json!({"id":"rig-reader","version":1,"purpose":"Read staged run evidence","explanation":"Produces a staged run for the rig readers to observe; it has no other effects.","trigger":{"schedule":{"interval":"1h"}}});
    let installed = rt
        .install(&InstallRequest {
            script: "return 9;".into(),
            manifest: manifest.to_string(),
            author: "operator".into(),
            loop_override: false,
        })
        .unwrap();
    rt.approve("rig-reader", 1, &installed.code_hash, "rig test")
        .unwrap();
    let Admission::Admitted { run_id } = rt
        .admit_trigger("rig-reader", "first", JsonText::null())
        .unwrap()
    else {
        panic!("run not admitted")
    };
    let call = |position, primitive, args: &str| {
        WorkerMessage::HostCall(HostCall {
            position,
            kind: CallKind::Primitive(primitive),
            args: JsonText::new(args).unwrap(),
        })
    };
    let mut worker = basal_core::test_support::ScriptedWorker::new(vec![
        call(0, Primitive::Now, "null"),
        call(1, Primitive::KvGet, "{\"key\":\"missing\"}"),
        WorkerMessage::Blocked { awaiting: vec![1] },
        WorkerMessage::Finished {
            activation_id: 0,
            result: ActivationResult::Completed {
                value: JsonText::new("9").unwrap(),
            },
        },
    ]);
    assert_eq!(
        rt.activate(&run_id, &mut worker).unwrap(),
        ActivationEnd::Succeeded { value: "9".into() }
    );
    let evidence = BasalStore {
        path: scratch.0.clone(),
    };
    let calls = evidence.calls("rig-reader").unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].settlement.as_deref(), Some("fulfilled"));
    assert_eq!(calls[0].value, Some(json!(100)));
    assert_eq!(evidence.activations(&run_id).unwrap(), 1);
    let runs = evidence.runs("rig-reader").unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].state, "succeeded");
    assert_eq!(runs[0].result, Some(json!(9)));
    rt.store()
        .read(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE name='runs'",
                    [],
                    |r| r.get::<_, u64>(0)
                )?,
                0
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn log_backed_rig_readers_refuse_missing_detail_instead_of_returning_empty_evidence() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE run_history(run_id TEXT);")
        .unwrap();
    let evidence = BasalStore {
        path: scratch.0.clone(),
    };
    assert!(
        evidence
            .calls("flow")
            .unwrap_err()
            .contains("requires retain_detail")
    );
    assert!(
        evidence
            .runs("flow")
            .unwrap_err()
            .contains("requires retain_detail")
    );
    assert!(
        evidence
            .activations("run")
            .unwrap_err()
            .contains("requires retain_detail")
    );
}

#[test]
fn digest_evidence_reads_the_recipient_and_flow_from_core_rows() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE wake_fire(fire_id TEXT, agent_id TEXT, origin_flow_id TEXT, origin_flow_version INTEGER);
        INSERT INTO wake_fire VALUES ('foreign-hit','foreign','probe',1), ('owner-hit','owner','probe',1), ('other-flow','foreign','different',2);").unwrap();
    let store = CoreStore {
        path: scratch.0.clone(),
    };
    assert_eq!(
        store.digest_rows("foreign", "probe").unwrap(),
        json!([{"fire_id":"foreign-hit","agent_id":"foreign","flow_id":"probe","version":1}])
    );
    assert_eq!(store.digest_rows("absent", "probe").unwrap(), json!([]));
    c.execute_batch("DROP TABLE wake_fire;").unwrap();
    assert!(store.digest_rows("foreign", "probe").is_err());
}

#[test]
fn package_intent_evidence_keeps_the_provider_rejection_and_rendered_body() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE package_consent_intent(package TEXT,version INTEGER,attempt INTEGER,provider TEXT,request_json TEXT,state TEXT,elicitation_id TEXT);
        INSERT INTO package_consent_intent VALUES ('package',1,2,'cingulate','{\"kind\":\"package\",\"package\":{\"manifest_json\":\"exact bytes\"}}','rejected',NULL);
        INSERT INTO package_consent_intent VALUES ('other',1,3,'other-provider','{}','prepared',NULL);").unwrap();
    let store = CoreStore {
        path: scratch.0.clone(),
    };
    let intent = store.package_intent("package").unwrap().unwrap();
    assert_eq!(intent["provider"], "cingulate");
    assert_eq!(intent["state"], "rejected");
    assert_eq!(intent["attempt"], 2);
    assert_eq!(intent["request"]["package"]["manifest_json"], "exact bytes");
    assert_eq!(store.package_intent("absent").unwrap(), None);
    c.execute_batch("UPDATE package_consent_intent SET request_json='not json';")
        .unwrap();
    assert!(store.package_intent("package").is_err());
}

#[test]
fn flow_enable_evidence_reads_basals_state_column() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE flows(flow_id TEXT,state TEXT); INSERT INTO flows VALUES ('enabled-flow','enabled'),('disabled-flow','disabled');").unwrap();
    let store = BasalStore {
        path: scratch.0.clone(),
    };
    assert_eq!(store.flow_enabled("enabled-flow").unwrap(), Some(true));
    assert_eq!(store.flow_enabled("disabled-flow").unwrap(), Some(false));
    assert_eq!(store.flow_enabled("absent").unwrap(), None);
}

#[test]
fn install_intent_reads_only_the_exact_flow_version_attempt_and_hash() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE flow_install_intent(flow_id TEXT,version INTEGER,attempt INTEGER,code_hash TEXT,provider TEXT,request_json TEXT,state TEXT,elicitation_id TEXT);
        INSERT INTO flow_install_intent VALUES ('flow',1,2,'hash','cingulate','{\"flow_install\":{\"manifest_json\":\"exact bytes\"}}','sent','el_exact');
        INSERT INTO flow_install_intent VALUES ('other',1,2,'hash','cingulate','{}','sent','el_other');
        INSERT INTO flow_install_intent VALUES ('flow',2,2,'hash','cingulate','{}','sent','el_version');
        INSERT INTO flow_install_intent VALUES ('flow',1,3,'hash','cingulate','{}','sent','el_attempt');
        INSERT INTO flow_install_intent VALUES ('flow',1,2,'changed','cingulate','{}','sent','el_hash');").unwrap();
    let store = CoreStore {
        path: scratch.0.clone(),
    };
    let intent = store.install_intent("flow", 1, 2, "hash").unwrap().unwrap();
    assert_eq!(intent["elicitation_id"], "el_exact");
    assert_eq!(intent["flow_id"], "flow");
    assert_eq!(intent["version"], 1);
    assert_eq!(intent["attempt"], 2);
    assert_eq!(intent["code_hash"], "hash");
    assert_eq!(
        intent["request"]["flow_install"]["manifest_json"],
        "exact bytes"
    );
    assert_eq!(intent["provider"], "cingulate");
    assert_eq!(intent["state"], "sent");
    assert_eq!(store.install_intent("absent", 1, 2, "hash").unwrap(), None);
    assert_eq!(store.install_intent("flow", 1, 4, "hash").unwrap(), None);
    c.execute_batch("UPDATE flow_install_intent SET request_json='invalid json' WHERE elicitation_id='el_exact';").unwrap();
    assert!(store.install_intent("flow", 1, 2, "hash").is_err());
}

#[test]
fn instance_evidence_keeps_removal_separate_from_enablement_and_generation() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE flows(flow_id TEXT,owner TEXT,package TEXT,state TEXT,approved_version INTEGER,removed INTEGER,disabled_by TEXT,disabled_reason TEXT,disabled_at INTEGER);
        CREATE TABLE instance_generations(package TEXT,agent_id TEXT,generation INTEGER,operation TEXT,reply TEXT);
        INSERT INTO flows VALUES ('instance','owner','package','enabled',1,1,NULL,NULL,NULL);
        INSERT INTO instance_generations VALUES ('package','owner',1007,'remove','{}');").unwrap();
    let store = BasalStore {
        path: scratch.0.clone(),
    };
    let row = store.instance("instance").unwrap().unwrap();
    assert_eq!(row["removed"], true);
    assert_eq!(row["generation"], 1007);
    assert_eq!(row["operation"], "remove");
    assert_eq!(row["agent_id"], "owner");
    assert_eq!(row["state"], "enabled");
    c.execute_batch("UPDATE flows SET state='disabled',disabled_by='operator',removed=0;")
        .unwrap();
    let row = store.instance("instance").unwrap().unwrap();
    assert_eq!(row["removed"], false);
    assert_eq!(row["disabled_by"], "operator");
    assert_eq!(row["state"], "disabled");
    assert_eq!(store.instance("absent").unwrap(), None);
}

#[test]
fn card_decision_observations_are_keyed_to_the_exact_attempt_and_committed_row() {
    let scratch = Scratch::new();
    let c = throwaway_connection(&scratch.0);
    c.execute_batch("CREATE TABLE flow_install_intent(flow_id TEXT,version INTEGER,attempt INTEGER,state TEXT,code_hash TEXT);
        CREATE TABLE flow_install(flow_id TEXT,version INTEGER,code_hash TEXT,approved_at_ms INTEGER,revoked_at_ms INTEGER,approved_by TEXT,author_agent_id TEXT);
        CREATE TABLE flow_install_decision_hint(flow_id TEXT,version INTEGER,attempt INTEGER,terminal_state TEXT,created_at INTEGER,delivered_at INTEGER);
        INSERT INTO flow_install_intent VALUES ('flow',1,2,'sent','hash');
        INSERT INTO flow_install_decision_hint VALUES ('flow',1,1,'executed',5,6);
        CREATE TABLE install_cards(flow_id TEXT,version INTEGER,attempt INTEGER,state TEXT,decided_at INTEGER);
        CREATE TABLE flows(flow_id TEXT,state TEXT,approved_version INTEGER);
        CREATE TABLE installs(flow_id TEXT,version INTEGER,approved_at INTEGER);
        INSERT INTO install_cards VALUES ('flow',1,2,'pending',NULL);
        INSERT INTO flows VALUES ('flow','unapproved',NULL);
        INSERT INTO installs VALUES ('flow',1,NULL);").unwrap();
    let core = CoreStore {
        path: scratch.0.clone(),
    };
    let basal = BasalStore {
        path: scratch.0.clone(),
    };
    assert_eq!(core.install_decision("flow", 1, 1).unwrap(), None);
    assert_eq!(basal.install_decision("flow", 1, 1).unwrap(), None);
    let pending = core.install_decision("flow", 1, 2).unwrap().unwrap();
    assert_eq!(pending["state"], "sent");
    assert!(pending["installed_hash"].is_null());
    assert!(pending["hint_delivered_at_ms"].is_null());
    assert_eq!(
        basal.install_decision("flow", 1, 2).unwrap().unwrap()["state"],
        "pending"
    );
    c.execute_batch(
        "INSERT INTO flow_install VALUES ('flow',1,'hash',10,NULL,'card','agent');
        UPDATE flow_install_intent SET state='executed';
        INSERT INTO flow_install_decision_hint VALUES ('flow',1,2,'executed',10,11);
        UPDATE install_cards SET state='approved',decided_at=11;
        UPDATE flows SET state='enabled',approved_version=1;
        UPDATE installs SET approved_at=11;",
    )
    .unwrap();
    let approved = core.install_decision("flow", 1, 2).unwrap().unwrap();
    assert_eq!(approved["installed_hash"], "hash");
    assert_eq!(approved["approved_at_ms"], 10);
    assert_eq!(approved["hint_delivered_at_ms"], 11);
    let accepted = basal.install_decision("flow", 1, 2).unwrap().unwrap();
    assert_eq!(accepted["approved_version"], 1);
    assert_eq!(accepted["decided_at_ms"], 11);
    assert_eq!(accepted["flow_state"], "enabled");
    let policy = core.policy_install("flow", 1).unwrap().unwrap();
    assert_eq!(policy["approved_by"], "card");
    assert_eq!(policy["card_intents"], 1);
    assert_eq!(policy["card_hints"], 2);
    c.execute("UPDATE flow_install SET revoked_at_ms=12", [])
        .unwrap();
    assert!(core.install_decision("flow", 1, 2).unwrap().unwrap()["installed_hash"].is_null());
    assert_eq!(core.policy_install("flow", 1).unwrap(), None);
}
