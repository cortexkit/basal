use super::*;
use rusqlite::Connection;

#[test]
fn decision_path_migration_preserves_all_legacy_intents_and_withdrawals() {
    let conn = Connection::open_in_memory().unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 17) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at) VALUES ('lost-reply','reenable','flow',1,1,'{}','open',0),('unraised','reenable','other',1,1,'{}','open',0); INSERT INTO decision_withdrawals VALUES ('legacy-id');").unwrap();
    conn.execute_batch(DECISION_PATHS).unwrap();
    let paths: Vec<String> = conn
        .prepare("SELECT consent_path FROM decision_cards ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(paths, ["legacy", "legacy"]);
    let withdrawal: (String, String) = conn
        .query_row(
            "SELECT consent_path,elicitation_id FROM decision_withdrawals",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(withdrawal, ("legacy".into(), "legacy-id".into()));
    conn.execute_batch("INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at) VALUES ('new','reenable','new',1,1,'{}','open',0);").unwrap();
    assert!(
        conn.query_row(
            "SELECT consent_path FROM decision_cards WHERE dedup_key='new'",
            [],
            |r| r.get::<_, Option<String>>(0)
        )
        .unwrap()
        .is_none()
    );
}
