use super::MIGRATIONS;
use rusqlite::Connection;

#[test]
fn migration_51_preserves_old_delivery_and_skips_historical_owner_notices() {
    let conn = Connection::open_in_memory().unwrap();
    for migration in MIGRATIONS.iter().filter(|m| m.version <= 50) {
        conn.execute_batch(migration.statements).unwrap();
    }
    conn.execute_batch(
        "INSERT INTO outbox(at,kind,flow_id,body,delivered_at,attempts,retry_at) VALUES
        (1,'flow.owner_notice','f','{}',NULL,3,900),
        (2,'flow.owner_notice','f','{}',4,2,700),
        (3,'secret.revoke_flow_grants','g','{}',NULL,1,500);",
    )
    .unwrap();
    let rows = |sql: &str| -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let query = "SELECT json_array(seq,at,kind,flow_id,body,delivered_at,attempts,retry_at) FROM outbox ORDER BY seq";
    let before = rows(query);
    conn.execute_batch(
        MIGRATIONS
            .iter()
            .find(|m| m.version == 51)
            .unwrap()
            .statements,
    )
    .unwrap();
    assert_eq!(rows(query), before);
    assert_eq!(conn.query_row("SELECT COUNT(*) FROM outbox WHERE kind='flow.owner_notice' AND bus_published_at IS NULL", [], |r|r.get::<_,i64>(0)).unwrap(),0);
    conn.execute_batch(
        "INSERT INTO outbox(at,kind,flow_id,body) VALUES(5,'flow.owner_notice','f','{}')",
    )
    .unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT json_array(bus_published_at,bus_attempts,bus_retry_at) FROM outbox WHERE at=5",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "[null,0,0]"
    );
    assert_eq!(rows("PRAGMA integrity_check"), ["ok"]);
}
