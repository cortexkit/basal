//! The IPC protocol between basal's parent process and a `ck-basal-worker`.
//!
//! The parent owns every durable thing (journal, audit, manifests, dispatch);
//! the worker is stateless compute that runs one activation of a script at a
//! time. They talk over the worker's stdin and stdout in length-prefixed
//! frames. One conversation goes:
//!
//! 1. The parent sends [`ParentMessage::Hello`] with its protocol version; the
//!    worker answers [`WorkerMessage::Welcome`] (or refuses a mismatch).
//! 2. The parent sends [`ParentMessage::Activate`] carrying the script, the
//!    budgets and the run's recorded journal prefix. The worker replays the
//!    prefix locally and crosses the boundary only for new calls:
//!    - [`WorkerMessage::HostCall`] issues a new call. A synchronous call
//!      (a clock read or a random sample) is answered at once with a
//!      [`ParentMessage::Deliver`] for the same position.
//!    - [`WorkerMessage::Blocked`] says the script cannot progress until an
//!      awaited call settles; the parent answers with one `Deliver` or with
//!      [`ParentMessage::LongRunning`].
//! 3. The worker ends the activation with [`WorkerMessage::Finished`] and
//!    waits for the next `Activate` or [`ParentMessage::Shutdown`].
//!
//! Any frame the worker does not expect is answered with
//! [`WorkerMessage::Refused`] carrying a typed [`Refusal`]. This crate does
//! no I/O beyond reading and writing frames on the streams it is handed.

mod codec;
mod frame;
mod limits;
mod types;
mod wire;

