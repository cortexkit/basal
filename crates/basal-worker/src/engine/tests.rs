//! In-process tests of the engine, driven by an in-memory host link. These
//! cover what a script cannot reach through the prelude: the native bridge
//! itself.

use std::cell::RefCell;
use std::rc::Rc;

use basal_proto::{
    ActivationRequest, ActivationResult, BudgetKind, Budgets, CallKind, Failure, HostCall,
    JsonText, Outcome, Primitive, Profile, Settlement,
};

use super::{run_activation, run_activation_with_raw_bridge};
use crate::link::{HostLink, LinkError, WaitReply};

/// Answers every call with its own arguments, in issue order.
#[derive(Default)]
struct EchoLink {
    issued: Vec<HostCall>,
    waiting: Vec<HostCall>,
    next_order: u64,
}

impl HostLink for EchoLink {
    fn issue(&mut self, call: &HostCall) -> Result<(), LinkError> {
        self.issued.push(call.clone());
        self.waiting.push(call.clone());
        Ok(())
    }

    fn issue_sync(&mut self, call: &HostCall, _: Option<u64>) -> Result<Outcome, LinkError> {
        self.issued.push(call.clone());
        self.next_order += 1;
        Ok(Outcome {
            position: call.position,
            settlement: Settlement::Fulfilled,
            value: JsonText::new("0.5").map_err(|e| LinkError(e.to_string()))?,
            delivery_order: self.next_order,
        })
    }

    fn wait(&mut self, awaiting: &[u64], _: Option<u64>) -> Result<WaitReply, LinkError> {
        let index = self
            .waiting
            .iter()
            .position(|c| awaiting.contains(&c.position))
            .ok_or_else(|| LinkError("nothing to deliver".into()))?;
        let call = self.waiting.remove(index);
        self.next_order += 1;
        Ok(WaitReply::Deliver(Outcome {
            position: call.position,
            settlement: Settlement::Fulfilled,
            value: call.args,
            delivery_order: self.next_order,
        }))
    }
}

fn request(profile: Profile, script: &str) -> ActivationRequest {
    ActivationRequest {
        activation_id: 1,
        profile,
        prelude_hash: super::profile_prelude_hash(profile),
        tools: vec![],
        script: script.into(),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        params_input: JsonText::null(),
        budgets: Budgets::default(),
        prefix: Vec::new(),
    }
}

fn shell_issued(link: &EchoLink) -> bool {
    link.issued
        .iter()
        .any(|c| c.kind == CallKind::Primitive(Primitive::Sh))
}

#[test]
#[cfg(target_os = "linux")]
fn prelude_stack_exhaustion_is_a_stack_budget() {
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::custom::<super::Intrinsics>(&rt).unwrap();
    rt.set_max_stack_size(8 * 1024);
    ctx.with(|ctx| {
        // No user source is involved: this limit cannot compile the fixed prelude.
        assert!(matches!(
            ctx.eval::<rquickjs::Value, _>(super::PRELUDE),
            Err(rquickjs::Error::Exception)
        ));
        ctx.catch();
    });

    let req = request(
        Profile::Flow,
        "function f(n) { return n ? 1 + f(n - 1) : 0; } return f(50);",
    );
    assert_eq!(
        run_activation(&req, Rc::new(RefCell::new(EchoLink::default()))),
        ActivationResult::Completed {
            value: JsonText::new("50").unwrap()
        }
    );
    // Parser frame sizes vary across builds. Cover the small limits where
    // QuickJS can replace stack overflow with a secondary syntax diagnostic.
    for bytes in (8 * 1024..=16 * 1024).step_by(256) {
        assert_eq!(
            super::run(
                &req,
                Rc::new(RefCell::new(EchoLink::default())),
                false,
                vec![],
                Some(bytes),
            ),
            ActivationResult::BudgetExhausted(BudgetKind::Stack),
            "stack budget: {bytes}"
        );
    }
}

#[test]
fn user_script_compilation_obeys_the_stack_budget() {
    // Construct nested syntax, not recursive execution, to exhaust the parser.
    for (open, close) in [("{a:", "}"), ("({a:", "})"), ("{a(){ return ", "}} ")] {
        let source = format!(
            "const value = {}0{}; return 42;",
            open.repeat(50),
            close.repeat(50)
        );
        for profile in [Profile::Flow, Profile::Codemode] {
            let req = request(profile, &source);
            assert_eq!(
                run_activation(&req, Rc::new(RefCell::new(EchoLink::default()))),
                ActivationResult::Completed {
                    value: JsonText::new("42").unwrap()
                }
            );
            for bytes in (8 * 1024..=16 * 1024).step_by(256) {
                assert_eq!(
                    super::run(
                        &req,
                        Rc::new(RefCell::new(EchoLink::default())),
                        false,
                        vec![],
                        Some(bytes),
                    ),
                    ActivationResult::BudgetExhausted(BudgetKind::Stack),
                    "{profile:?}, nesting {open}, stack budget {bytes}"
                );
            }
        }
    }
}

