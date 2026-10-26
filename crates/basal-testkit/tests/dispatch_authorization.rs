//! Authorization at dispatch, in the parent: a call outside the manifest
//! is refused and journaled as `denied`, the script can catch it, and a
//! replay after a cut gives the same result; shell-capable ops are refused
//! by every route; every call, allowed or refused, has one audit row.

mod common;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use basal_core::channel::{ChannelError, WorkerChannel, WorkerReceiver};
use basal_core::manifest::OpRef;
use basal_core::{ActivationEnd, Config, RunState, ShellDenylist};
use basal_host::{OpDecl, OpKind};
use basal_proto::{
    ActivationResult, CallKind, Confinement, HostCall, JsonText, ParentMessage, PreludeHash,
    Primitive, Settlement, Welcome, WorkerMessage,
};
use basal_testkit::harness::{Point, World, run_with_cuts, run_with_cuts_under, test_manifest};
use common::{admit, config, finish, query_i64, result, runtime};
use serde_json::{Value, json};

fn rejection_code(value: &JsonText) -> Option<String> {
    serde_json::from_str::<Value>(value.as_str())
        .ok()
        .and_then(|v| v.get("code").and_then(Value::as_str).map(str::to_owned))
}

const UNLISTED: &str = r#"
    let code = null;
    try {
        await ops.call('mock', 'unlisted', { x: 1 });
    } catch (e) {
        code = e.data.code;
    }
    const after = await ops.call('mock', 'echo', { after: code });
    return { code, after: after.after };
"#;

#[test]
fn unlisted_op_is_denied_journaled_and_catchable() {
    let world = World::new("auth-unlisted");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, UNLISTED);
    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(result(&run), json!({"code": "denied", "after": "denied"}));
    let calls = rt.calls(&run_id).expect("calls");
    let refused = calls[0].outcome.as_ref().expect("journaled outcome");
    assert_eq!(refused.settlement, Settlement::Rejected);
    assert_eq!(rejection_code(&refused.value).as_deref(), Some("denied"));
    // Never sent: the mock saw only the echo.
    assert_eq!(world.mock.total_sends(), 1);
    let audit = rt
        .store()
        .read(|c| basal_core::audit::rows(c, &run_id))
        .expect("audit");
    let outcomes: Vec<&str> = audit.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(outcomes, ["denied", "allowed"]);
    assert_eq!(audit[0].op, "op (mock, unlisted)");

    // A cut right after the refusal commits, and right after its release,
    // replays to the same result.
    for cut in [
        "RefusalCommitted { position: 0 }#1",
        "Delivered { position: 0 }#1",
    ] {
        let world = World::new("auth-unlisted-cut");
        let point = Point::parse(cut).expect("point");
        let cut_run = run_with_cuts(&world, UNLISTED, &[point], &config()).expect("runs");
        assert_eq!(cut_run.fired, vec![true], "{cut}");
        assert_eq!(
            cut_run.summary.result,
            Some(json!({"code": "denied", "after": "denied"})),
            "{cut}: {cut_run:#?}"
        );
        assert!(cut_run.summary.audit_complete(), "{cut}: {cut_run:#?}");
        assert_eq!(world.mock.total_sends(), 1, "{cut}");
    }
}

/// Every route a script has to a shell: the shell op itself, spelt
/// differently, codemode, and the `sh` global. None is listed (install
/// refuses them), and none reaches the host.
#[test]
fn shell_ops_are_unreachable_from_a_script() {
    let world = World::new("auth-shell-script");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        r#"
        const out = {};
        for (const [m, o] of [['aft', 'bash'], ['AFT', 'Bash'], ['basal', 'codemode']]) {
            try {
                await ops.call(m, o, { command: 'id' });
                out[m + '.' + o] = 'sent';
            } catch (e) {
                out[m + '.' + o] = e.data.code;
            }
        }
        out.sh = typeof sh;
        out.global_sh = typeof globalThis.sh;
        return out;
        "#,
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(
        result(&run),
        json!({
            "aft.bash": "denied",
            "AFT.Bash": "denied",
            "basal.codemode": "denied",
            "sh": "undefined",
            "global_sh": "undefined",
        }),
        "{run:#?}"
    );
    assert_eq!(world.mock.total_sends(), 0);
}

