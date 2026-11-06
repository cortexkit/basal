#[test]
fn next_wake_includes_the_install_gate_retry() {
    let fixture = crate::runtime_authorization_regressions::TestRuntime::new();
    let rt = &fixture.rt;
    rt.store()
        .write(|tx| {
            tx.execute(
                "INSERT INTO flows(flow_id,created_at) VALUES('waiting',0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
    rt.fixture_run(
        "waiting",
        "waiting",
        1,
        "return 1",
        "{}",
        crate::run_records::State::new(0),
    )
    .unwrap();
    *rt.shared.retention_next.lock().unwrap() = Some(i64::MAX);
    rt.back_off("waiting").unwrap();
    let at = rt.shared.gate_backoff.lock().unwrap()["waiting"].retry_at_ms;
    assert_eq!(rt.next_wake_at(None).unwrap(), Some(at));
}

#[test]
fn new_gate_backoffs_reap_terminal_and_pruned_runs() {
    let fixture = crate::runtime_authorization_regressions::TestRuntime::new();
    let rt = &fixture.rt;
    rt.store()
        .write(|tx| {
            tx.execute("INSERT INTO flows(flow_id,created_at) VALUES('f',0)", [])?;
            Ok(())
        })
        .unwrap();
    for id in ["done", "gone", "waiting"] {
        rt.fixture_run(
            id,
            "f",
            1,
            "return 1",
            "{}",
            crate::run_records::State::new(1),
        )
        .unwrap();
        rt.back_off(id).unwrap();
    }
    for id in ["done", "gone"] {
        rt.fixture_finish(id, crate::RunState::Cancelled, None, None)
            .unwrap();
    }
    rt.store()
        .write(|tx| {
            tx.execute("DELETE FROM run_history WHERE run_id='gone'", [])?;
            Ok(())
        })
        .unwrap();
    rt.back_off("waiting").unwrap();
    let table = rt.shared.gate_backoff.lock().unwrap();
    assert_eq!(table.len(), 1);
    assert_eq!(table["waiting"].failures, 2);
}