#[test]
fn genuine_syntax_errors_remain_script_failures_at_small_stack_limits() {
    for profile in [Profile::Flow, Profile::Codemode] {
        let req = request(profile, "return @;");
        for bytes in (8 * 1024..=16 * 1024).step_by(256) {
            let result = super::run(
                &req,
                Rc::new(RefCell::new(EchoLink::default())),
                false,
                vec![],
                Some(bytes),
            );
            assert!(
                matches!(result, ActivationResult::Failed(Failure::Script { .. })),
                "{profile:?}, stack budget {bytes}: {result:?}"
            );
        }
    }
}

#[test]
fn user_thrown_syntax_errors_are_not_parser_stack_exhaustion() {
    for profile in [Profile::Flow, Profile::Codemode] {
        let req = request(
            profile,
            "}); throw new SyntaxError('intentional syntax error'); (async function () { return 42;",
        );
        let result = run_activation(&req, Rc::new(RefCell::new(EchoLink::default())));
        assert!(
            matches!(&result, ActivationResult::Failed(Failure::Script { message }) if message.contains("intentional syntax error")),
            "{profile:?}: {result:?}"
        );
    }
}

#[test]
fn parser_stack_retry_does_not_execute_escaped_top_level_code() {
    let source = format!(
        "}}); throw 'compile-only retry executed'; (async function () {{ const value = {}0{}; return 42;",
        "{a:".repeat(50),
        "}".repeat(50)
    );
    for profile in [Profile::Flow, Profile::Codemode] {
        let req = request(profile, &source);
        assert_eq!(
            run_activation(&req, Rc::new(RefCell::new(EchoLink::default()))),
            ActivationResult::Failed(Failure::Script {
                message: "compile-only retry executed".into()
            })
        );
        for bytes in (8 * 1024..=16 * 1024).step_by(256) {
            assert_eq!(
                super::run(
                    &req,
                    Rc::new(RefCell::new(EchoLink::default())),
                    false,
                    vec![],
                    Some(bytes),
                ),
                ActivationResult::BudgetExhausted(BudgetKind::Stack),
                "{profile:?}, stack budget {bytes}"
            );
        }
    }
}

#[test]
fn parser_retry_restores_the_stack_limit_after_a_genuine_syntax_error() {
    let rt = rquickjs::Runtime::new().unwrap();
    let ctx = rquickjs::Context::custom::<super::Intrinsics>(&rt).unwrap();
    let bytes = super::MIN_STACK_BYTES as usize;
    rt.set_max_stack_size(bytes);
    ctx.with(|ctx| {
        assert!(matches!(
            super::compile_user_source(&ctx, "(async function () { return @; })", bytes),
            Err(rquickjs::Error::Exception)
        ));
        assert!(matches!(
            super::classify_exception(ctx.catch()),
            ActivationResult::Failed(Failure::Script { .. })
        ));
        // Fifty calls fit at MAX_STACK_BYTES, but not at the configured limit.
        assert!(matches!(
            ctx.eval::<rquickjs::Value, _>("(function f(n) { return n ? 1 + f(n - 1) : 0; })(50)"),
            Err(rquickjs::Error::Exception)
        ));
        assert_eq!(
            super::classify_exception(ctx.catch()),
            ActivationResult::BudgetExhausted(BudgetKind::Stack)
        );
    });
}

#[test]
fn wrapper_global_declaration_refusal_does_not_depend_on_os_confinement() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let state = run_activation(
        &request(
            Profile::Flow,
            "return [Object.isExtensible(globalThis), Object.getOwnPropertyDescriptor(globalThis, 'f') === undefined];",
        ),
        link.clone(),
    );
    assert_eq!(
        state,
        ActivationResult::Completed {
            value: JsonText::new("[false,true]").unwrap()
        }
    );
    let mut global = request(
        Profile::Flow,
        "return 1\n}); function f(n) { return n ? 1 + f(n - 1) : 0; } f(50); (async function () { return 1;",
    );
    global.budgets.stack_bytes = 1024 * 1024;
    let result = run_activation(&global, link);
    assert!(
        matches!(result, ActivationResult::Failed(Failure::Script { ref message }) if message.contains("cannot define variable 'f'")),
        "{result:?}"
    );
}

