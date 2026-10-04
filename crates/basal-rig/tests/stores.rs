use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use basal_rig::stores::{BasalStore, BrocaStore};
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
        "CREATE TABLE journal (run_id TEXT, position INTEGER, args TEXT);
        CREATE TABLE broca_calls (run_id TEXT, position INTEGER, snapshot TEXT);
        CREATE TABLE token_ledger (run_id TEXT, position INTEGER, state TEXT, reserved INTEGER,
            input_tokens INTEGER, cache_write_tokens INTEGER, output_tokens INTEGER,
            cached_input_tokens INTEGER, unreported_tokens INTEGER);
        INSERT INTO journal VALUES ('first', 0, '{\"selection\":{\"decisionID\":\"d1\"}}');
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