pub use codec::DecodeError;
pub use frame::{
    FrameError, decode_parent_payload, decode_worker_payload, encode_parent_frame,
    encode_worker_frame, read_frame, read_parent_message, read_worker_message, write_host_call,
    write_parent_message, write_raw_frame, write_worker_message,
};
pub use limits::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> JsonText {
        JsonText::new(s).expect("small test value")
    }

    fn sample_request() -> ActivationRequest {
        ActivationRequest {
            activation_id: 7,
            profile: Profile::Codemode,
            tools: vec!["lookup".into(), "echo".into()],
            prelude_hash: PreludeHash::of("prelude"),
            script: "return 1".into(),
            trigger: text("{\"a\":1}"),
            self_input: text("{\"agent_id\":\"agent_test\"}"),
            budgets: Budgets::default(),
            prefix: vec![
                RecordedCall {
                    position: 0,
                    kind: CallKind::Op {
                        module: "mock".into(),
                        op: "echo".into(),
                    },
                    args_digest: ArgsDigest::of(&text("1")),
                    outcome: Some(RecordedOutcome {
                        settlement: Settlement::Rejected,
                        value: text("{\"message\":\"no\"}"),
                        delivery_order: 3,
                    }),
                },
                RecordedCall {
                    position: 1,
                    kind: CallKind::Primitive(Primitive::Llm),
                    args_digest: ArgsDigest::of(&text("{}")),
                    outcome: None,
                },
            ],
        }
    }

    #[test]
    fn codemode_wire_codes_preserve_shell_and_round_trip_tool_json() {
        assert_eq!(PROTOCOL_VERSION, 4);
        assert!(CODEMODE_ADDRESS_SPACE_BYTES > 64 * 1024 * 1024);
        assert_eq!(Primitive::Sh.code(), 11);
        for (kind, code) in [
            (CallKind::Primitive(Primitive::Sh), 11),
            (
                CallKind::Tool {
                    name: "echo".into(),
                },
                22,
            ),
        ] {
            let message = WorkerMessage::HostCall(HostCall {
                position: 7,
                kind,
                args: text("{\"a\":1}"),
            });
            let frame = encode_worker_frame(&message).unwrap();
            assert_eq!(frame[13], code);
            if code == 22 {
                assert!(Primitive::ALL.iter().all(|p| p.code() < code));
            }
            assert_eq!(read_worker_message(&mut frame.as_slice()).unwrap(), message);
        }
        let mut shell = vec![102];
        shell.extend_from_slice(&7u64.to_be_bytes());
        shell.extend_from_slice(&[11, 0, 0, 0, 4]);
        shell.extend_from_slice(b"null");
        assert_eq!(
            decode_worker_payload(&shell).unwrap(),
            WorkerMessage::HostCall(HostCall {
                position: 7,
                kind: CallKind::Primitive(Primitive::Sh),
                args: text("null"),
            })
        );
    }

    #[test]
    fn codemode_activation_tools_round_trip_and_names_are_bounded() {
        let message = ParentMessage::Activate(Box::new(sample_request()));
        let frame = encode_parent_frame(&message).unwrap();
        assert_eq!(read_parent_message(&mut frame.as_slice()).unwrap(), message);
        for name in [String::new(), "a".repeat(MAX_NAME_BYTES + 1)] {
            let mut req = sample_request();
            req.tools = vec![name];
            assert!(encode_parent_frame(&ParentMessage::Activate(Box::new(req))).is_err());
        }
    }

    #[test]
    fn codemode_welcome_reports_two_distinct_prelude_hashes() {
        let message = WorkerMessage::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            engine: "quickjs".into(),
            prelude_hash: PreludeHash::of("flow"),
            codemode_prelude_hash: PreludeHash::of("codemode"),
            confinement: Confinement::None,
        });
        let frame = encode_worker_frame(&message).unwrap();
        assert_eq!(read_worker_message(&mut frame.as_slice()).unwrap(), message);
    }

    #[test]
    fn codemode_console_has_a_unique_worker_tag_and_round_trips() {
        let message = WorkerMessage::Console {
            line: "a 1\n".into(),
        };
        let frame = encode_worker_frame(&message).unwrap();
        assert_eq!(frame[4], 106);
        assert_eq!(read_worker_message(&mut frame.as_slice()).unwrap(), message);
        assert!(decode_parent_payload(&frame[4..]).is_err());
    }

    #[test]
    fn every_parent_message_round_trips() {
        let messages = vec![
            ParentMessage::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            ParentMessage::Activate(Box::new(sample_request())),
            ParentMessage::Deliver(Outcome {
                position: 4,
                settlement: Settlement::Fulfilled,
                value: text("\"\u{2028}\""),
                delivery_order: 9,
            }),
            ParentMessage::LongRunning {
                positions: vec![1, 2],
            },
            ParentMessage::Shutdown,
        ];
        for m in messages {
            let frame = encode_parent_frame(&m).expect("encodes");
            let back = read_parent_message(&mut frame.as_slice()).expect("decodes");
            assert_eq!(back, m);
        }
    }

    #[test]
    fn every_worker_message_round_trips() {
        let sig = CallSignature {
            kind: CallKind::Primitive(Primitive::Facts),
            args_digest: ArgsDigest::of(&text("null")),
        };
        let failures = vec![
            Failure::Script {
                message: "boom".into(),
            },
            Failure::ScriptHostRejection {
                position: 7,
                message: "no scope".into(),
            },
            Failure::Nondeterminism(Nondeterminism::Divergence {
                position: 2,
                recorded: sig.clone(),
                observed: sig.clone(),
            }),
            Failure::Nondeterminism(Nondeterminism::UnreleasedOutcome {
                position: 1,
                delivery_order: 5,
            }),
            Failure::Nondeterminism(Nondeterminism::UnconsumedCall { position: 3 }),
            Failure::ProfileViolation {
                kind: CallKind::Primitive(Primitive::Sh),
            },
            Failure::EngineMismatch {
                expected: PreludeHash::of("a"),
                actual: PreludeHash::of("b"),
            },
            Failure::InvalidRequest { detail: "x".into() },
            Failure::ArgumentsTooLarge { bytes: 9, cap: 8 },
            Failure::ResultTooLarge { bytes: 9, cap: 8 },
            Failure::ResultNotSerializable { detail: "y".into() },
            Failure::InvalidHostValue {
                position: 1,
                detail: "z".into(),
            },
            Failure::HostLink { detail: "w".into() },
            Failure::Engine { detail: "v".into() },
        ];
        let mut messages = vec![
            WorkerMessage::Welcome(Welcome {
                protocol_version: PROTOCOL_VERSION,
                engine: "quickjs".into(),
                prelude_hash: PreludeHash::of("p"),
                codemode_prelude_hash: PreludeHash::of("c"),
                confinement: Confinement::Seatbelt,
            }),
            WorkerMessage::HostCall(HostCall {
                position: 3,
                kind: CallKind::Op {
                    module: "m".into(),
                    op: "o".into(),
                },
                args: text("[1,2]"),
            }),
            WorkerMessage::Blocked {
                awaiting: vec![1, 5],
            },
            WorkerMessage::Refused(Refusal::UnexpectedFrame {
                received: MessageKind::Deliver,
                state: WorkerState::Idle,
            }),
            WorkerMessage::Refused(Refusal::Oversized {
                declared: 1 << 40,
                max: MAX_FRAME_BYTES as u64,
            }),
        ];
        for result in [
            ActivationResult::Completed { value: text("42") },
            ActivationResult::Suspended { awaited: vec![6] },
            ActivationResult::Stalled,
            ActivationResult::BudgetExhausted(BudgetKind::Stack),
        ] {
            messages.push(WorkerMessage::Finished {
                activation_id: 1,
                result,
            });
        }
        for f in failures {
            messages.push(WorkerMessage::Finished {
                activation_id: 2,
                result: ActivationResult::Failed(f),
            });
        }
        for m in messages {
            let frame = encode_worker_frame(&m).expect("encodes");
            let back = read_worker_message(&mut frame.as_slice()).expect("decodes");
            assert_eq!(back, m);
        }
    }

    fn confinement_welcome(confinement: Confinement) -> WorkerMessage {
        WorkerMessage::Welcome(Welcome {
            protocol_version: PROTOCOL_VERSION,
            engine: String::new(),
            prelude_hash: PreludeHash([0; 32]),
            codemode_prelude_hash: PreludeHash([0; 32]),
            confinement,
        })
    }

    fn confinement_payload(suffix: &[u8]) -> Vec<u8> {
        // A version-4 Welcome with an empty engine name and two zero hashes.
        let mut payload = vec![101, 0, 0, 0, 4, 0, 0, 0, 0];
        payload.extend_from_slice(&[0; 64]);
        payload.extend_from_slice(suffix);
        payload
    }

    #[test]
    fn linux_confinement_round_trips() {
        for seccomp in [false, true] {
            for landlock in [
                None,
                Some(LandlockReport {
                    runtime_abi: LANDLOCK_ABI + 2,
                    applied_abi: LANDLOCK_ABI,
                }),
            ] {
                let message = confinement_welcome(Confinement::Linux { seccomp, landlock });
                let frame = encode_worker_frame(&message).expect("encodes");
                let back = read_worker_message(&mut frame.as_slice()).expect("decodes");
                assert_eq!(back, message);
            }
        }
    }

    #[test]
    fn confinement_wire_tags_and_linux_fields_are_stable() {
        let cases: &[(Confinement, &[u8])] = &[
            (Confinement::None, &[0]),
            (Confinement::Seatbelt, &[1]),
            (
                Confinement::Linux {
                    seccomp: false,
                    landlock: None,
                },
                &[2, 0, 0],
            ),
            (
                Confinement::Linux {
                    seccomp: true,
                    landlock: None,
                },
                &[2, 1, 0],
            ),
            (
                Confinement::Linux {
                    seccomp: true,
                    landlock: Some(LandlockReport {
                        runtime_abi: 11,
                        applied_abi: 9,
                    }),
                },
                &[2, 1, 1, 0, 0, 0, 11, 0, 0, 0, 9],
            ),
        ];
        for &(confinement, suffix) in cases {
            let message = confinement_welcome(confinement);
            let payload = confinement_payload(suffix);
            let frame = encode_worker_frame(&message).expect("encodes");
            assert_eq!(&frame[4..], payload);
            assert_eq!(decode_worker_payload(&payload), Ok(message));
        }
    }

    #[test]
    fn unknown_confinement_tags_are_refused() {
        for (suffix, field) in [
            (&[99][..], "confinement"),
            (&[2, 99, 0][..], "seccomp"),
            (&[2, 1, 99][..], "landlock"),
        ] {
            assert_eq!(
                decode_worker_payload(&confinement_payload(suffix)),
                Err(DecodeError::UnknownTag { field, tag: 99 })
            );
        }
    }

    #[test]
    fn truncated_linux_confinement_is_refused() {
        for (suffix, field) in [
            (&[2][..], "seccomp"),
            (&[2, 1][..], "landlock"),
            (&[2, 1, 1][..], "runtime ABI"),
            (&[2, 1, 1, 0, 0, 0, 9][..], "applied ABI"),
        ] {
            assert!(matches!(
                decode_worker_payload(&confinement_payload(suffix)),
                Err(DecodeError::Truncated { field: actual, .. }) if actual == field
            ));
        }
    }

    #[test]
    fn script_host_rejection_is_additive_and_pins_its_position_on_the_wire() {
        let legacy = WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Failed(Failure::Script {
                message: "no".into(),
            }),
        };
        let host = WorkerMessage::Finished {
            activation_id: 1,
            result: ActivationResult::Failed(Failure::ScriptHostRejection {
                position: 7,
                message: "no".into(),
            }),
        };
        let legacy_bytes = [
            0, 0, 0, 17, 104, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 2, b'n', b'o',
        ];
        let host_bytes = [
            0, 0, 0, 25, 104, 0, 0, 0, 0, 0, 0, 0, 1, 1, 11, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0, 2,
            b'n', b'o',
        ];
        assert_eq!(encode_worker_frame(&legacy).unwrap(), legacy_bytes);
        assert_eq!(encode_worker_frame(&host).unwrap(), host_bytes);
        assert_eq!(
            read_worker_message(&mut host_bytes.as_slice()).unwrap(),
            host
        );
        assert_eq!(
            read_worker_message(&mut legacy_bytes.as_slice()).unwrap(),
            legacy
        );
    }

    #[test]
    fn oversized_header_is_refused_without_reading_the_payload() {
        let header = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        match read_frame(&mut header.as_slice()) {
            Err(FrameError::Oversized { declared, max }) => {
                assert_eq!(declared, MAX_FRAME_BYTES as u64 + 1);
                assert_eq!(max, MAX_FRAME_BYTES as u64);
            }
            other => panic!("expected an oversized refusal, got {other:?}"),
        }
    }

    #[test]
    fn truncated_frames_and_payloads_are_typed_errors() {
        assert!(matches!(
            read_frame(&mut [0u8, 0].as_slice()),
            Err(FrameError::Truncated { .. })
        ));
        assert!(matches!(
            read_frame(&mut [0u8, 0, 0, 9, 1].as_slice()),
            Err(FrameError::Truncated { .. })
        ));
        assert!(matches!(
            read_frame(&mut [].as_slice()),
            Err(FrameError::Closed)
        ));
        // A Deliver frame cut short inside its value.
        let full = encode_parent_frame(&ParentMessage::Deliver(Outcome {
            position: 1,
            settlement: Settlement::Fulfilled,
            value: text("[1,2,3]"),
            delivery_order: 0,
        }))
        .expect("encodes");
        let payload = &full[4..full.len() - 3];
        assert!(matches!(
            decode_parent_payload(payload),
            Err(DecodeError::Truncated { .. })
        ));
    }

    #[test]
    fn unknown_tags_and_trailing_bytes_are_refused() {
        assert_eq!(
            decode_parent_payload(&[99]),
            Err(DecodeError::UnknownTag {
                field: "parent message",
                tag: 99
            })
        );
        // A worker message sent to the worker is not a parent message.
        let welcome =
            encode_worker_frame(&WorkerMessage::Blocked { awaiting: vec![] }).expect("encodes");
        assert!(matches!(
            decode_parent_payload(&welcome[4..]),
            Err(DecodeError::UnknownTag { .. })
        ));
        assert_eq!(
            decode_parent_payload(&[5, 0]),
            Err(DecodeError::TrailingBytes { count: 1 })
        );
    }

    #[test]
    fn values_over_the_cap_are_refused_before_parsing() {
        assert!(JsonText::new("x".repeat(MAX_VALUE_BYTES + 1)).is_err());
        // Hand-build a Deliver whose value declares one byte over the cap.
        let mut payload = vec![3u8];
        payload.extend_from_slice(&1u64.to_be_bytes());
        payload.push(0);
        payload.extend_from_slice(&((MAX_VALUE_BYTES + 1) as u32).to_be_bytes());
        assert!(matches!(
            decode_parent_payload(&payload),
            Err(DecodeError::TooLong {
                field: "delivered value",
                ..
            })
        ));
    }

    #[test]
    fn invalid_utf8_is_refused() {
        let mut payload = vec![3u8];
        payload.extend_from_slice(&1u64.to_be_bytes());
        payload.push(0);
        payload.extend_from_slice(&2u32.to_be_bytes());
        payload.extend_from_slice(&[0xff, 0xfe]);
        payload.extend_from_slice(&0u64.to_be_bytes());
        assert_eq!(
            decode_parent_payload(&payload),
            Err(DecodeError::InvalidUtf8 {
                field: "delivered value"
            })
        );
    }

    #[test]
    fn synchronous_kinds_are_exactly_clock_and_random() {
        for p in Primitive::ALL {
            let sync = CallKind::Primitive(p).is_synchronous();
            assert_eq!(
                sync,
                matches!(p, Primitive::Now | Primitive::Random),
                "{p:?}"
            );
            assert_eq!(Primitive::from_code(p.code()), Some(p));
        }
        assert!(
            !CallKind::Op {
                module: "sh".into(),
                op: "sh".into()
            }
            .is_synchronous()
        );
    }
}
