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
