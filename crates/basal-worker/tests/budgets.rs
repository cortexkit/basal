//! Per-activation limits: JS time, memory, stack, and stalled promises.

mod common;

use std::time::Duration;

use basal_proto::{ActivationResult, BudgetKind};

#[test]
fn cpu_bound_loop_is_stopped_by_the_js_time_budget() {
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 100_000;
    // Generous: the budget is CPU time, and a loaded machine stretches it.
    parent.deadline = Duration::from_secs(15);
    for script in [
        "for (;;) {}",
        // Catching does not help: the interrupt cannot be caught.
        "try { for (;;) {} } catch (e) {} return 'escaped';",
        "await ops.call('mock', 'echo', 1); for (;;) {}",
    ] {
        let report = parent.run("t", script);
        assert_eq!(
            common::finished(&report),
            &ActivationResult::BudgetExhausted(BudgetKind::JsTime),
            "{script}"
        );
        parent.journals.clear();
    }
}

#[test]
fn long_host_waits_do_not_consume_the_js_time_budget() {
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 50_000;
    parent.host.sync_sleep = Duration::from_millis(300);
    let script = r#"
        await ops.call('mock', 'sleep', { ms: 300 });
        const t = Date.now();
        await ops.call('mock', 'sleep', { ms: 300 });
        return typeof t;
    "#;
    let report = parent.run("t", script);
    assert_eq!(
        common::finished(&report),
        &ActivationResult::Completed {
            value: basal_proto::JsonText::new("\"number\"").expect("small")
        }
    );
    // The waits really happened: twelve times the budget passed in them.
    assert!(
        report.wall >= Duration::from_millis(600),
        "{:?}",
        report.wall
    );
}

#[test]
fn memory_limit_holds() {
    let mut parent = common::parent();
    parent.budgets.memory_bytes = 2 * 1024 * 1024;
    let report = parent.run("t", "return 'x'.repeat(32 * 1024 * 1024).length;");
    assert_eq!(
        common::finished(&report),
        &ActivationResult::BudgetExhausted(BudgetKind::Memory)
    );
    // Catching the failure does not get the allocation through.
    let report = parent.run(
        "u",
        "try { return 'x'.repeat(32 * 1024 * 1024).length; } catch (e) { return e.message; }",
    );
    assert_eq!(report.value(), serde_json::json!("out of memory"));
}

#[test]
fn stack_limit_holds() {
    let mut parent = common::parent();
    parent.budgets.stack_bytes = 32 * 1024;
    // Fifty levels fit easily in the engine's default stack, so this fails
    // only because of the configured limit.
    let report = parent.run(
        "t",
        "function f(n) { return n ? 1 + f(n - 1) : 0; } return f(50);",
    );
    assert_eq!(
        common::finished(&report),
        &ActivationResult::BudgetExhausted(BudgetKind::Stack)
    );
}

#[test]
fn stalled_promise_is_reported_as_stalled() {
    let mut parent = common::parent();
    for script in [
        "await new Promise(() => {});",
        "await ops.call('mock', 'echo', 1); await new Promise(() => {});",
        "const never = new Promise(() => {}); await Promise.all([never, ops.call('mock', 'echo', 1)]);",
    ] {
        let report = parent.run("t", script);
        assert_eq!(
            common::finished(&report),
            &ActivationResult::Stalled,
            "{script}"
        );
        parent.journals.clear();
    }
}