const POST: &str = r#"
    try {
        await ops.call('mock', 'post', { n: 1 });
        return 'sent';
    } catch (e) {
        return e.data.code;
    }
"#;

/// An op the manifest lists, which its module marks shell-capable after the
/// flow was approved, is refused before dispatch.
#[test]
fn listed_op_marked_shell_capable_is_refused_at_dispatch() {
    let world = World::new("auth-shell-marker");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, POST);
    world.catalog.set_op(
        "mock",
        "post",
        OpDecl {
            kind: Some(OpKind::Mutate),
            cause_echo: false,
            shell_capable: true,
        },
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("denied"), "{run:#?}");
    assert!(world.mock.effects().is_empty());
}

/// An op the manifest lists, which the operator puts on the shell denylist
/// after the flow was approved (spelt in another case), is refused before
/// dispatch.
#[test]
fn listed_op_on_the_denylist_is_refused_at_dispatch() {
    let world = World::new("auth-shell-denylist");
    let run_id = {
        let rt = runtime(&world);
        let run_id = admit(&rt, &world, POST);
        rt.quiesce();
        run_id
    };
    world.mock.detach();
    let mut denylist = ShellDenylist::default();
    denylist.0.push(OpRef {
        module: "MOCK".into(),
        op: "Post".into(),
        for_target: None,
    });
    let rt = world
        .runtime(
            Arc::new(basal_core::NoHooks),
            Config {
                shell_denylist: denylist,
                ..config()
            },
        )
        .expect("runtime");
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!("denied"), "{run:#?}");
    assert!(world.mock.effects().is_empty());
}

/// A worker channel that, once activated, issues a forbidden call at position 0: what
/// a compromised worker could send, since the worker itself never would.
struct Compromised {
    welcome: Welcome,
    activated: bool,
    killed: Arc<AtomicBool>,
    message: WorkerMessage,
}

impl WorkerChannel for Compromised {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        if let ParentMessage::Activate(_) = message {
            self.activated = true;
        }
        Ok(())
    }

    fn recv(&mut self, _timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        if std::mem::take(&mut self.activated) {
            return Ok(self.message.clone());
        }
        Err(ChannelError::Closed)
    }

    fn receiver(&mut self) -> Box<dyn WorkerReceiver> {
        panic!("the flow driver reads this worker through recv")
    }

    fn kill(&mut self) {
        self.killed.store(true, Ordering::SeqCst);
    }
}

#[test]
fn a_compromised_worker_cannot_issue_sh() {
    let world = World::new("auth-shell-worker");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, "return 1;");
    let mut real = world.source.spawn().expect("worker");
    let killed = Arc::new(AtomicBool::new(false));
    let mut fake = Compromised {
        welcome: real.welcome().clone(),
        activated: false,
        killed: killed.clone(),
        message: WorkerMessage::HostCall(HostCall {
            position: 0,
            kind: CallKind::Primitive(Primitive::Sh),
            args: JsonText::new("{\"command\":\"id\"}").expect("small"),
        }),
    };
    real.kill();
    let end = rt.activate(&run_id, &mut fake).expect("activation");
    assert!(
        matches!(&end, ActivationEnd::Failed { kind, .. } if kind == "profile_violation"),
        "{end:?}"
    );
    assert!(killed.load(Ordering::SeqCst), "the worker was not killed");
    assert!(rt.calls(&run_id).expect("calls").is_empty());
    assert_eq!(query_i64(&rt, "SELECT COUNT(*) FROM call_audit"), 0);
    assert_eq!(world.mock.total_sends(), 0);
}

