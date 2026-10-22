use super::*;
use rusqlite::{Connection, OptionalExtension};

#[test]
fn decision_path_migration_preserves_all_legacy_intents_and_withdrawals() {
    let conn = Connection::open_in_memory().unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 18) {
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

#[test]
fn consent_only_migration_reraises_open_legacy_cards_and_refuses_legacy_paths() {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    for migration in MIGRATIONS
        .iter()
        .take_while(|m| m.statements != DECISION_PATHS_CONSENT_ONLY)
    {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at,answered_at,elicitation_id,raised_revision,consent_path) VALUES
            ('open-legacy','reenable','a',1,1,'{}','open',0,NULL,'core-1',1,'legacy'),
            ('closed-legacy','reenable','b',1,1,'{}','declined',0,1,'core-2',1,'legacy'),
            ('unraised','reenable','c',1,1,'{}','open',0,NULL,NULL,NULL,NULL),
            ('provider','reenable','d',1,1,'{}','open',0,NULL,'cg-1',1,'consent:cingulate');
         INSERT INTO decision_superseded_ids VALUES (1,'core-0'),(2,'core-1'),(4,'cg-0');
         INSERT INTO decision_withdrawals VALUES ('legacy','core-3'),('consent:cingulate','cg-2');
         INSERT INTO decision_answer_cursors VALUES ('legacy',9),('consent:cingulate',4);",
    )
    .unwrap();
    conn.execute_batch(DECISION_PATHS_CONSENT_ONLY).unwrap();
    type Row = (String, Option<String>, Option<i64>, Option<String>);
    let cards: Vec<Row> = conn
        .prepare("SELECT dedup_key,elicitation_id,raised_revision,consent_path FROM decision_cards ORDER BY seq")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        cards,
        vec![
            ("open-legacy".into(), None, None, None),
            ("closed-legacy".into(), Some("core-2".into()), Some(1), None),
            ("unraised".into(), None, None, None),
            (
                "provider".into(),
                Some("cg-1".into()),
                Some(1),
                Some("consent:cingulate".into())
            ),
        ]
    );
    let strings = |sql: &str| -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(
        strings("SELECT elicitation_id FROM decision_superseded_ids ORDER BY card_seq"),
        ["core-1", "cg-0"]
    );
    assert_eq!(
        strings("SELECT consent_path||'/'||elicitation_id FROM decision_withdrawals"),
        ["consent:cingulate/cg-2"]
    );
    assert_eq!(
        strings("SELECT consent_path||'/'||cursor FROM decision_answer_cursors"),
        ["consent:cingulate/4"]
    );
    for refused in [
        "UPDATE decision_cards SET consent_path='legacy' WHERE dedup_key='unraised'",
        "UPDATE decision_cards SET consent_path='consent: ' WHERE dedup_key='unraised'",
        "INSERT INTO decision_cards(dedup_key,kind,flow_id,version,instance,card,state,created_at,consent_path) VALUES ('new','reenable','e',1,1,'{}','open',0,'legacy')",
        "INSERT INTO decision_withdrawals VALUES ('legacy','core-4')",
        "UPDATE decision_withdrawals SET consent_path='legacy'",
        "INSERT INTO decision_answer_cursors VALUES ('legacy',1)",
        "UPDATE decision_answer_cursors SET consent_path='legacy'",
    ] {
        assert!(conn.execute(refused, []).is_err(), "{refused}");
    }
    conn.execute(
        "UPDATE decision_cards SET consent_path='consent:cingulate' WHERE dedup_key='unraised'",
        [],
    )
    .unwrap();
}

#[test]
fn install_request_migration_preserves_history_without_inventing_attempts() {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", true).unwrap();
    for migration in MIGRATIONS
        .iter()
        .take_while(|m| m.statements != INSTALL_REQUEST)
    {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch("INSERT INTO flows(flow_id,created_at) VALUES ('flow',0);
        INSERT INTO installs(flow_id,version,code_hash,manifest,script,author,state,installed_at) VALUES ('flow',1,zeroblob(32),'{}','return 1','operator','validated',0),('flow',2,zeroblob(32),'{}','return 2','operator','validated',0);
        INSERT INTO install_cards(card_id,flow_id,version,code_hash,author,card,state,decided_by,created_at,decided_at) VALUES ('pending','flow',1,zeroblob(32),'operator','{}','pending',NULL,0,NULL),('declined-old','flow',2,zeroblob(32),'operator','{}','rejected','operator',0,1);
        UPDATE installs SET card_confirmed=1;").unwrap();
    conn.execute_batch(INSTALL_REQUEST).unwrap();
    let rows: Vec<(String, String, Option<i64>, Option<String>)> = conn
        .prepare("SELECT card_id,state,attempt,decided_by FROM install_cards ORDER BY version")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("pending".into(), "pending".into(), None, None),
            (
                "declined-old".into(),
                "rejected".into(),
                None,
                Some("operator".into())
            )
        ]
    );
    assert!(
        conn.execute(
            "UPDATE install_cards SET attempt=0 WHERE card_id='pending'",
            []
        )
        .is_err()
    );
    for state in ["declined", "expired"] {
        conn.execute("UPDATE install_cards SET attempt=7,state=?1,decided_by='core:card',decided_at=2 WHERE card_id='pending'", [state]).unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT state FROM install_cards WHERE flow_id='flow' AND version=1 AND attempt=7",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            state
        );
    }
    assert!(
        conn.query_row(
            "SELECT * FROM pragma_foreign_key_check LIMIT 1",
            [],
            |_| Ok(())
        )
        .optional()
        .unwrap()
        .is_none()
    );
}