#[test]
fn echo_round_trip() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let result = run_activation(
        &request(
            Profile::Flow,
            "return await ops.call('mock', 'echo', {a: 1})",
        ),
        link.clone(),
    );
    assert_eq!(
        result,
        ActivationResult::Completed {
            value: JsonText::new("{\"a\":1}").expect("small")
        }
    );
}

/// The Rust side refuses `sh` in the flow profile even when a script has
/// reached the native bridge directly, bypassing the prelude's API.
#[test]
fn raw_bridge_cannot_issue_sh_in_flow_profile() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let script = "try { await new Promise(() => __rawBridge.issuePrimitive(11, '{}')); } \
                  catch (e) { return 'caught'; } return 'issued';";
    let result = run_activation_with_raw_bridge(&request(Profile::Flow, script), link.clone());
    assert_eq!(
        result,
        ActivationResult::Failed(Failure::ProfileViolation {
            kind: CallKind::Primitive(Primitive::Sh)
        })
    );
    assert!(!shell_issued(&link.borrow()), "sh reached the host");
}

/// The control: the same raw call in the codemode profile is issued.
#[test]
fn raw_bridge_issues_sh_in_codemode_profile() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let script = "return __rawBridge.issuePrimitive(11, '{\"command\":\"echo\"}');";
    let result = run_activation_with_raw_bridge(&request(Profile::Codemode, script), link.clone());
    // The raw call registered no promise, so the activation cannot take its
    // outcome; what matters here is that the call reached the host.
    assert!(
        !matches!(
            result,
            ActivationResult::Failed(Failure::ProfileViolation { .. })
        ),
        "{result:?}"
    );
    assert!(shell_issued(&link.borrow()));
}

#[test]
fn replayed_sync_delivery_orders_must_increase() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let mut req = request(Profile::Flow, "Date.now(); return Math.random();");
    for (position, primitive, order) in [(0, Primitive::Now, 5), (1, Primitive::Random, 2)] {
        req.prefix.push(basal_proto::RecordedCall {
            position,
            kind: CallKind::Primitive(primitive),
            args_digest: basal_proto::ArgsDigest::of(&JsonText::null()),
            outcome: Some(basal_proto::RecordedOutcome {
                settlement: Settlement::Fulfilled,
                value: JsonText::new("0.5").unwrap(),
                delivery_order: order,
            }),
        });
    }
    let result = run_activation(&req, link.clone());
    assert!(
        matches!(
            result,
            ActivationResult::Failed(Failure::InvalidRequest { .. })
        ),
        "{result:?}"
    );
    assert!(link.borrow().issued.is_empty());
}

#[test]
fn raw_bridge_rejects_empty_op_names_before_sending() {
    for script in [
        "__rawBridge.issueOp('', 'echo', 'null');",
        "__rawBridge.issueOp('mock', '', 'null');",
    ] {
        let link = Rc::new(RefCell::new(EchoLink::default()));
        let result = run_activation_with_raw_bridge(&request(Profile::Flow, script), link.clone());
        assert!(
            matches!(result, ActivationResult::Failed(Failure::Script { .. })),
            "{result:?}"
        );
        assert!(link.borrow().issued.is_empty());
    }
}

#[test]
fn script_rejection_drains_outstanding_calls_before_finishing() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let result = run_activation(
        &request(
            Profile::Flow,
            "ops.call('mock', 'echo', 1); throw new Error('failed');",
        ),
        link.clone(),
    );
    assert!(matches!(
        result,
        ActivationResult::Failed(Failure::Script { .. })
    ));
    assert!(link.borrow().waiting.is_empty(), "unsettled calls remain");
}

