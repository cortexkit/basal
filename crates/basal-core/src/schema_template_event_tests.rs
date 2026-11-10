use super::MIGRATIONS;
use rusqlite::Connection;

fn strings(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn migration_50_preserves_outbox_and_admits_exact_lifecycle_event_set() {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 49) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO flows(flow_id,created_at) VALUES ('f',1),('g',2);
         INSERT INTO template_instances(flow_id,params,grant_guards,expires_at,seq)
         VALUES ('f','{}','[]',1000,9),('g','{}','[]',2000,4);
         INSERT INTO template_event_outbox(flow_id,seq,event_name,body,published_at,retry_at,attempts)
         VALUES ('f',1,'flow_instance_created','{\"seq\":1,\"next_fire_at_ms\":100}',10,1,0),
                ('f',8,'flow_instance_extended','{\"seq\":8,\"next_fire_at_ms\":200}',NULL,90,3),
                ('f',9,'flow_instance_ended','{\"seq\":9,\"next_fire_at_ms\":null}',NULL,100,0),
                ('g',4,'flow_instance_expired','{\"seq\":4,\"params\":{}}',20,5,2);",
    ).unwrap();
    let rows = "SELECT json_array(flow_id,seq,event_name,body,published_at,retry_at,attempts)
                FROM template_event_outbox ORDER BY flow_id,seq";
    let columns = "SELECT json_array(cid,name,type,\"notnull\",dflt_value,pk)
                   FROM pragma_table_info('template_event_outbox') ORDER BY cid";
    let indexes_and_triggers = "SELECT json_array(type,name,sql) FROM sqlite_schema
        WHERE tbl_name='template_event_outbox' AND type IN ('index','trigger') ORDER BY type,name";
    let sequences = "SELECT json_array(flow_id,seq) FROM template_instances ORDER BY flow_id";
    let before_rows = strings(&conn, rows);
    let before_columns = strings(&conn, columns);
    let before_indexes = strings(&conn, indexes_and_triggers);
    let before_sequences = strings(&conn, sequences);
    let tx = conn.transaction().unwrap();
    tx.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 50)
            .unwrap()
            .statements,
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(strings(&conn, rows), before_rows);
    assert_eq!(strings(&conn, columns), before_columns);
    assert_eq!(strings(&conn, indexes_and_triggers), before_indexes);
    assert_eq!(strings(&conn, sequences), before_sequences);
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name='template_event_outbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let events = sql
        .split_once("CHECK (event_name IN (")
        .unwrap()
        .1
        .split_once("))")
        .unwrap()
        .0
        .split(',')
        .map(|name| name.trim().trim_matches('\''))
        .collect::<Vec<_>>();
    assert_eq!(events, crate::template_events::EVENTS);
    for (offset, event) in events.iter().enumerate() {
        conn.execute(
            "INSERT INTO template_event_outbox(flow_id,seq,event_name,body,retry_at)
             VALUES ('f',?1,?2,'{}',100)",
            rusqlite::params![10 + offset as i64, event],
        )
        .unwrap();
    }
    assert_eq!(
        conn.query_row(
            "SELECT attempts FROM template_event_outbox WHERE flow_id='f' AND seq=14",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(
        conn.execute(
            "INSERT INTO template_event_outbox(flow_id,seq,event_name,body,retry_at)
         VALUES ('f',99,'unknown','{}',100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO template_event_outbox(flow_id,seq,event_name,body,retry_at)
         VALUES ('f',99,'flow_instance_rescheduled','invalid JSON',100)",
            [],
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO template_event_outbox(flow_id,seq,event_name,body,retry_at)
         VALUES ('absent',99,'flow_instance_rescheduled','{}',100)",
            [],
        )
        .is_err()
    );
    assert_eq!(strings(&conn, "PRAGMA integrity_check"), ["ok"]);
    assert!(
        conn.prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}
