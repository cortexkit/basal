use std::path::PathBuf;

use basal_rig::stores::{BasalStore, BrocaStore, CoreStore};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

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
    let c = Connection::open(&scratch.0).unwrap();
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
    let c = Connection::open(&scratch.0).unwrap();
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
        json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"opaque-scope","scope_epoch":9})
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
    let c = Connection::open(&scratch.0).unwrap();
    c.execute_batch(
        "CREATE TABLE runs(run_id TEXT, flow_id TEXT);
        CREATE TABLE journal(run_id TEXT, position INTEGER, kind_code INTEGER, dispatch TEXT,
            attempts INTEGER, settlement TEXT, value TEXT, request TEXT);
        INSERT INTO runs VALUES ('run', 'flow');
        INSERT INTO journal VALUES ('run', -1, 7, 'sent', 1, 'fulfilled', '{}', '{}');",
    )
    .unwrap();
    assert!(
        BasalStore {
            path: scratch.0.clone()
        }
        .calls("flow")
        .is_err()
    );
}

#[test]
fn transcript_evidence_rejects_ambiguous_session_keys() {
    let scratch = Scratch::new();
    Connection::open(&scratch.0)
        .unwrap()
        .execute_batch("CREATE TABLE message(session TEXT, ord INTEGER, json TEXT);")
        .unwrap();
    let store = BrocaStore {
        path: scratch.0.clone(),
    };
    assert!(
        store
            .final_text(
                &json!({"project_root":"/rig", "harness":"basal", "session":"bad\u{1f}session"})
            )
            .is_err()
    );
}

#[test]
fn independent_broca_evidence_matches_the_entire_bind_and_counts_duplicate_runs() {
    let scratch = Scratch::new();
    let c = Connection::open(&scratch.0).unwrap();
    c.execute_batch(
        "CREATE TABLE run_index (run_id TEXT, session TEXT, state TEXT, usage_json TEXT);
        CREATE TABLE message (session TEXT, ord INTEGER, json TEXT);",
    )
    .unwrap();
    let route = json!({"project_root":"/rig","harness":"basal","session":"first"});
    let other = json!({"project_root":"/other","harness":"basal","session":"first"});
    for (id, bind) in [("run1", &route), ("unrelated", &other)] {
        c.execute(
            "INSERT INTO run_index VALUES (?1, ?2, 'completed', ?3)",
            params![
                id,
                bind.to_string(),
                json!({"input_tokens":11,"output_tokens":2}).to_string()
            ],
        )
        .unwrap();
    }
    c.execute("INSERT INTO message VALUES (?1, 1, ?2)", params!["/rig\u{1f}basal\u{1f}first",
        json!({"role":"assistant","content":[{"type":"reasoning","text":"private"},{"type":"text","text":"hel"},{"type":"text","text":"lo"}]}).to_string()]).unwrap();
    let store = BrocaStore {
        path: scratch.0.clone(),
    };
    let rows = store.runs(&route).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["run_id"], "run1");
    assert_eq!(rows[0]["usage"]["input_tokens"], 11);
    assert_eq!(store.final_text(&route).unwrap(), Some("hello".into()));
    assert!(
        store
            .runs(&json!({"project_root":"/rig","harness":"basal","session":"unsent"}))
            .unwrap()
            .is_empty()
    );
    assert!(store.runs(&Value::Null).is_err());
    c.execute(
        "INSERT INTO run_index VALUES ('run2', ?1, 'completed', '{}')",
        [route.to_string()],
    )
    .unwrap();
    assert_eq!(
        store.runs(&route).unwrap().len(),
        2,
        "a duplicate send must not be collapsed"
    );
}

