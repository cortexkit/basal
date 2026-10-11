//! Codemode's confined wire contract, without the flow parent's journal or hosts.

mod common;

use std::time::Duration;

use basal_proto::{
    ActivationRequest, ActivationResult, BudgetKind, Budgets, CallKind, Failure, HostCall,
    JsonText, MAX_VALUE_BYTES, Outcome, ParentMessage, PreludeHash, Profile, Settlement,
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
