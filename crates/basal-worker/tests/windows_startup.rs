//! The worker's Windows startup, run for real: every spawn but two goes
//! through basal-launch under the full confinement, or under one of its test
//! variants that breaks exactly one property a startup check guards.
//!
//! The two exceptions start the worker directly, without a package SID or
//! with an unparsable one. The worker refuses those before it confines
//! itself or reads anything, and the launcher always passes a valid SID, so
//! it cannot produce them.
#![cfg(windows)]

use basal_launch::{
    ConfinedProcess, Deviation, LaunchOptions, create_or_open_profile, grant_test_binary_directory,
    launch,
};
use basal_proto::{
    ActivationRequest, ActivationResult, Budgets, CallKind, Confinement, FLOW_JOB_COMMIT_BYTES,
    JsonText, Outcome, PROTOCOL_VERSION, ParentMessage, Primitive, Profile, Settlement,
    WorkerMessage, read_worker_message, write_parent_message,
};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

mod windows_common;

/// The worker cargo built for these tests, under a development name.
fn worker() -> &'static Path {
    windows_common::dev_binary(env!("CARGO_BIN_EXE_ck-basal-worker"))
}

/// The worker, with its directory readable and executable by the worker's
/// package, so that the confined process can load its own image.
fn placed_worker() -> &'static Path {
    static GRANTED: OnceLock<()> = OnceLock::new();
    GRANTED.get_or_init(|| {
        let package = create_or_open_profile().expect("worker profile");
        grant_test_binary_directory(worker(), &package)
            .expect("grant the package read and execute");
    });
    worker()
}

fn start(deviation: Deviation) -> ConfinedProcess {
    let options = LaunchOptions::new(placed_worker(), FLOW_JOB_COMMIT_BYTES).deviation(deviation);
    launch(&options).unwrap_or_else(|error| panic!("launch {deviation}: {error}"))
}

/// How a worker ended: its exit code and everything it wrote to stderr.
struct Ended {
    code: u32,
    stderr: String,
}

impl Ended {
    /// The reason token in a startup refusal line,
    /// `ck-basal-worker: refusing to run unconfined: <reason>: <detail>`.
    fn reason(&self) -> Option<&str> {
        let rest = self.stderr.split("refusing to run unconfined: ").nth(1)?;
        rest.split(':').next()
    }
}

/// Closes the worker's stdin and waits for it to end. A worker that refused
/// to start has already exited; one that started anyway reads end of input
/// and exits 0, so a missing refusal shows up as a wrong exit code rather
/// than a test that never finishes.
fn end(mut child: ConfinedProcess) -> Ended {
    drop(child.stdin.take());
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read the worker's stderr");
    let code = child.wait().expect("wait for the worker");
    Ended { code, stderr }
}

/// Ends the worker and fails the test with what it wrote to stderr.
fn fail(child: ConfinedProcess, what: String) -> ! {
    let ended = end(child);
    panic!(
        "{what}; worker exit {:#x}, stderr: {}",
        ended.code, ended.stderr
    )
}

/// Launches the worker under `deviation` and requires it to refuse at the
/// startup check that variant breaks: exit 70, that reason on stderr.
fn assert_refused(deviation: Deviation) {
    let expected = deviation
        .worker_reason()
        .unwrap_or_else(|| panic!("{deviation} is not a worker check"));
    let ended = end(start(deviation));
    println!(
        "{deviation}: exit {} stderr {}",
        ended.code,
        ended.stderr.trim()
    );
    assert_eq!(ended.code, 70, "{deviation}: {}", ended.stderr);
    assert_eq!(ended.reason(), Some(expected), "{}", ended.stderr);
}

/// A Windows report with every property true: the only report a worker that
/// passed its startup checks sends.
const CONFINED: Confinement = Confinement::Windows {
    lpac: true,
    untrusted: true,
    no_thread_token: true,
    mitigations: true,
    handle_table: true,
};

/// The time the test parent answers a clock read with: 2026-01-01T00:00:00Z.
const NOW_MS: &str = "1767225600000";

