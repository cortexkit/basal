//! In-process tests of the engine, driven by an in-memory host link. These
//! cover what a script cannot reach through the prelude: the native bridge
//! itself.

use std::cell::RefCell;
use std::rc::Rc;

use basal_proto::{
    ActivationRequest, ActivationResult, Budgets, CallKind, Failure, HostCall, JsonText, Outcome,
    Primitive, Profile, Settlement,
};

use super::{prelude_hash, run_activation, run_activation_with_raw_bridge};
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
        prelude_hash: prelude_hash(),
        script: script.into(),
        trigger: JsonText::null(),
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
