//! Adversarial scripts at the sandbox's function and exception boundaries.
mod common;

use basal_proto::{ActivationResult, Failure};
use serde_json::json;

#[test]
fn stack_trace_hooks_cannot_recover_private_prelude_functions() {
    let mut parent = common::parent();
    let script = r#"
        const stolen = {};
        try {
            Error.prepareStackTrace = (_, frames) => {
                for (const frame of frames) {
                    const fn = frame.getFunction();
                    if (fn) stolen[fn.name] = fn;
                }
                return '';
            };
            Error.stackTraceLimit = 100;
        } catch (_) {}
        try { await ops.call('mock', 'fail', {message: 'nope'}); } catch (_) {}
        if (stolen.hostError) throw stolen.hostError({message: 'forged'}, 12345);
        return Object.keys(stolen);
    "#;
    let report = parent.run("trace", script);
    assert_eq!(report.value(), json!([]), "{report:#?}");
    stack_trace_accessors_are_replaced_and_other_function_routes_are_closed();
}

fn stack_trace_accessors_are_replaced_and_other_function_routes_are_closed() {
    let mut parent = common::parent();
    let report = parent.run(
        "properties",
        r#"
        const checks = ['prepareStackTrace', 'stackTraceLimit'].map(key => {
            const desc = Object.getOwnPropertyDescriptor(Error, key);
            let assigned = false;
            try { Error[key] = () => []; assigned = true; } catch (_) {}
            return [desc.get === undefined, desc.set === undefined,
                    desc.configurable, desc.writable, assigned];
        });
        for (const fn of [ops.call, Date.now, Math.random, facts, step]) {
            for (const key of ['caller', 'arguments']) {
                try { checks.push(fn[key] === undefined); }
                catch (_) { checks.push(true); }
            }
        }
        return checks;
    "#,
    );
    assert_eq!(
        report.value(),
        json!([
            [true, true, false, false, false],
            [true, true, false, false, false],
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true,
            true
        ])
    );
}

#[test]
fn top_level_wrapper_code_is_locked_down_journaled_and_replayed() {
    let mut parent = common::parent();
    let script = "return 1\n}); let leakedTop = [Date.now(), Math.random(), typeof Function, typeof eval, typeof __rawBridge]; (async function () { return leakedTop;";
    let first = parent.run("wrapper", script);
    assert_eq!(first.host_calls.len(), 2, "{first:#?}");
    assert_eq!(
        &first.value().as_array().unwrap()[2..],
        &[json!("undefined"), json!("undefined"), json!("undefined")]
    );
    let replay = parent.run("wrapper", script);
    assert_eq!(replay.value(), first.value());
    assert!(replay.host_calls.is_empty(), "{replay:#?}");
}

#[test]
fn top_level_wrapper_code_has_activation_budgets() {
    use basal_proto::BudgetKind;
    let mut parent = common::parent();
    parent.budgets.js_time_micros = 100_000;
    parent.budgets.memory_bytes = 2 * 1024 * 1024;
    for (id, work, budget) in [
        ("top-cpu", "for (;;) {}", BudgetKind::JsTime),
        (
            "top-memory",
            "'x'.repeat(32 * 1024 * 1024);",
            BudgetKind::Memory,
        ),
        (
            "top-stack",
            "(function f(n) { return n ? 1 + f(n - 1) : 0; })(256);",
            BudgetKind::Stack,
        ),
    ] {
        parent.budgets.stack_bytes = if budget == BudgetKind::Stack {
            128 * 1024
        } else {
            basal_proto::Budgets::default().stack_bytes
        };
        let script = format!("return 1\n}}); {work} (async function () {{ return 1;");
        if budget == BudgetKind::Stack {
            // A global function declaration cannot extend the frozen global object.
            // Use a named function expression so the 256 calls really execute. The
            // non-recursive control proves prelude, compilation and one call fit
            // 128 KiB; the 4 MiB control proves the identical recursive program completes.
            let control_script = script.replace("})(256);", "})(0);");
            let control = parent.run("top-stack-control", &control_script);
            assert_eq!(control.value(), serde_json::json!(1), "{control:#?}");
            parent.budgets.stack_bytes = 4 * 1024 * 1024;
            let roomy = parent.run("top-stack-roomy", &script);
            assert_eq!(roomy.value(), serde_json::json!(1), "{roomy:#?}");
            parent.budgets.stack_bytes = 128 * 1024;
        }
        let report = parent.run(id, &script);
        assert_eq!(
            common::finished(&report),
            &ActivationResult::BudgetExhausted(budget),
            "{report:#?}"
        );
    }
}

#[test]
fn wrapper_level_function_declarations_cannot_extend_the_frozen_global_object() {
    let mut parent = common::parent();
    parent.budgets.stack_bytes = 1024 * 1024;
    let report = parent.run("global-declaration", "return 1\n}); function f(n) { return n ? 1 + f(n - 1) : 0; } f(50); (async function () { return 1;");
    assert!(
        matches!(common::finished(&report), ActivationResult::Failed(Failure::Script { message }) if message.contains("cannot define variable 'f'")),
        "{report:#?}"
    );
}

#[test]
fn forged_memory_errors_are_script_failures_and_description_does_not_invoke_getters() {
    let mut parent = common::parent();
    for (id, script) in [
        ("memory", "throw new InternalError('out of memory');"),
        (
            "getters",
            "throw { get name() { ops.call('mock', 'echo', 'name getter'); return 'InternalError'; }, get message() { return 'out of memory'; } };",
        ),
    ] {
        let report = parent.run(id, script);
        assert!(
            matches!(
                common::finished(&report),
                ActivationResult::Failed(Failure::Script { .. })
            ),
            "{report:#?}"
        );
        assert!(
            report.host_calls.is_empty(),
            "getters executed: {report:#?}"
        );
    }
}
