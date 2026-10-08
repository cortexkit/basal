use super::*;

#[test]
fn overflowing_or_malformed_log_timestamps_are_refused_not_dropped() {
    assert!(parse_log("abc\0bad\0subject\n").is_err());
    assert!(parse_log("abc\09223372036854775807\0subject\n").is_err());
    assert_eq!(
        parse_log("abc\x001\0subject\n").unwrap()[0]["author_ms"],
        1000
    );
}

#[test]
fn show_distinguishes_missing_blob_from_broken_repository() {
    let ran = Ran {
        success: false,
        code: Some(128),
        stdout: vec![],
        stderr: "fatal: detected dubious ownership in repository".into(),
    };
    assert_eq!(show_failure(&ran).code, codes::GIT);
}

#[test]
fn tag_output_is_bounded_while_reading_not_after_completion() {
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "i=0; while [ \"$i\" -lt 4000 ]; do printf 'tag-%080d\\n' \"$i\"; i=$((i+1)); done",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let result = run_tags_command(command).unwrap();
    assert!(result.success);
    assert_eq!(
        String::from_utf8(result.stdout).unwrap().lines().count(),
        MAX_TAGS
    );
}
