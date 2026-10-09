//! The channel: malformed, oversized and out-of-order frames are refused
//! with typed errors, and none of them crashes the worker.

mod common;

use std::time::Duration;

use basal_proto::{
    ActivationRequest, ActivationResult, Budgets, CallKind, Confinement, JsonText, MAX_FRAME_BYTES,
    MessageKind, Outcome, PROTOCOL_VERSION, ParentMessage, PreludeHash, Primitive, Profile,
    Refusal, Settlement, Welcome, WorkerMessage, WorkerState, write_raw_frame,
};
use basal_testkit::{ParentError, WorkerProcess};

const WAIT: Duration = Duration::from_secs(10);

fn spawn() -> WorkerProcess {
    WorkerProcess::spawn(&common::worker_binary()).expect("spawn worker")
}

fn hello(worker: &mut WorkerProcess) -> Welcome {
    worker.handshake(WAIT).expect("handshake")
}

fn activation(prelude_hash: PreludeHash, script: &str) -> ParentMessage {
    ParentMessage::Activate(Box::new(ActivationRequest {
        activation_id: 9,
        profile: Profile::Flow,
        tools: vec![],
        prelude_hash,
        script: script.into(),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        budgets: Budgets::default(),
        prefix: Vec::new(),
    }))
}

fn deliver(position: u64, value: &str, delivery_order: u64) -> ParentMessage {
    ParentMessage::Deliver(Outcome {
        position,
        settlement: Settlement::Fulfilled,
        value: JsonText::new(value).expect("small"),
        delivery_order,
    })
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    write_raw_frame(&mut bytes, payload).expect("frame");
    bytes
}

