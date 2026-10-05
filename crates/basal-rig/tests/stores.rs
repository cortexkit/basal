use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use basal_rig::stores::{BasalStore, BrocaStore, CoreStore};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "basal-rig-store-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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
    // Address pinned independently to Broca's FNV-1a bind encoding.
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