#[test]
fn a_compromised_worker_cannot_issue_tool() {
    let world = World::new("auth-tool-worker");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, "return 1;");
    let mut real = world.source.spawn().expect("worker");
    let killed = Arc::new(AtomicBool::new(false));
    let mut fake = Compromised {
        welcome: real.welcome().clone(),
        activated: false,
        killed: killed.clone(),
        message: WorkerMessage::HostCall(HostCall {
            position: 0,
            kind: CallKind::Tool {
                name: "echo".into(),
            },
            args: JsonText::null(),
        }),
    };
    real.kill();
    let end = rt.activate(&run_id, &mut fake).expect("activation");
    assert!(
        matches!(&end, ActivationEnd::Failed { kind, .. } if kind == "profile_violation"),
        "{end:?}"
    );
    assert!(killed.load(Ordering::SeqCst), "the worker was not killed");
    assert!(rt.calls(&run_id).expect("calls").is_empty());
    assert_eq!(query_i64(&rt, "SELECT COUNT(*) FROM call_audit"), 0);
    assert_eq!(world.mock.total_sends(), 0);
}

#[test]
fn a_compromised_worker_cannot_issue_sleep() {
    let world = World::new("auth-sleep-worker");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, "return 1;");
    let mut real = world.source.spawn().expect("worker");
    let killed = Arc::new(AtomicBool::new(false));
    let mut fake = Compromised {
        welcome: real.welcome().clone(),
        activated: false,
        killed: killed.clone(),
        message: WorkerMessage::HostCall(HostCall {
            position: 0,
            kind: CallKind::Primitive(Primitive::Sleep),
            args: JsonText::new("1000").expect("small"),
        }),
    };
    real.kill();
    let end = rt.activate(&run_id, &mut fake).expect("activation");
    assert!(
        matches!(&end, ActivationEnd::Failed { kind, .. } if kind == "profile_violation"),
        "{end:?}"
    );
    assert!(killed.load(Ordering::SeqCst), "the worker was not killed");
    assert!(rt.calls(&run_id).expect("calls").is_empty());
    assert_eq!(query_i64(&rt, "SELECT COUNT(*) FROM call_audit"), 0);
    assert_eq!(world.mock.total_sends(), 0);
}

#[test]
fn a_compromised_worker_cannot_send_console() {
    let world = World::new("auth-console-worker");
    let rt = runtime(&world);
    let run_id = admit(&rt, &world, "return 1;");
    let mut real = world.source.spawn().expect("worker");
    let killed = Arc::new(AtomicBool::new(false));
    let mut fake = Compromised {
        welcome: real.welcome().clone(),
        activated: false,
        killed: killed.clone(),
        message: WorkerMessage::Console {
            line: "a 1\n".into(),
        },
    };
    real.kill();
    let end = rt.activate(&run_id, &mut fake).expect("activation");
    assert!(
        matches!(&end, ActivationEnd::Failed { kind, .. } if kind == "profile_violation"),
        "{end:?}"
    );
    assert!(killed.load(Ordering::SeqCst));
    assert!(rt.calls(&run_id).expect("calls").is_empty());
    assert_eq!(query_i64(&rt, "SELECT COUNT(*) FROM call_audit"), 0);
    assert_eq!(world.mock.total_sends(), 0);
}

#[test]
fn facts_sinks_and_model_calls_are_checked_against_the_manifest() {
    let world = World::new("auth-primitives");
    let rt = runtime(&world);
    let mut manifest = test_manifest();
    manifest["sinks"] = json!([{ "agent": "ALF", "digest_max": "piggyback" }]);
    let spec = world
        .spec_with(
            &rt,
            r#"
            const out = {};
            const attempt = async (name, f) => {
                try { await f(); out[name] = 'ok'; } catch (e) { out[name] = e.data.code; }
            };
            await attempt('facts', () => facts('ALF'));
            await attempt('facts_other', () => facts('SYNAPSE'));
            await attempt('facts_text', () => facts('ALF', { include: ['text'] }));
            await attempt('digest', () => sink.digest('ALF', { title: 't' }, 'piggyback'));
            await attempt('digest_above_cap', () => sink.digest('ALF', { title: 't' }, 'wake'));
            await attempt('digest_other', () => sink.digest('SYNAPSE', { title: 't' }, 'silent'));
            await attempt('status', () => sink.status('ALF', 'v'));
            await attempt('status_other', () => sink.status('SYNAPSE', 'v'));
            await attempt('tools', () => llm({ prompt: 'x', tools: [] }));
            return out;
            "#,
            &manifest,
        )
        .expect("approve");
    let run_id = rt
        .admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();
    let run = finish(&rt, &world, &run_id);
    assert_eq!(
        result(&run),
        json!({
            "facts": "ok",
            "facts_other": "denied",
            "facts_text": "denied",
            "digest": "ok",
            "digest_above_cap": "denied",
            "digest_other": "denied",
            "status": "ok",
            "status_other": "denied",
            "tools": "denied",
        }),
        "{run:#?}"
    );
    // Only the three allowed calls were sent.
    assert_eq!(world.mock.total_sends(), 3);
}

