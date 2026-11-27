//! Untrusted payloads through JSON, strings and regular expressions, under
//! the JS time budget. Every activation must end with a typed result and the
//! worker must survive all of them.

mod common;

use std::time::Duration;

use basal_proto::{ActivationResult, BudgetKind};
use basal_testkit::{Ending, ParentError, fuzz};
use serde_json::json;

#[test]
fn catastrophic_regex_is_stopped_by_the_js_time_budget() {
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 200_000;
    parent.deadline = Duration::from_secs(15);
    let report = parent.run("t", "return /(a+)+$/.test('a'.repeat(40) + '!');");
    assert_eq!(
        common::finished(&report),
        &ActivationResult::BudgetExhausted(BudgetKind::JsTime)
    );
}

#[test]
fn deeply_nested_payload_fails_typed() {
    let mut parent = common::parent();
    let raw = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    parent.trigger = basal_proto::JsonText::new(json!({ "raw": raw }).to_string()).expect("fits");
    let report = parent.run("t", fuzz::SCRIPT);
    assert!(
        matches!(
            common::finished(&report),
            ActivationResult::Failed(basal_proto::Failure::InvalidHostValue { .. })
                | ActivationResult::BudgetExhausted(_)
        ),
        "{report:?}"
    );
}

#[test]
fn fuzzed_payloads_end_typed_and_the_worker_survives() {
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 200_000;
    parent.deadline = Duration::from_secs(15);
    let tally = fuzz::run(&mut parent, 0x5EED, 150);
    assert!(tally.unexpected.is_empty(), "{:#?}", tally.unexpected);
    assert!(tally.completed > 50, "{tally:?}");
    assert!(tally.invalid_value > 0, "{tally:?}");
    assert!(
        tally.js_time >= 3,
        "the evil regexes were not stopped: {tally:?}"
    );
    // One worker served every case.
    let pid = parent.worker().expect("worker").pid();
    parent.run("after", "return 1").value();
    assert_eq!(parent.worker().expect("worker").pid(), pid);
}

/// `/(a|a?)+$/u` backtracks through loop iterations whose body matched
/// nothing. On Windows arm64 it once killed the confined worker with an
/// access violation before its time budget ran out. It must end typed, and
/// the worker must survive it.
#[test]
fn an_empty_matching_loop_regex_ends_typed_and_the_worker_survives() {
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 200_000;
    parent.deadline = Duration::from_secs(15);
    let pid = parent.worker().expect("worker").pid();
    let report = parent.run("t", "return /(a|a?)+$/u.test('a'.repeat(40) + '!');");
    assert_eq!(
        report.result(),
        Some(&ActivationResult::BudgetExhausted(BudgetKind::JsTime)),
        "{report:?}"
    );
    parent.run("after", "return 1").value();
    assert_eq!(parent.worker().expect("worker").pid(), pid);
}

/// TEMPORARY bisection of the Windows arm64 crash in
/// `fuzzed_payloads_end_typed_and_the_worker_survives`. There, `regex-2`
/// (`/(a|a?)+$/u` on forty `a`s and `!`) killed the confined worker with an
/// access violation as its 153rd activation, at 73, 93 and 249 ms in three
/// runs, while each half of the preceding sequence followed by `regex-2`
/// survived. Each plan below runs the fuzz test's whole sequence (150
/// payloads, then every regex case) in its own fresh worker: five times
/// with the fuzz test's 200 ms JS time budget, to see how often it crashes,
/// and twice with a 5 s budget, to see whether the crash follows the
/// budget's interrupt or the amount of regex work. Every regex case's
/// ending and wall time is listed. The test fails, listing everything, if
/// any activation did not end with a typed result from the worker. Remove
/// it once the crash is fixed.
#[test]
fn regex_crash_bisection() {
    let plans: Vec<(String, u64, u64)> = (1..=5)
        .map(|run| {
            (
                format!("full sequence, 200 ms budget, run {run}"),
                200_000,
                15,
            )
        })
        .chain((1..=2).map(|run| {
            (
                format!("full sequence, 5 s budget, run {run}"),
                5_000_000,
                60,
            )
        }))
        .collect();
    let regexes = fuzz::regex_cases();
    let mut lines = Vec::new();
    let mut failed = false;
    for (plan, js_time_micros, deadline) in plans {
        let mut parent = common::parent();
        parent.budgets.js_time_micros = js_time_micros;
        parent.deadline = Duration::from_secs(deadline);
        let mut tally = fuzz::Tally::default();
        for index in 0..150 {
            let trigger = json!({ "raw": fuzz::payload(0x5EED, index) });
            fuzz::run_case(
                &mut parent,
                &mut tally,
                &format!("payload-{index}"),
                trigger,
            );
        }
        let mut endings = Vec::new();
        for (index, (pattern, subject)) in regexes.iter().enumerate() {
            let trigger = json!({ "raw": "\"x\"", "pattern": pattern, "subject": subject });
            parent.trigger = basal_proto::JsonText::new(trigger.to_string()).expect("small");
            let report = parent.run(&format!("regex-{index}"), fuzz::SCRIPT);
            let typed = matches!(report.ending, Ending::Finished(_));
            failed |= !typed;
            let mut line = format!(
                "  regex-{index}: {:?} after {:?}",
                report.ending, report.wall
            );
            if let Some(exit) = &report.worker_exit {
                line.push_str(&format!("; {exit}"));
            }
            endings.push(line);
        }
        failed |= !tally.unexpected.is_empty();
        lines.push(format!(
            "{plan}: unexpected payload endings {:#?}\n{}",
            tally.unexpected,
            endings.join("\n")
        ));
    }
    let report = lines.join("\n");
    println!("{report}");
    assert!(!failed, "{report}");
}

/// A worker that dies during an activation is reported with its exit status,
/// so a fuzz case that kills the worker names how it died (on Windows, an
/// exception code) instead of only the closed channel.
#[test]
fn a_worker_that_dies_mid_activation_is_reported_with_its_exit_status() {
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 30_000_000;
    parent.deadline = Duration::from_secs(60);
    let pid = parent.worker().expect("worker").pid();
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        kill(pid);
    });
    let report = parent.run("t", "while (true) {}");
    killer.join().expect("killer thread");
    assert_eq!(
        report.ending,
        Ending::Broken(ParentError::Closed),
        "{report:?}"
    );
    let exit = report.worker_exit.as_deref().expect("an exit report");
    #[cfg(unix)]
    assert!(exit.starts_with("worker signal: 9"), "{exit}");
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(basal_launch::KILL_EXIT_CODE);
        assert!(exit.starts_with(&format!("worker {status};")), "{exit}");
    }
}

fn kill(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: kill only sends a signal to the worker this test spawned.
        let killed = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        assert_eq!(killed, 0, "{}", std::io::Error::last_os_error());
    }
    #[cfg(windows)]
    basal_testkit::process::terminate_process(pid).expect("terminate the worker");
}