#[test]
fn model_journal_and_ledger_reads_are_scoped_to_the_run_and_preserve_missing_usage() {
    let scratch = Scratch::new();
    let c = Connection::open(&scratch.0).unwrap();
    c.execute_batch(
        "CREATE TABLE journal (run_id TEXT, position INTEGER, args TEXT, request TEXT);
        CREATE TABLE broca_calls (run_id TEXT, position INTEGER, snapshot TEXT);
        CREATE TABLE token_ledger (run_id TEXT, position INTEGER, state TEXT, reserved INTEGER,
            input_tokens INTEGER, cache_write_tokens INTEGER, output_tokens INTEGER,
            cached_input_tokens INTEGER, unreported_tokens INTEGER);
        INSERT INTO journal VALUES ('first', 0, '{\"prompt\":\"authored\"}', '{\"selection\":{\"decisionID\":\"d1\"}}');
        INSERT INTO broca_calls VALUES ('first', 0, '{\"broca_run_id\":\"b1\"}');
        INSERT INTO token_ledger VALUES ('first', 0, 'settled', 800, 9, NULL, 2, 1, 789);",
    )
    .unwrap();
    let store = BasalStore {
        path: scratch.0.clone(),
    };
    let call = store.model_call("first").unwrap().unwrap();
    assert_eq!(call["journal"]["selection"]["decisionID"], "d1");
    assert_eq!(call["broca"]["broca_run_id"], "b1");
    let charge = store.ledger("first").unwrap();
    assert_eq!(charge.len(), 1);
    assert_eq!(charge[0]["reserved"], 800);
    assert_eq!(charge[0]["cache_write_tokens"], Value::Null);
    assert_eq!(charge[0]["unreported_tokens"], 789);
    assert!(store.model_call("unsent").unwrap().is_none());
    assert!(store.ledger("unsent").unwrap().is_empty());
}

#[test]
fn ownership_reads_brocas_attestation_not_the_callers_harness_label() {
    let scratch = Scratch::new();
    let c = Connection::open(&scratch.0).unwrap();
    c.execute_batch("CREATE TABLE meta (session TEXT, admission_checkpoint_json TEXT);")
        .unwrap();
    let owner = json!({"episodes":1,"first_principal":{"kind":"direct"},
        "recorded_principals":[{"kind":"direct"}]});
    c.execute(
        "INSERT INTO meta VALUES (?1, ?2)",
        params![
            "/rig\u{1f}basal\u{1f}first",
            json!({"version":1,"checkpoint":{"ownership":owner}}).to_string()
        ],
    )
    .unwrap();
    let store = BrocaStore {
        path: scratch.0.clone(),
    };
    let route = json!({"project_root":"/rig","harness":"basal","session":"first"});
    let recorded = store.ownership(&route).unwrap();
    assert_eq!(recorded["first_principal"], json!({"kind":"direct"}));
    assert_eq!(recorded["episodes"], 1);
    assert_eq!(recorded["scope"], Value::Null);
    assert_eq!(
        recorded["recorded_principals"],
        owner["recorded_principals"]
    );
    assert!(
        store
            .ownership(&json!({"project_root":"/other","harness":"basal","session":"first"}))
            .is_err()
    );
}

#[test]
fn core_scope_reader_requires_this_active_version_and_an_accepted_registration() {
    let scratch = Scratch::new();
    let c = Connection::open(&scratch.0).unwrap();
    c.execute_batch("CREATE TABLE agent (agent_id TEXT, terminal_reason TEXT);
        CREATE TABLE flow_scope (flow_id TEXT, scope_ref TEXT, scope_epoch INTEGER, agent_id TEXT,
            flow_scope_targets_json TEXT, removed_at_ms INTEGER);
        CREATE TABLE flow_install (flow_id TEXT, version INTEGER, author_agent_id TEXT, revoked_at_ms INTEGER);
        INSERT INTO agent VALUES ('agent-a', NULL), ('retired', 'done');
        INSERT INTO flow_scope VALUES ('owned','ref-a',7,'agent-a','[\"broca\"]',NULL),
            ('global','ref-g',2,NULL,'[\"broca\"]',NULL),
            ('retired','ref-r',1,'retired','[]',NULL),
            ('pending','ref-p',1,NULL,NULL,NULL),
            ('removed','ref-x',1,NULL,'[]',42);
        INSERT INTO flow_install VALUES ('owned',1,'agent-a',NULL), ('owned',2,'agent-a',42),
            ('owned',3,'other-agent',NULL), ('global',1,NULL,NULL), ('retired',1,'retired',NULL),
            ('pending',1,NULL,NULL), ('removed',1,NULL,NULL);").unwrap();
    let store = CoreStore {
        path: scratch.0.clone(),
    };
    assert_eq!(
        store.flow_scope("owned", 1).unwrap(),
        Some(
            json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"ref-a","epoch":7})
        )
    );
    assert_eq!(
        store.flow_scope("global", 1).unwrap().unwrap()["ref"],
        "ref-g"
    );
    for (flow, version) in [
        ("owned", 2),
        ("owned", 3),
        ("owned", 4),
        ("retired", 1),
        ("pending", 1),
        ("removed", 1),
        ("missing", 1),
    ] {
        assert_eq!(
            store.flow_scope(flow, version).unwrap(),
            None,
            "{flow} {version}"
        );
    }
    assert_eq!(
        c.query_row("SELECT COUNT(*) FROM flow_scope", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        5
    );
}