/// A real activation under the full confinement: the worker passes all six
/// startup checks with the launcher's environment, reports the Windows
/// confinement, runs a script that makes a synchronous host call, returns its
/// result and shuts down cleanly. The parent also reads the worker's primary
/// back and finds it Untrusted, which only the worker's own lowering makes it.
#[test]
fn an_activation_completes_under_the_full_confinement() {
    let mut child = start(Deviation::Full);
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = child.stdout.take().expect("stdout");

    write_parent_message(
        &mut stdin,
        &ParentMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
    )
    .expect("send hello");
    let welcome = match read_worker_message(&mut stdout) {
        Ok(WorkerMessage::Welcome(welcome)) => welcome,
        other => fail(child, format!("no welcome: {other:?}")),
    };
    println!("full: welcome {welcome:?}");
    assert_eq!(welcome.confinement, CONFINED);

    let token = child.primary_token().expect("read the worker's primary");
    println!("full: primary after startup {token:?}");
    assert_eq!(token.integrity, "S-1-16-0", "the worker lowered itself");

    let script = r#"
        const now = Date.now();
        const total = [1, 2, 3, 4].map((n) => n * n).reduce((a, b) => a + b, 0);
        const text = JSON.stringify({ nested: [total, "x"] });
        return { now, total, text, parsed: JSON.parse(text).nested[1] };
    "#;
    let request = ActivationRequest {
        activation_id: 7,
        profile: Profile::Flow,
        tools: vec![],
        prelude_hash: welcome.prelude_hash,
        script: script.into(),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        budgets: Budgets::default(),
        prefix: vec![],
    };
    write_parent_message(&mut stdin, &ParentMessage::Activate(Box::new(request)))
        .expect("send the activation");
    let mut host_calls = Vec::new();
    let result = loop {
        match read_worker_message(&mut stdout) {
            Ok(WorkerMessage::HostCall(call)) => {
                assert_eq!(call.kind, CallKind::Primitive(Primitive::Now), "{call:?}");
                let reply = ParentMessage::Deliver(Outcome {
                    position: call.position,
                    settlement: Settlement::Fulfilled,
                    value: JsonText::new(NOW_MS).expect("small"),
                    delivery_order: host_calls.len() as u64,
                });
                host_calls.push(call);
                write_parent_message(&mut stdin, &reply).expect("deliver the clock read");
            }
            Ok(WorkerMessage::Finished {
                activation_id,
                result,
            }) => {
                assert_eq!(activation_id, 7);
                break result;
            }
            other => fail(child, format!("unexpected reply: {other:?}")),
        }
    };
    println!("full: {} host call(s), result {result:?}", host_calls.len());
    assert_eq!(host_calls.len(), 1);
    let value = match result {
        ActivationResult::Completed { value } => value,
        other => fail(child, format!("the activation did not complete: {other:?}")),
    };
    let value: serde_json::Value = serde_json::from_str(value.as_str()).expect("JSON result");
    assert_eq!(
        value,
        serde_json::json!({
            "now": 1767225600000u64,
            "total": 30,
            "text": "{\"nested\":[30,\"x\"]}",
            "parsed": "x",
        })
    );

    write_parent_message(&mut stdin, &ParentMessage::Shutdown).expect("send shutdown");
    drop(stdin);
    drop(stdout);
    let ended = end(child);
    println!("full: shut down with exit {}", ended.code);
    assert_eq!(ended.code, 0, "{}", ended.stderr);
}

// Worker checks of token, mitigation and handle properties the launcher can
// break when it creates the process. In these test variants the parent's own
// pre-resume checks expect the broken property, so the worker is resumed and
// must notice and refuse by itself.

#[test]
fn a_primary_without_the_lpac_claim_is_refused_as_not_lpac() {
    assert_refused(Deviation::NotLpac);
}

#[test]
fn a_primary_with_a_capability_is_refused_as_capabilities_present() {
    assert_refused(Deviation::CapabilitiesPresent);
}

#[test]
fn a_second_restricting_sid_is_refused_as_restricting_sid_mismatch() {
    assert_refused(Deviation::RestrictingSidMismatch);
}