#[test]
fn prelude_primitive_calls_match_the_rust_wire_codes() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let script = r#"
        Date.now(); Math.random();
        await facts('a', {}); await classify('t', []); await llm({});
        await sink.digest('a', {}, 'add'); await sink.status('a', {});
        await kv.get('k'); await kv.set('k', 1); await kv.delete('k');
        await fs.read('p', {}); await fs.list('p');
        await fs.stat('p'); await fs.write('p', 't'); await git.log('r', {});
        await git.revParse('r', 'HEAD'); await git.describeTags('r', {});
        await git.show('r', 'HEAD', 'p'); await git.diff('r', 'a', 'b', {});
        await net.fetch('https://example.com', {});
        await instance.end();
        await sink.board.append('a', 'work');
        await sink.board.setState('a', 'i', 'done');
        await sink.board.retire('a');
        await sink.wait.resolve('met', 'ready');
        await sink.room.post('rm_fleet', 'daily', {mentions: ['ag_other']});
        return null;
    "#;
    let result = run_activation(&request(Profile::Flow, script), link.clone());
    assert!(
        matches!(result, ActivationResult::Completed { .. }),
        "{result:?}"
    );
    let expected = [
        Primitive::Now,
        Primitive::Random,
        Primitive::Facts,
        Primitive::Classify,
        Primitive::Llm,
        Primitive::SinkDigest,
        Primitive::SinkStatus,
        Primitive::KvGet,
        Primitive::KvSet,
        Primitive::KvDelete,
        Primitive::FsRead,
        Primitive::FsList,
        Primitive::FsStat,
        Primitive::FsWrite,
        Primitive::GitLog,
        Primitive::GitRevParse,
        Primitive::GitDescribeTags,
        Primitive::GitShow,
        Primitive::GitDiff,
        Primitive::NetFetch,
        Primitive::InstanceEnd,
        Primitive::SinkBoard,
        Primitive::SinkBoard,
        Primitive::SinkBoard,
        Primitive::SinkWaitResolve,
        Primitive::SinkRoomPost,
    ];
    assert_eq!(
        link.borrow()
            .issued
            .iter()
            .map(|c| c.kind.clone())
            .collect::<Vec<_>>(),
        expected.map(CallKind::Primitive)
    );
}

/// The Rust side refuses `sleep` in the flow profile even when a script has
/// reached the native bridge directly, and the flow prelude defines neither
/// pause helper.
#[test]
fn raw_bridge_cannot_issue_sleep_in_flow_profile() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let script = "try { await new Promise(() => __rawBridge.issuePrimitive(27, '1')); } \
                  catch (e) { return 'caught'; } return 'issued';";
    let result = run_activation_with_raw_bridge(&request(Profile::Flow, script), link.clone());
    assert_eq!(
        result,
        ActivationResult::Failed(Failure::ProfileViolation {
            kind: CallKind::Primitive(Primitive::Sleep)
        })
    );
    assert!(link.borrow().issued.is_empty(), "sleep reached the host");
    let globals = run_activation(
        &request(Profile::Flow, "return [typeof sleep, typeof waitFor];"),
        Rc::new(RefCell::new(EchoLink::default())),
    );
    assert_eq!(
        globals,
        ActivationResult::Completed {
            value: JsonText::new("[\"undefined\",\"undefined\"]").unwrap()
        }
    );
}

#[test]
fn codemode_sleep_issues_the_rust_sleep_primitive() {
    let link = Rc::new(RefCell::new(EchoLink::default()));
    let result = run_activation(
        &request(Profile::Codemode, "return await sleep(7);"),
        link.clone(),
    );
    assert_eq!(
        result,
        ActivationResult::Completed {
            value: JsonText::null()
        }
    );
    let issued = &link.borrow().issued;
    assert_eq!(issued.len(), 1);
    assert_eq!(issued[0].kind, CallKind::Primitive(Primitive::Sleep));
    assert_eq!(issued[0].args.as_str(), "7");
}

/// Answers each wait only after spending CPU time on the worker's thread,
/// standing in for a parent that takes a while to complete a sleep.
#[derive(Default)]
struct SpinLink(EchoLink);

impl HostLink for SpinLink {
    fn issue(&mut self, call: &HostCall) -> Result<(), LinkError> {
        self.0.issue(call)
    }

    fn issue_sync(&mut self, call: &HostCall, order: Option<u64>) -> Result<Outcome, LinkError> {
        self.0.issue_sync(call, order)
    }

    fn wait(&mut self, awaiting: &[u64], order: Option<u64>) -> Result<WaitReply, LinkError> {
        let start = crate::clock::thread_cpu_time();
        while crate::clock::thread_cpu_time() - start < std::time::Duration::from_millis(40) {
            std::hint::spin_loop();
        }
        self.0.wait(awaiting, order)
    }
}

/// Time the worker spends waiting for a sleep to complete is outside the
/// engine, so it is not charged to the JavaScript CPU budget even when the
/// waiting thread is busy.
#[test]
fn sleep_waits_do_not_spend_the_js_cpu_budget() {
    let mut req = request(
        Profile::Codemode,
        "await sleep(10); await sleep(10); await sleep(10); return 3;",
    );
    req.budgets.js_time_micros = 20_000;
    let link = Rc::new(RefCell::new(SpinLink::default()));
    let result = run_activation(&req, link.clone());
    assert_eq!(
        result,
        ActivationResult::Completed {
            value: JsonText::new("3").unwrap()
        }
    );
    assert_eq!(link.borrow().0.issued.len(), 3);
}
