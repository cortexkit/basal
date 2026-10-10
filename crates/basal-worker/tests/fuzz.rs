//! Untrusted payloads through JSON, strings and regular expressions, under
//! the JS time budget. Every activation must end with a typed result and the
//! worker must survive all of them.

mod common;

use std::time::Duration;

use basal_proto::{ActivationResult, BudgetKind};
use basal_testkit::fuzz;
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