#[test]
fn an_enabled_logon_group_is_refused_as_group_not_deny_only() {
    assert_refused(Deviation::GroupNotDenyOnly);
}

#[test]
fn a_primary_with_privileges_is_refused_as_privileges_present() {
    assert_refused(Deviation::PrivilegesPresent);
}

#[test]
fn allowed_dynamic_code_is_refused_as_mitigation_mismatch() {
    assert_refused(Deviation::MitigationMismatch);
}

/// Without the explicit inherited-handle list the worker inherits every
/// inheritable handle of this process. One is planted here, so the worker's
/// table holds an inheritable file that is not one of its pipes.
#[test]
fn an_inherited_handle_beyond_the_pipes_is_refused_as_handle_not_allowed() {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    let fixture = worker()
        .parent()
        .expect("the worker has a directory")
        .join(format!("basal-worker-planted-{}", std::process::id()));
    let planted = File::create(&fixture).expect("create the planted file");
    let ok = unsafe {
        SetHandleInformation(
            planted.as_raw_handle(),
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    };
    assert_ne!(
        ok,
        0,
        "SetHandleInformation: {}",
        std::io::Error::last_os_error()
    );
    assert_refused(Deviation::HandleNotAllowed);
    drop(planted);
    let _ = std::fs::remove_file(&fixture);
}

/// A production worker has no way to skip a startup check: it refuses the
/// request the launcher's test variants add as an unknown argument, exit 64,
/// before any check runs.
#[cfg(not(feature = "deviations"))]
#[test]
fn a_production_worker_refuses_the_deviation_argument() {
    for deviation in [
        Deviation::ThreadTokenPresent,
        Deviation::IntegrityLowerFailed,
        Deviation::IntegrityNotUntrusted,
    ] {
        let argument = deviation.child_argument().expect("a worker argument");
        let ended = end(start(deviation));
        println!(
            "{deviation}: exit {} stderr {}",
            ended.code,
            ended.stderr.trim()
        );
        assert_eq!(ended.code, 64, "{deviation}: {}", ended.stderr);
        assert!(
            ended
                .stderr
                .contains(&format!("unknown argument {argument}")),
            "{}",
            ended.stderr
        );
    }
}

// Worker checks of steps the worker performs itself (dropping the start-up
// thread token, lowering its integrity), which the parent cannot break when
// it creates the process. A worker built with the `deviations` feature skips
// or breaks that step when the launcher's command line asks it to.

#[cfg(feature = "deviations")]
#[test]
fn a_kept_start_up_token_is_refused_as_thread_token_present() {
    assert_refused(Deviation::ThreadTokenPresent);
}

#[cfg(feature = "deviations")]
#[test]
fn a_failed_lowering_is_refused_as_integrity_lower_failed() {
    assert_refused(Deviation::IntegrityLowerFailed);
}

#[cfg(feature = "deviations")]
#[test]
fn a_skipped_lowering_is_refused_as_integrity_not_untrusted() {
    assert_refused(Deviation::IntegrityNotUntrusted);
}

/// Started without a package SID, the worker refuses before confining
/// itself, as it does without its Landlock argument on Linux.
#[test]
fn a_missing_package_sid_exits_70_package_sid_argument_missing() {
    let output = Command::new(worker())
        .stdin(Stdio::null())
        .output()
        .expect("run the worker");
    let ended = Ended {
        code: output.status.code().expect("an exit code") as u32,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    println!(
        "missing: exit {} stderr {}",
        ended.code,
        ended.stderr.trim()
    );
    assert_eq!(ended.code, 70, "{}", ended.stderr);
    assert_eq!(
        ended.reason(),
        Some("package-sid-argument-missing"),
        "{}",
        ended.stderr
    );
}

/// A package SID the system's SID parser rejects is a usage error.
#[test]
fn an_unparsable_package_sid_exits_64() {
    let output = Command::new(worker())
        .arg("--package-sid=S-1-15-2-not-a-sid")
        .stdin(Stdio::null())
        .output()
        .expect("run the worker");
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("unparsable: {:?} stderr {}", output.status, stderr.trim());
    assert_eq!(output.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains("unparsable --package-sid"), "{stderr}");
}
