//! Codemode's confined wire contract, without the flow parent's journal or hosts.

mod common;

use std::time::Duration;

use basal_proto::{
    ActivationRequest, ActivationResult, BudgetKind, Budgets, CallKind, Failure, HostCall,
    JsonText, MAX_VALUE_BYTES, Outcome, ParentMessage, PreludeHash, Primitive, Profile, Settlement,
    WorkerMessage,
};
use basal_testkit::WorkerProcess;
use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);

fn request(script: &str) -> ActivationRequest {
    ActivationRequest {
        activation_id: 1,
        profile: Profile::Codemode,
        tools: vec!["echo".into()],
        prelude_hash: basal_worker::engine::codemode_prelude_hash(),
        script: script.into(),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        params_input: JsonText::null(),
        budgets: Budgets {
            js_time_micros: 10_000_000,
            memory_bytes: 64 * 1024 * 1024,
            stack_bytes: 1024 * 1024,
            max_value_bytes: MAX_VALUE_BYTES as u32,
        },
        prefix: vec![],
    }
}

#[derive(Debug)]
struct Report {
    result: ActivationResult,
    calls: Vec<HostCall>,
    lines: Vec<String>,
}

fn run(req: ActivationRequest) -> Report {
    let mut worker = WorkerProcess::spawn(&common::worker_binary()).expect("spawn");
    let welcome = worker.handshake(WAIT).expect("handshake");
    assert_eq!(welcome.prelude_hash, basal_worker::engine::prelude_hash());
    assert_eq!(
        welcome.codemode_prelude_hash,
        basal_worker::engine::codemode_prelude_hash()
    );
    worker
        .send(&ParentMessage::Activate(Box::new(req)))
        .expect("activate");
    let mut calls = Vec::new();
    let mut lines = Vec::new();
    loop {
        match worker.recv(WAIT).expect("worker message") {
            WorkerMessage::Console { line } => lines.push(line),
            WorkerMessage::HostCall(call) => calls.push(call),
            WorkerMessage::Blocked { awaiting } => {
                let position = awaiting[0];
                let call = calls
                    .iter()
                    .find(|c| c.position == position)
                    .expect("pending call");
                worker
                    .send(&ParentMessage::Deliver(Outcome {
                        position,
                        settlement: Settlement::Fulfilled,
                        value: call.args.clone(),
                        delivery_order: position + 1,
                    }))
                    .expect("deliver");
            }
            WorkerMessage::Finished {
                activation_id,
                result,
            } => {
                assert_eq!(activation_id, 1);
                return Report {
                    result,
                    calls,
                    lines,
                };
            }
            other => panic!("unexpected worker message: {other:?}"),
        }
    }
}

fn value(report: &Report) -> Value {
    match &report.result {
        ActivationResult::Completed { value } => serde_json::from_str(value.as_str()).unwrap(),
        other => panic!("expected completion: {other:?}"),
    }
}

