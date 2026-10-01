use std::process::Command;
#[test]
fn unknown_control_flags_refuse_without_running_any_set() {
    for args in [
        vec!["--does-not-exist"],
        vec!["--model-host"],
        vec!["--hosts", "--does-not-exist"],
        vec!["--check", "--does-not-exist"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mutation-controls"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        for known in [
            "--worker",
            "--journal",
            "--dispatch",
            "--schedule",
            "--module",
            "--broca",
            "--hosts",
        ] {
            assert!(stderr.contains(known), "{stderr}");
        }
        assert!(stderr.contains("unknown control flag"));
        assert!(output.stdout.is_empty());
    }
}
#[test]
fn two_control_sets_are_refused_instead_of_becoming_a_filter() {
    let output = Command::new(env!("CARGO_BIN_EXE_mutation-controls"))
        .args(["--hosts", "--broca"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("choose exactly one")
    );
}
