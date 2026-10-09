//! Shutdown must end a worker even when an activation is waiting on its parent.
use basal_proto::{
    ActivationRequest, Budgets, Confinement, JsonText, ParentMessage, Profile, WorkerMessage,
    encode_parent_frame, read_worker_message,
};
use std::{
    cell::RefCell,
    io::{self, Write},
    rc::Rc,
};

#[derive(Clone)]
struct Output(Rc<RefCell<Vec<u8>>>);
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn shutdown_ends_sync_and_async_activations_without_refusal() {
    for script in [
        "return Date.now();",
        "return await ops.call('mock', 'echo', 1);",
    ] {
        let request = ActivationRequest {
            activation_id: 1,
            profile: Profile::Flow,
            prelude_hash: basal_worker::engine::prelude_hash(),
            tools: vec![],
            script: script.into(),
            trigger: JsonText::null(),
            self_input: JsonText::null(),
            budgets: Budgets::default(),
            prefix: vec![],
        };
        let mut input = vec![];
        for message in [
            ParentMessage::Hello {
                protocol_version: basal_proto::PROTOCOL_VERSION,
            },
            ParentMessage::Activate(Box::new(request)),
            ParentMessage::Shutdown,
        ] {
            input.extend(encode_parent_frame(&message).unwrap());
        }
        let out = Output(Rc::new(RefCell::new(vec![])));
        let exit =
            basal_worker::serve::serve(io::Cursor::new(input), out.clone(), Confinement::Seatbelt);
        assert_eq!(exit, basal_worker::serve::ServeExit::Done);
        let bytes = out.0.borrow();
        let mut stream = bytes.as_slice();
        assert!(matches!(
            read_worker_message(&mut stream).unwrap(),
            WorkerMessage::Welcome(_)
        ));
        assert!(matches!(
            read_worker_message(&mut stream).unwrap(),
            WorkerMessage::HostCall(_)
        ));
        if script.contains("await") {
            assert!(matches!(
                read_worker_message(&mut stream).unwrap(),
                WorkerMessage::Blocked { .. }
            ));
        }
        assert!(
            stream.is_empty(),
            "shutdown emitted refusal or Finished: {stream:?}"
        );
    }
}

#[test]
fn encoder_rejects_fields_that_its_decoder_would_refuse() {
    for module in ["".to_string(), "x".repeat(basal_proto::MAX_NAME_BYTES + 1)] {
        let message = WorkerMessage::HostCall(basal_proto::HostCall {
            position: 0,
            kind: basal_proto::CallKind::Op {
                module,
                op: "echo".into(),
            },
            args: JsonText::null(),
        });
        assert!(basal_proto::encode_worker_frame(&message).is_err());
    }
    let mut request = ActivationRequest {
        activation_id: 1,
        profile: Profile::Flow,
        prelude_hash: basal_worker::engine::prelude_hash(),
        tools: vec![],
        script: "x".repeat(basal_proto::MAX_SCRIPT_BYTES + 1),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        budgets: Budgets::default(),
        prefix: vec![],
    };
    assert!(encode_parent_frame(&ParentMessage::Activate(Box::new(request.clone()))).is_err());
    request.script = "return 1".into();
    let too_many = ParentMessage::LongRunning {
        positions: vec![0; basal_proto::MAX_LIST_ENTRIES + 1],
    };
    assert!(encode_parent_frame(&too_many).is_err());
}
