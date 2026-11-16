use super::*;
use serde_json::json;

#[test]
fn attribution_flow_approval_is_atomic_and_owner_changes_supersede() {
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    for m in crate::schema::MIGRATIONS {
        c.execute_batch(m.statements).unwrap();
    }
    let mut approve = |version: u32, owner: &str, rollback: bool| {
        let manifest = json!({"id":"owned","version":version,"explanation":"Exercises ownership attribution without external effects.","purpose":"Attribute approved ownership","trigger":{"schedule":{"interval":"1h"}},"ops":[]}).to_string();
        let manifest = Manifest::parse_ordinary(&manifest).unwrap();
        let request = InstallRequest {
            script: "return 1;".into(),
            manifest: serde_json::to_string(&manifest).unwrap(),
            author: owner.into(),
            loop_override: false,
        };
        let tx = c.transaction().unwrap();
        let installed = record(
            &tx,
            &manifest,
            &request,
            Validation {
                warnings: vec![],
                tool_policies: Default::default(),
            },
            version as i64 * 100,
        )
        .unwrap();
        let before: i64 = tx
            .query_row("SELECT count(*) FROM attribution_facts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before,
            if version == 1 {
                0
            } else if version <= 3 {
                1
            } else {
                2
            }
        );
        approve_with_resolved_agents(
            &tx,
            "owned",
            version,
            &installed.code_hash,
            "card",
            version as i64 * 100,
            &SchedulerConfig::default(),
            &Default::default(),
        )
        .unwrap();
        if rollback {
            tx.rollback().unwrap();
        } else {
            tx.commit().unwrap();
        }
    };
    approve(1, "agent_a", true);
    approve(1, "agent_a", false);
    approve(2, "agent_a", false);
    approve(3, "agent_b", false);
    approve(4, "agent_a", false);
    let page = crate::attribution::read(&c, 0, 100).unwrap();
    let facts = page["facts"].as_array().unwrap();
    assert_eq!(facts.len(), 3);
    assert_eq!(facts[0]["fact_id"], "basal:flow:owned:v1");
    assert_eq!(facts[1]["supersedes"], "basal:flow:owned:v1");
    assert_eq!(facts[2]["supersedes"], "basal:flow:owned:v3");
    assert_eq!(facts[2]["alfonso"], "agent_a");
    assert_eq!(facts[2]["effective_from_ms"], 400);
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM outbox WHERE kind='attribution.fact'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        3
    );
}