/// One audit row per call, written with its journal row, and none more when
/// the run is replayed after a suspension.
#[test]
fn every_call_has_one_audit_row_and_replay_writes_none() {
    let world = World::new("auth-audit");
    let rt = runtime(&world);
    let run_id = admit(
        &rt,
        &world,
        r#"
        const t = Date.now();
        try { await ops.call('mock', 'unlisted', {}); } catch (e) {}
        await kv.set('k', 1);
        const a = await ops.call('mock', 'echo', { a: 1 });
        const l = await ops.call('mock', 'long', {});
        return [a.a, l.done, await kv.get('k')];
        "#,
    );
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    let audited = |rt: &basal_core::Runtime| {
        query_i64(
            rt,
            &format!("SELECT COUNT(*) FROM call_audit WHERE run_id = '{run_id}'"),
        )
    };
    assert_eq!(audited(&rt), 5);
    let run = finish(&rt, &world, &run_id);
    assert_eq!(result(&run), json!([1, true, 1]));
    assert!(rt.activation_count(&run_id).expect("count") >= 2);
    // The second activation replayed five calls and issued one more.
    assert_eq!(audited(&rt), 6);
    assert_eq!(rt.calls(&run_id).expect("calls").len(), 6);
}

/// The representative run's cuts are in `journal_cut`; this one checks a
/// refused call under a different manifest replays the same way too.
#[test]
fn refusals_replay_the_same_after_a_cut_under_any_manifest() {
    let world = World::new("auth-cut-manifest");
    let mut manifest = test_manifest();
    manifest["ops"] = json!([{ "module": "mock", "op": "echo" }]);
    let script = r#"
        let code = null;
        try { await ops.call('mock', 'send', {}); } catch (e) { code = e.data.code; }
        return code;
    "#;
    let point = Point::parse("Delivered { position: 0 }#1").expect("point");
    let run = run_with_cuts_under(&world, script, &manifest, &[point], &config()).expect("runs");
    assert_eq!(run.fired, vec![true]);
    assert_eq!(run.summary.result, Some(json!("denied")), "{run:#?}");
    assert!(world.mock.effects().is_empty());
}

/// A worker channel that stands in for the script engine: it issues `calls`
/// one at a time, waits for each outcome, and finishes with what each call
/// returned (its rejection code and message, or `"fulfilled"`). The parent's
/// checks run exactly as for a real worker, without needing a confined
/// worker process.
struct Scripted {
    welcome: Welcome,
    activation: Option<u64>,
    calls: VecDeque<HostCall>,
    awaiting: Option<u64>,
    seen: Vec<Value>,
}

impl Scripted {
    fn new(calls: Vec<(Primitive, Value)>) -> Self {
        Self {
            welcome: Welcome {
                protocol_version: basal_proto::PROTOCOL_VERSION,
                engine: "scripted".into(),
                prelude_hash: PreludeHash([0; 32]),
                codemode_prelude_hash: PreludeHash([0; 32]),
                confinement: Confinement::None,
            },
            activation: None,
            calls: calls
                .into_iter()
                .enumerate()
                .map(|(position, (primitive, args))| HostCall {
                    position: position as u64,
                    kind: CallKind::Primitive(primitive),
                    args: JsonText::new(args.to_string()).expect("small"),
                })
                .collect(),
            awaiting: None,
            seen: Vec::new(),
        }
    }
}