#[test]
fn scoped_ownership_preserves_ref_epoch_flow_and_attested_sender_for_the_exact_bind() {
    let scratch = Scratch::new();
    let c = Connection::open(&scratch.0).unwrap();
    c.execute_batch("CREATE TABLE meta (session TEXT, admission_checkpoint_json TEXT);")
        .unwrap();
    let owner = json!({"scope":{"owner":{"kind":"reserved","module_id":"prefrontal-core"},
        "ref":"core-ref","scope_epoch":7,"flow_id":"flow-a","sender":{"kind":"reserved","module_id":"basal"}},
        "first_principal":{"kind":"reserved","module_id":"basal"},
        "recorded_principals":[{"kind":"reserved","module_id":"basal"}],"episodes":1});
    c.execute(
        "INSERT INTO meta VALUES (?1, ?2)",
        params![
            "/rig\u{1f}basal\u{1f}first",
            json!({"checkpoint":{"ownership":owner}}).to_string()
        ],
    )
    .unwrap();
    let store = BrocaStore {
        path: scratch.0.clone(),
    };
    let route = json!({"project_root":"/rig","harness":"basal","session":"first"});
    assert_eq!(store.ownership(&route).unwrap(), owner);
    assert_eq!(store.meta_count(&route).unwrap(), 1);
    assert_eq!(
        store
            .meta_count(&json!({"project_root":"/other","harness":"basal","session":"first"}))
            .unwrap(),
        0
    );
    assert!(store.meta_count(&Value::Null).is_err());
}

#[test]
fn wal_no_record_reader_rejects_record_bytes_and_partial_writes_at_the_pinned_address() {
    use sha2::{Digest, Sha256};
    let scratch = Scratch::new();
    let dir = scratch.0.with_extension("dir");
    std::fs::create_dir_all(dir.join("wal")).unwrap();
    let store = BrocaStore {
        path: dir.join("run-index.db"),
    };
    let route = json!({"project_root":"/rig","harness":"basal","session":"first"});
    // The WAL file name Broca derives for this bind triple, computed separately
    // and pinned here so the reader's own hashing is checked against it.
    let path = dir.join("wal/2c81ab058ec72219.wal");
    assert!(store.wal_has_no_records(&route).unwrap());
    std::fs::write(&path, b"a record or a partial frame").unwrap();
    assert!(!store.wal_has_no_records(&route).unwrap());
    let envelope = [vec![2], vec![0; 16], vec![42; 16]].concat();
    let lineage = [
        16_u32.to_le_bytes().to_vec(),
        envelope[..17].to_vec(),
        Sha256::digest(&envelope).to_vec(),
        envelope[17..].to_vec(),
    ]
    .concat();
    std::fs::write(&path, &lineage).unwrap();
    assert!(store.wal_has_no_records(&route).unwrap());
    let mut bytes = lineage.clone();
    bytes.push(0);
    std::fs::write(&path, bytes).unwrap();
    assert!(!store.wal_has_no_records(&route).unwrap());
    let mut bytes = lineage;
    bytes[21] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    assert!(!store.wal_has_no_records(&route).unwrap());
    assert!(store.wal_has_no_records(&Value::Null).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn digest_evidence_reads_the_recipient_and_flow_from_core_rows() {
    let scratch = Scratch::new();
    let c = Connection::open(&scratch.0).unwrap();
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
    let c = Connection::open(&scratch.0).unwrap();
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
    let c = Connection::open(&scratch.0).unwrap();
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
    let c = Connection::open(&scratch.0).unwrap();
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
    let c = Connection::open(&scratch.0).unwrap();
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
    let c = Connection::open(&scratch.0).unwrap();
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
