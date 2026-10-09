//! In-process tests of the engine, driven by an in-memory host link. These
//! cover what a script cannot reach through the prelude: the native bridge
//! itself.

use std::cell::RefCell;
use std::rc::Rc;

use basal_proto::{
    ActivationRequest, ActivationResult, Budgets, CallKind, Failure, HostCall, JsonText, Outcome,
    Primitive, Profile, Settlement,
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
