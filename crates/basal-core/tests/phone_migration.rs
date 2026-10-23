use basal_core::schema::MIGRATIONS;
use rusqlite::Connection;

#[test]
fn phone_removal_migration_preserves_populated_history() {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version < 39) {
        c.execute_batch(migration.statements).unwrap();
    }
    c.execute_batch("INSERT INTO flows(flow_id,owner,created_at) VALUES ('flow_synthetic','agent_synthetic',1);
        INSERT INTO flows(flow_id,owner,created_at,removed) VALUES ('instance_synthetic','agent_synthetic',2,1);
        INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at)
        VALUES ('flow_synthetic',1,zeroblob(32),'{}','return 1;','agent_synthetic','validated',3);").unwrap();
    let before: (String, String, i64) = c
        .query_row(
            "SELECT manifest,script,installed_at FROM installs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let migration = MIGRATIONS.iter().find(|m| m.version == 39).unwrap();
    c.execute_batch(migration.statements).unwrap();
    let after: (String, String, i64) = c
        .query_row(
            "SELECT manifest,script,installed_at FROM installs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(before, after);
    let rows: Vec<(String, bool, Option<String>, Option<i64>)> = c
        .prepare("SELECT flow_id,removed,removed_by,removed_at FROM flows ORDER BY flow_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("flow_synthetic".into(), false, None, None),
            ("instance_synthetic".into(), true, None, None)
        ]
    );
    c.execute("UPDATE flows SET removed=1,removed_by='operator',removed_at=4 WHERE flow_id='flow_synthetic'", []).unwrap();
    let audit: (String, i64) = c
        .query_row(
            "SELECT removed_by,removed_at FROM flows WHERE flow_id='flow_synthetic'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(audit, ("operator".into(), 4));
    assert!(
        c.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[test]
fn phone_migration_keeps_the_version_chain_contiguous() {
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        assert_eq!(migration.version as usize, index + 1);
    }
    assert_eq!(MIGRATIONS.last().unwrap().version, 39);
}