fn refusal(worker: &WorkerProcess) -> Refusal {
    match worker.recv(WAIT) {
        Ok(WorkerMessage::Refused(r)) => r,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn unexpected(received: MessageKind, state: WorkerState) -> Refusal {
    Refusal::UnexpectedFrame { received, state }
}

/// Runs a trivial activation to show the worker is alive and in sync.
fn still_serves(worker: &mut WorkerProcess, prelude_hash: PreludeHash) {
    worker
        .send(&activation(prelude_hash, "return 42"))
        .expect("send");
    match worker.recv(WAIT) {
        Ok(WorkerMessage::Finished { result, .. }) => assert_eq!(
            result,
            ActivationResult::Completed {
                value: JsonText::new("42").expect("small")
            }
        ),
        other => panic!("worker no longer serves: {other:?}"),
    }
}

#[test]
fn handshake_reports_version_engine_and_confinement() {
    let mut worker = spawn();
    let welcome = hello(&mut worker);
    assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
    #[cfg(target_os = "macos")]
    assert_eq!(welcome.confinement, Confinement::Seatbelt);
    #[cfg(target_os = "linux")]
    assert!(matches!(
        welcome.confinement,
        Confinement::Linux {
            seccomp: true,
            landlock: Some(_)
        }
    ));
    assert!(welcome.engine.contains("quickjs"), "{}", welcome.engine);
}

#[cfg(target_os = "linux")]
#[test]
fn required_linux_welcome_activation_and_shutdown() {
    let mut worker =
        WorkerProcess::spawn_with_args(&common::worker_binary(), &["--landlock=required"])
            .expect("required Linux worker launch");
    let welcome = hello(&mut worker);
    assert_eq!(welcome.protocol_version, PROTOCOL_VERSION);
    match welcome.confinement {
        Confinement::Linux {
            seccomp: true,
            landlock: Some(report),
        } => {
            assert!(report.runtime_abi >= 1, "{report:?}");
            assert_eq!(
                report.applied_abi,
                report.runtime_abi.min(basal_proto::LANDLOCK_ABI)
            );
            eprintln!("required-mode Welcome Landlock readback: {report:?}");
        }
        other => panic!("required Linux confinement missing from Welcome: {other:?}"),
    }
    still_serves(&mut worker, welcome.prelude_hash);
    worker
        .send(&ParentMessage::Shutdown)
        .expect("send shutdown");
    assert_eq!(
        worker.wait_exit(WAIT).expect("worker exits").code(),
        Some(0)
    );
}

#[test]
fn malformed_frames_are_refused_and_the_worker_survives() {
    let mut worker = spawn();
    let welcome = hello(&mut worker);
    let hash = welcome.prelude_hash;

    // An unknown tag.
    worker.send_raw(&frame(&[99])).expect("send");
    assert!(matches!(refusal(&worker), Refusal::Malformed { .. }));
    // A Deliver cut short inside its fields.
    worker.send_raw(&frame(&[3, 0, 0, 0])).expect("send");
    assert!(matches!(refusal(&worker), Refusal::Malformed { .. }));
    // A value that is not UTF-8.
    let mut payload = vec![3u8];
    payload.extend_from_slice(&0u64.to_be_bytes());
    payload.push(0);
    payload.extend_from_slice(&2u32.to_be_bytes());
    payload.extend_from_slice(&[0xc3, 0x28]);
    payload.extend_from_slice(&0u64.to_be_bytes());
    worker.send_raw(&frame(&payload)).expect("send");
    assert!(matches!(refusal(&worker), Refusal::Malformed { .. }));
    // A worker-to-parent message sent the wrong way.
    let wrong_way = basal_proto::encode_worker_frame(&WorkerMessage::Blocked { awaiting: vec![] })
        .expect("frame");
    worker.send_raw(&wrong_way).expect("send");
    assert!(matches!(refusal(&worker), Refusal::Malformed { .. }));
    // Trailing bytes after a valid message.
    worker.send_raw(&frame(&[5, 0])).expect("send");
    assert!(matches!(refusal(&worker), Refusal::Malformed { .. }));

    still_serves(&mut worker, hash);
    worker.send(&ParentMessage::Shutdown).expect("send");
    let status = worker.wait_exit(WAIT).expect("worker exits");
    assert_eq!(status.code(), Some(0));
}

#[test]
fn oversized_frame_is_refused_and_the_worker_exits_cleanly() {
    let mut worker = spawn();
    hello(&mut worker);
    let declared = MAX_FRAME_BYTES as u32 + 1;
    worker.send_raw(&declared.to_be_bytes()).expect("send");
    assert_eq!(
        refusal(&worker),
        Refusal::Oversized {
            declared: declared as u64,
            max: MAX_FRAME_BYTES as u64
        }
    );
    // The stream cannot be framed any more, so the worker exits with code 3
    // (a broken channel) rather than crashing on a signal.
    let status = worker.wait_exit(WAIT).expect("worker exits");
    assert_eq!(
        status.code(),
        Some(3),
        "{status:?}: {}",
        worker.stderr_text()
    );
}

#[test]
fn out_of_order_frames_are_refused() {
    let mut worker = spawn();

    // Before the handshake.
    worker
        .send(&activation(PreludeHash([0; 32]), "return 1"))
        .expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::Activate, WorkerState::AwaitingHello)
    );
    worker.send(&deliver(0, "1", 0)).expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::Deliver, WorkerState::AwaitingHello)
    );
    worker
        .send(&ParentMessage::Hello {
            protocol_version: PROTOCOL_VERSION + 1,
        })
        .expect("send");
    assert_eq!(
        refusal(&worker),
        Refusal::VersionMismatch {
            supported: PROTOCOL_VERSION,
            requested: PROTOCOL_VERSION + 1
        }
    );
    let hash = hello(&mut worker).prelude_hash;

    // Between activations.
    worker.send(&deliver(0, "1", 0)).expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::Deliver, WorkerState::Idle)
    );
    worker
        .send(&ParentMessage::LongRunning { positions: vec![0] })
        .expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::LongRunning, WorkerState::Idle)
    );
    worker
        .send(&ParentMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
        })
        .expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::Hello, WorkerState::Idle)
    );

    // While blocked on an asynchronous call.
    worker
        .send(&activation(
            hash,
            "const a = await ops.call('mock', 'echo', 1); const b = await ops.call('mock', 'echo', 2); return [a, b];",
        ))
        .expect("send");
    assert!(matches!(worker.recv(WAIT), Ok(WorkerMessage::HostCall(c)) if c.position == 0));
    assert_eq!(
        worker.recv(WAIT),
        Ok(WorkerMessage::Blocked { awaiting: vec![0] })
    );
    worker.send(&deliver(7, "1", 0)).expect("send");
    assert_eq!(refusal(&worker), Refusal::UnknownPosition { position: 7 });
    worker.send(&activation(hash, "return 1")).expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::Activate, WorkerState::Blocked)
    );
    worker
        .send(&ParentMessage::LongRunning { positions: vec![9] })
        .expect("send");
    assert_eq!(refusal(&worker), Refusal::UnknownPosition { position: 9 });
    worker
        .send(&ParentMessage::LongRunning { positions: vec![] })
        .expect("send");
    assert!(matches!(refusal(&worker), Refusal::Malformed { .. }));
    worker.send(&deliver(0, "1", 5)).expect("send");
    assert!(matches!(worker.recv(WAIT), Ok(WorkerMessage::HostCall(c)) if c.position == 1));
    assert_eq!(
        worker.recv(WAIT),
        Ok(WorkerMessage::Blocked { awaiting: vec![1] })
    );
    // Delivery orders must increase.
    worker.send(&deliver(1, "2", 3)).expect("send");
    assert_eq!(
        refusal(&worker),
        Refusal::DeliveryOrderRegression {
            last: 5,
            received: 3
        }
    );
    worker.send(&deliver(1, "2", 6)).expect("send");
    match worker.recv(WAIT) {
        Ok(WorkerMessage::Finished { result, .. }) => assert_eq!(
            result,
            ActivationResult::Completed {
                value: JsonText::new("[1,2]").expect("small")
            }
        ),
        other => panic!("{other:?}"),
    }

    // While waiting for a synchronous answer.
    worker
        .send(&activation(hash, "return typeof Date.now();"))
        .expect("send");
    match worker.recv(WAIT) {
        Ok(WorkerMessage::HostCall(c)) => {
            assert_eq!(c.kind, CallKind::Primitive(Primitive::Now));
        }
        other => panic!("{other:?}"),
    }
    worker.send(&deliver(3, "1", 0)).expect("send");
    assert_eq!(refusal(&worker), Refusal::UnknownPosition { position: 3 });
    worker
        .send(&ParentMessage::LongRunning { positions: vec![0] })
        .expect("send");
    assert_eq!(
        refusal(&worker),
        unexpected(MessageKind::LongRunning, WorkerState::AwaitingSyncReply)
    );
    worker.send(&deliver(0, "1767225600000", 0)).expect("send");
    match worker.recv(WAIT) {
        Ok(WorkerMessage::Finished {
            result: ActivationResult::Completed { value },
            ..
        }) => {
            assert_eq!(value.as_str(), "\"number\"");
        }
        other => panic!("Date.now did not return a number: {other:?}"),
    }

    still_serves(&mut worker, hash);
    worker.send(&ParentMessage::Shutdown).expect("send");
    assert_eq!(worker.wait_exit(WAIT).and_then(|s| s.code()), Some(0));
}

#[test]
fn closing_the_channel_ends_the_worker_cleanly() {
    let mut worker = spawn();
    hello(&mut worker);
    worker.close_stdin();
    assert_eq!(worker.recv(WAIT), Err(ParentError::Closed));
    assert_eq!(worker.wait_exit(WAIT).and_then(|s| s.code()), Some(0));
}
