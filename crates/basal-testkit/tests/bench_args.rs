use basal_testkit::command::Command;

#[test]
fn worker_benchmark_refuses_invalid_arguments_before_spawning() {
    for args in [
        vec!["--worker", "/missing-worker", "--fuzz", "garbage"],
        vec!["--worker", "/missing-worker", "--out"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_worker-bench"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(64), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("spawn\n"));
    }
}