#[test]
fn codemode_exposes_only_tools_console_and_frozen_intrinsics() {
    let report = run(request(
        r#"
        const forbidden = ['ops','sh','sink','kv','facts','llm','classify','fs','git','net','step',
            'self','trigger','now','random','eval','Function','Atomics','SharedArrayBuffer','Intl',
            'setTimeout','queueMicrotask','print','os','std'];
        const roots = [globalThis, tools, tools.echo, console, console.log, Object, Object.prototype,
            Date, Date.prototype, Math, Promise, Promise.prototype,
            Object.getPrototypeOf(async function() {}), Object.getPrototypeOf(function*() {})];
        return [Object.getPrototypeOf(tools) === null, Object.keys(tools), Object.keys(console),
            roots.every(Object.isFrozen), forbidden.every(k => typeof globalThis[k] === 'undefined'),
            (function() {}).constructor === undefined, Error.prepareStackTrace === undefined,
            Error.stackTraceLimit === undefined];
    "#,
    ));
    assert_eq!(
        value(&report),
        json!([true, ["echo"], ["log"], true, true, true, true, true])
    );
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_unknown_tool_access_names_the_tool_and_lists_the_catalog() {
    let mut req = request(
        r#"
        try { await tools.work({}); return 'reached'; }
        catch (e) { return [e instanceof TypeError, e.message]; }
    "#,
    );
    req.tools = vec!["echo".into(), "bash".into()];
    let report = run(req);
    assert_eq!(
        value(&report),
        json!([
            true,
            "tools.work is not available in this codemode run. Available tools: bash, echo (2). Not every tool is available inside programs yet."
        ])
    );
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_uncaught_unknown_tool_is_a_script_error_naming_it() {
    let report = run(request("return await tools.ctx_expand({});"));
    match &report.result {
        ActivationResult::Failed(Failure::Script { message }) => {
            // Codemode hides stack traces, so the message is the whole text.
            assert_eq!(
                message,
                "TypeError: tools.ctx_expand is not available in this codemode run. Available tools: echo (1). Not every tool is available inside programs yet."
            );
        }
        other => panic!("expected a script failure: {other:?}"),
    }
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_tools_keeps_keys_in_typeof_await_and_json_working() {
    let report = run(request(
        r#"
        const awaited = await tools;
        return [Object.keys(tools), 'echo' in tools, 'work' in tools, typeof tools.echo,
            awaited === tools, JSON.stringify(tools), JSON.stringify({tools}), tools.then, tools.toJSON,
            tools[Symbol.iterator], tools[Symbol.toPrimitive], tools[Symbol.toStringTag],
            Object.prototype.toString.call(tools), Object.getOwnPropertyNames(tools),
            Object.getOwnPropertySymbols(tools).length, (await tools.echo(3))];
    "#,
    ));
    assert_eq!(
        value(&report),
        json!([
            ["echo"],
            true,
            false,
            "function",
            true,
            "{}",
            "{\"tools\":{}}",
            null,
            null,
            null,
            null,
            null,
            "[object Object]",
            ["echo"],
            0,
            3
        ])
    );
    assert_eq!(report.calls.len(), 1);
}

#[test]
fn codemode_tools_entries_cannot_be_replaced_or_added() {
    let report = run(request(
        r#"
        const echo = tools.echo;
        const refused = [
            () => { tools.echo = 1; },
            () => { tools.work = 1; },
            () => { delete tools.echo; },
            () => Object.defineProperty(tools, 'echo', { value: 1 }),
            () => Object.defineProperty(tools, 'work', { value: 1 }),
            () => Object.setPrototypeOf(tools, {}),
        ].map(f => { try { f(); return false; } catch (e) { return e instanceof TypeError; } });
        return [refused, tools.echo === echo, Object.isFrozen(tools),
            Object.getPrototypeOf(tools) === null, Object.keys(tools)];
    "#,
    ));
    assert_eq!(
        value(&report),
        json!([
            [true, true, true, true, true, true],
            true,
            true,
            true,
            ["echo"]
        ])
    );
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_clock_and_random_are_worker_native() {
    let report = run(request(
        r#"
        const before = Date.now();
        const instance = new Date();
        const text = Date();
        const randoms = Array.from({length: 8}, () => Math.random());
        return [before > 1_600_000_000_000, instance.getTime() >= before,
            Date.parse(text) >= before, Date.now() >= instance.getTime(),
            randoms.every(x => x >= 0 && x < 1), new Set(randoms).size > 1];
    "#,
    ));
    assert_eq!(value(&report), json!([true, true, true, true, true, true]));
    assert!(
        report.calls.is_empty(),
        "native clock/random reached the parent: {:?}",
        report.calls
    );
}

#[test]
fn codemode_dates_are_utc_only() {
    let report = run(request(
        r#"
        let refused = 0;
        for (const text of ['2026-01-02T12:00:00', 'January 2, 2026', '2026/01/02']) {
            try { new Date(text); } catch (e) { if (e instanceof RangeError) refused++; }
        }
        return [refused, Number.isNaN(Date.parse('2026-01-02T12:00:00')),
            new Date(2026, 0, 2, 12).toISOString(), new Date('2026-01-02').toISOString(),
            new Date('2026-01-02T12:00:00+02:00').toISOString(),
            typeof new Date().getHours, Date.prototype.constructor === Date];
    "#,
    ));
    assert_eq!(
        value(&report),
        json!([
            3,
            true,
            "2026-01-02T12:00:00.000Z",
            "2026-01-02T00:00:00.000Z",
            "2026-01-02T10:00:00.000Z",
            "undefined",
            true
        ])
    );
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_console_formats_and_sends_lines_without_host_calls() {
    let report = run(request(
        r#"
        console.log('a', 1);
        const circular = {}; circular.self = circular;
        console.log({x:1}, undefined, 1n, circular);
        console.log();
        return 4;
    "#,
    ));
    assert_eq!(value(&report), json!(4));
    assert_eq!(
        report.lines,
        ["a 1\n", "{\"x\":1} undefined 1 [object Object]\n", "\n"]
    );
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_tool_input_crosses_as_json_and_unawaited_calls_drain() {
    let report = run(request("tools.echo({a:1}); tools.echo({b:2}); return 4;"));
    assert_eq!(value(&report), json!(4));
    assert_eq!(report.calls.len(), 2);
    for call in &report.calls {
        assert_eq!(
            call.kind,
            CallKind::Tool {
                name: "echo".into()
            }
        );
    }
    assert_eq!(report.calls[0].args.as_str(), "{\"a\":1}");
    assert_eq!(report.calls[1].args.as_str(), "{\"b\":2}");
}

#[test]
fn codemode_rejects_the_wrong_profile_prelude_hash() {
    let mut req = request("return 4");
    let actual = req.prelude_hash;
    req.prelude_hash = basal_worker::engine::prelude_hash();
    let expected = req.prelude_hash;
    assert_ne!(actual, expected);
    let report = run(req);
    assert_eq!(
        report.result,
        ActivationResult::Failed(Failure::EngineMismatch { actual, expected })
    );
    assert!(report.calls.is_empty());
}

#[test]
fn codemode_prelude_hash_is_independent_of_flow_source() {
    let flow = PreludeHash::of(include_str!("../src/prelude.js"));
    let codemode = include_str!("../src/codemode_prelude.js");
    assert_eq!(basal_worker::engine::prelude_hash(), flow);
    assert_eq!(
        basal_worker::engine::codemode_prelude_hash(),
        PreludeHash::of(codemode)
    );
    let edited = format!("{codemode}\n// source fingerprint control\n");
    assert_ne!(
        PreludeHash::of(&edited),
        basal_worker::engine::codemode_prelude_hash()
    );
    assert_eq!(basal_worker::engine::prelude_hash(), flow);
}

#[test]
fn codemode_caught_memory_exhaustion_is_sticky() {
    let report = run(request(
        "try { new ArrayBuffer(80 * 1024 * 1024); } catch (_) {} await tools.echo(4); return 4;",
    ));
    assert_eq!(
        report.result,
        ActivationResult::BudgetExhausted(BudgetKind::Memory)
    );
    assert!(report.calls.is_empty());
    let control = run(request("return new ArrayBuffer(1024 * 1024).byteLength;"));
    assert_eq!(value(&control), json!(1024 * 1024));
}

#[test]
fn codemode_caught_stack_exhaustion_is_sticky() {
    let report = run(request(
        "function f() { return f(); } try { f(); } catch (_) {} await tools.echo(4); return 4;",
    ));
    assert_eq!(
        report.result,
        ActivationResult::BudgetExhausted(BudgetKind::Stack)
    );
    assert!(report.calls.is_empty());
    let control = run(request(
        "function f(n) { return n ? f(n-1) + 1 : 0; } return f(10);",
    ));
    assert_eq!(value(&control), json!(10));
}

#[test]
fn codemode_tool_arguments_cap_is_inclusive_and_cannot_be_caught() {
    // A JSON string adds exactly two quotes to its ASCII payload.
    let exact = run(request(&format!(
        "return (await tools.echo('x'.repeat({}))).length;",
        MAX_VALUE_BYTES - 2
    )));
    assert_eq!(value(&exact), json!(MAX_VALUE_BYTES - 2));
    assert_eq!(exact.calls.len(), 1);
    assert_eq!(exact.calls[0].args.len(), MAX_VALUE_BYTES);
    let exceeded = run(request(&format!(
        "try {{ await tools.echo('x'.repeat({})); }} catch (_) {{}} return 4;",
        MAX_VALUE_BYTES - 1
    )));
    assert_eq!(
        exceeded.result,
        ActivationResult::Failed(Failure::ArgumentsTooLarge {
            bytes: (MAX_VALUE_BYTES + 1) as u64,
            cap: MAX_VALUE_BYTES as u64,
        })
    );
    assert!(
        exceeded.calls.is_empty(),
        "oversized input reached the parent"
    );
}

#[test]
fn codemode_js_cpu_budget_stops_loops() {
    let mut req = request("while (true) {}");
    req.budgets.js_time_micros = 10_000;
    let report = run(req);
    assert_eq!(
        report.result,
        ActivationResult::BudgetExhausted(BudgetKind::JsTime)
    );
    assert!(report.calls.is_empty());
    let control = run(request("let n=0; for (let i=0;i<1000;i++) n++; return n;"));
    assert_eq!(value(&control), json!(1000));
}

/// The sleeps a program asked the parent for, as their requested milliseconds.
fn sleeps(report: &Report) -> Vec<u64> {
    report
        .calls
        .iter()
        .map(|call| {
            assert_eq!(call.kind, CallKind::Primitive(Primitive::Sleep));
            call.args.as_str().parse().expect("sleep takes an integer")
        })
        .collect()
}

#[test]
fn codemode_sleep_takes_0_to_10000_ms_and_refuses_anything_else() {
    let report = run(request(
        r#"
        const slept = [];
        for (const ms of [0, 1, 10000]) slept.push(await sleep(ms));
        const refused = [];
        for (const ms of [-1, 10001, 1.5, NaN, Infinity, '5', undefined, null]) {
            try { await sleep(ms); refused.push('slept'); }
            catch (e) { refused.push(e instanceof RangeError ? e.message : String(e)); }
        }
        const fixed = ['sleep', 'waitFor'].map(name => {
            const d = Object.getOwnPropertyDescriptor(globalThis, name);
            return typeof d.value === 'function' && !d.writable && !d.configurable;
        });
        return [slept, refused, fixed];
    "#,
    ));
    let message = "sleep(ms) takes an integer from 0 to 10000 milliseconds";
    assert_eq!(
        value(&report),
        json!([[null, null, null], vec![message; 8], [true, true]])
    );
    assert_eq!(sleeps(&report), [0, 1, 10000]);
}

#[test]
fn codemode_wait_for_returns_the_first_truthy_value() {
    let report = run(request(
        r#"
        let n = 0;
        const sync = await waitFor(() => { n++; return n >= 3 ? {n} : 0; });
        let m = 0;
        const later = await waitFor(async () => { m++; return m >= 2 && 'ready'; }, {interval: 300});
        return [sync, later];
    "#,
    ));
    assert_eq!(value(&report), json!([{"n": 3}, "ready"]));
    assert_eq!(sleeps(&report), [500, 500, 300]);
}

#[test]
fn codemode_wait_for_propagates_what_the_condition_throws() {
    let report = run(request(
        r#"
        let n = 0;
        try {
            await waitFor(() => { n++; if (n === 2) { const e = new Error('boom'); e.code = 'mine'; throw e; } return false; });
            return 'returned';
        } catch (e) { return [e.message, e.code, n]; }
    "#,
    ));
    assert_eq!(value(&report), json!(["boom", "mine", 2]));
    assert_eq!(sleeps(&report), [500]);
}

#[test]
fn codemode_wait_for_raises_intervals_below_250_ms_to_250() {
    let report = run(request(
        r#"
        const out = [];
        for (const interval of [1, 0, 249]) {
            try { await waitFor(() => false, {timeout: 1000, interval}); out.push('returned'); }
            catch (e) { out.push([e.code, e.message]); }
        }
        try { await waitFor(() => false, {interval: -1}); } catch (e) { out.push(e instanceof RangeError); }
        return out;
    "#,
    ));
    let gave_up = json!([
        "wait_timeout",
        "waitFor gave up after 1000 ms and 5 attempts"
    ]);
    assert_eq!(
        value(&report),
        json!([gave_up.clone(), gave_up.clone(), gave_up, true])
    );
    assert_eq!(sleeps(&report), [250; 12]);
}

#[test]
fn codemode_wait_for_caps_the_timeout_at_30_seconds_and_names_its_attempts() {
    let report = run(request(
        r#"
        const out = [];
        let called = false;
        try { await waitFor(() => { called = true; }, {timeout: 30001}); }
        catch (e) { out.push([e instanceof RangeError, e.message, called]); }
        for (const options of [undefined, {timeout: 30000, interval: 10000}, {timeout: 30000, interval: 20000}, {timeout: 0}]) {
            try { await waitFor(() => false, options); out.push('returned'); }
            catch (e) { out.push([e.code, e.message]); }
        }
        return out;
    "#,
    ));
    assert_eq!(
        value(&report),
        json!([
            [
                true,
                "waitFor timeout must be an integer from 0 to 30000 milliseconds",
                false
            ],
            [
                "wait_timeout",
                "waitFor gave up after 30000 ms and 61 attempts"
            ],
            [
                "wait_timeout",
                "waitFor gave up after 30000 ms and 4 attempts"
            ],
            [
                "wait_timeout",
                "waitFor gave up after 30000 ms and 3 attempts"
            ],
            ["wait_timeout", "waitFor gave up after 0 ms and 1 attempt"]
        ])
    );
    let mut expected = vec![500; 60];
    // A 20 s interval is slept in sleeps of at most 10 s.
    expected.extend([10000; 3]);
    expected.extend([10000; 3]);
    assert_eq!(sleeps(&report), expected);
}