impl WorkerChannel for Scripted {
    fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    fn send(&mut self, message: &ParentMessage) -> Result<(), ChannelError> {
        match message {
            ParentMessage::Activate(request) => self.activation = Some(request.activation_id),
            ParentMessage::Deliver(outcome) if self.awaiting == Some(outcome.position) => {
                self.awaiting = None;
                self.seen.push(match outcome.settlement {
                    Settlement::Rejected => {
                        let value: Value =
                            serde_json::from_str(outcome.value.as_str()).expect("rejection JSON");
                        json!(format!(
                            "{}: {}",
                            value["code"].as_str().unwrap_or_default(),
                            value["message"].as_str().unwrap_or_default()
                        ))
                    }
                    _ => json!("fulfilled"),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn recv(&mut self, _timeout: Duration) -> Result<WorkerMessage, ChannelError> {
        let Some(activation_id) = self.activation else {
            return Err(ChannelError::Closed);
        };
        if let Some(position) = self.awaiting {
            return Ok(WorkerMessage::Blocked {
                awaiting: vec![position],
            });
        }
        if let Some(call) = self.calls.pop_front() {
            self.awaiting = Some(call.position);
            return Ok(WorkerMessage::HostCall(call));
        }
        self.activation = None;
        Ok(WorkerMessage::Finished {
            activation_id,
            result: ActivationResult::Completed {
                value: JsonText::new(Value::from(self.seen.clone()).to_string()).expect("small"),
            },
        })
    }

    fn receiver(&mut self) -> Box<dyn WorkerReceiver> {
        panic!("the flow driver reads this worker through recv")
    }

    fn kill(&mut self) {}
}

/// `$audience` is valid only in a manifest grant. A script that passes it
/// (or an unresolved `$self`) as a recipient is refused before anything is
/// sent, with a code and message that say what to pass instead, while a
/// concrete recipient under the same grant is still sent.
#[test]
fn placeholder_recipients_are_refused_before_dispatch() {
    let world = World::new("auth-placeholder-target");
    let rt = runtime(&world);
    let mut manifest = test_manifest();
    manifest["audience"] = json!({"kind":"agent","id":"ALF"});
    manifest["sinks"] = json!([{ "agent": "$audience", "digest_max": "wake" }]);
    manifest["status"] = json!(["$audience"]);
    manifest.as_object_mut().unwrap().remove("facts");
    let spec = world
        .spec_with(&rt, "return 1;", &manifest)
        .expect("approve");
    let run_id = rt
        .admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();
    let mut worker = Scripted::new(vec![
        (
            Primitive::SinkDigest,
            json!({"agent":"$audience","withdraw":true,"options":{"replace_key":"runner-alert"}}),
        ),
        (
            Primitive::SinkDigest,
            json!({"agent":"$audience","item":{"title":"alert"}}),
        ),
        (
            Primitive::SinkStatus,
            json!({"agent":"$audience","value":"ok"}),
        ),
        (Primitive::Facts, json!({"agent":"$self"})),
        (
            Primitive::SinkDigest,
            json!({"agent":"ALF","item":{"title":"alert"}}),
        ),
    ]);
    let end = rt.activate(&run_id, &mut worker).expect("activation");
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    let run = rt.run(&run_id).expect("run");
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    let refused = |name: &str| {
        json!(format!(
            "sink_target_placeholder: \"{name}\" is a manifest placeholder; pass the recipient's agent id (ALF is this flow's approved audience)"
        ))
    };
    assert_eq!(
        result(&run),
        json!([
            refused("$audience"),
            refused("$audience"),
            refused("$audience"),
            refused("$self"),
            "fulfilled"
        ])
    );
    // Only the concrete recipient reached the host.
    assert_eq!(world.mock.total_sends(), 1);
    let audit = rt
        .store()
        .read(|c| basal_core::audit::rows(c, &run_id))
        .expect("audit");
    let outcomes: Vec<&str> = audit.iter().map(|r| r.outcome.as_str()).collect();
    assert_eq!(
        outcomes,
        [
            "sink_target_placeholder",
            "sink_target_placeholder",
            "sink_target_placeholder",
            "sink_target_placeholder",
            "allowed"
        ]
    );
}
