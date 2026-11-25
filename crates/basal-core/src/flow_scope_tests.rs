use super::LIVE_RUN_VERSIONS_SQL;
use crate::runtime_authorization_regressions::connection;

#[test]
fn retirement_live_run_query_searches_only_the_requested_flow() {
    let c = connection();
    let details = c
        .prepare(&format!("EXPLAIN QUERY PLAN {LIVE_RUN_VERSIONS_SQL}"))
        .unwrap()
        .query_map(["requested-flow"], |r| r.get::<_, String>(3))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert!(
        details
            .iter()
            .any(|line| line.starts_with("SEARCH live_runs ") && line.contains("flow_id=?")),
        "{details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|line| line.starts_with("SCAN live_runs")),
        "{details:?}"
    );
}
