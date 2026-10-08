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

#[test]
fn git_waiter_timeout_kills_the_process_group_and_reaps_the_child() {
    let directory = std::env::temp_dir().join(format!("basal-git-timeout-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let pid_file = directory.join("pid");
    let survivor = directory.join("survivor");
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "echo $$ > \"$1\"; (sleep \"$3\"; echo survived > \"$2\") & wait",
            "sh",
        ])
        .arg(&pid_file)
        .arg(&survivor)
        .arg((TIMEOUT.as_secs() + 2).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let error = run_command(command).err().expect("git must time out");
    let pid: i32 = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The bounded descendant writes only if timeout cleanup fails to kill the
    // group. No elapsed-time assertion is needed to observe that failure.
    let survived = survivor.exists();
    std::fs::remove_dir_all(directory).unwrap();
    assert_eq!(error.code, codes::TIMEOUT);
    assert!(!survived, "git's descendant survived the timeout");
    // SAFETY: pid names the child this test spawned. A completed waiter must
    // already have reaped it, leaving no child for waitpid to collect.
    assert_eq!(
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
