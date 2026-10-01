//! Encoding and decoding of every message.

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::limits::{
    MAX_DETAIL_BYTES, MAX_LIST_ENTRIES, MAX_NAME_BYTES, MAX_SCRIPT_BYTES, MAX_VALUE_BYTES,
};
use crate::types::*;

// Parent and worker tags live in disjoint ranges, so a frame sent in the
// wrong direction is refused as an unknown tag instead of being misread.
const TAG_HELLO: u8 = 1;
const TAG_ACTIVATE: u8 = 2;
const TAG_DELIVER: u8 = 3;
const TAG_LONG_RUNNING: u8 = 4;
const TAG_SHUTDOWN: u8 = 5;

const TAG_WELCOME: u8 = 101;
const TAG_HOST_CALL: u8 = 102;
const TAG_BLOCKED: u8 = 103;
const TAG_FINISHED: u8 = 104;
const TAG_REFUSED: u8 = 105;

/// Truncates a detail string to the decoder's limit, so an encoder never
/// produces a frame its own peer would refuse just for a long message.
fn detail(enc: &mut Encoder, text: &str) {
    if text.len() <= MAX_DETAIL_BYTES {
        enc.str(text);
    } else {
        let mut end = MAX_DETAIL_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        enc.str(&text[..end]);
    }
}

fn value(dec: &mut Decoder<'_>, field: &'static str) -> Result<JsonText, DecodeError> {
    let text = dec.string(field, MAX_VALUE_BYTES)?;
    JsonText::new(text).map_err(|e| DecodeError::TooLong {
        field,
        len: e.bytes,
        max: e.cap,
    })
}

fn positions(enc: &mut Encoder, list: &[u64]) {
    enc.count(list.len());
    for p in list {
        enc.u64(*p);
    }
}

fn read_positions(dec: &mut Decoder<'_>, field: &'static str) -> Result<Vec<u64>, DecodeError> {
    let n = dec.count(field, MAX_LIST_ENTRIES)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dec.u64(field)?);
    }
    Ok(out)
}

fn profile(enc: &mut Encoder, p: Profile) {
    enc.u8(match p {
        Profile::Flow => 0,
        Profile::Codemode => 1,
    });
}

fn read_profile(dec: &mut Decoder<'_>) -> Result<Profile, DecodeError> {
    match dec.u8("profile")? {
        0 => Ok(Profile::Flow),
        1 => Ok(Profile::Codemode),
        tag => Err(DecodeError::UnknownTag {
            field: "profile",
            tag,
        }),
    }
}

fn call_kind(enc: &mut Encoder, kind: &CallKind) {
    match kind {
        CallKind::Op { module, op } => {
            enc.u8(0);
            enc.str(module);
            enc.str(op);
        }
        CallKind::Primitive(p) => enc.u8(p.code()),
    }
}

fn read_call_kind(dec: &mut Decoder<'_>) -> Result<CallKind, DecodeError> {
    match dec.u8("call kind")? {
        0 => Ok(CallKind::Op {
            module: dec.string("module", MAX_NAME_BYTES)?,
            op: dec.string("op", MAX_NAME_BYTES)?,
        }),
        code => {
            Primitive::from_code(code)
                .map(CallKind::Primitive)
                .ok_or(DecodeError::UnknownTag {
                    field: "call kind",
                    tag: code,
                })
        }
    }
}

fn settlement(enc: &mut Encoder, s: Settlement) {
    enc.u8(match s {
        Settlement::Fulfilled => 0,
        Settlement::Rejected => 1,
    });
}

fn read_settlement(dec: &mut Decoder<'_>) -> Result<Settlement, DecodeError> {
    match dec.u8("settlement")? {
        0 => Ok(Settlement::Fulfilled),
        1 => Ok(Settlement::Rejected),
        tag => Err(DecodeError::UnknownTag {
            field: "settlement",
            tag,
        }),
    }
}

fn signature(enc: &mut Encoder, s: &CallSignature) {
    call_kind(enc, &s.kind);
    enc.fixed(&s.args_digest.0);
}

fn read_signature(dec: &mut Decoder<'_>) -> Result<CallSignature, DecodeError> {
    Ok(CallSignature {
        kind: read_call_kind(dec)?,
        args_digest: ArgsDigest(dec.fixed32("args digest")?),
    })
}

fn budgets(enc: &mut Encoder, b: &Budgets) {
    enc.u64(b.js_time_micros);
    enc.u64(b.memory_bytes);
    enc.u64(b.stack_bytes);
    enc.u32(b.max_value_bytes);
}

fn read_budgets(dec: &mut Decoder<'_>) -> Result<Budgets, DecodeError> {
    Ok(Budgets {
        js_time_micros: dec.u64("js time budget")?,
        memory_bytes: dec.u64("memory budget")?,
        stack_bytes: dec.u64("stack budget")?,
        max_value_bytes: dec.u32("value cap")?,
    })
}

fn recorded_call(enc: &mut Encoder, c: &RecordedCall) {
    enc.u64(c.position);
    call_kind(enc, &c.kind);
    enc.fixed(&c.args_digest.0);
    enc.option(c.outcome.as_ref(), |enc, o| {
        settlement(enc, o.settlement);
        enc.str(o.value.as_str());
        enc.u64(o.delivery_order);
    });
}

fn read_recorded_call(dec: &mut Decoder<'_>) -> Result<RecordedCall, DecodeError> {
    Ok(RecordedCall {
        position: dec.u64("position")?,
        kind: read_call_kind(dec)?,
        args_digest: ArgsDigest(dec.fixed32("args digest")?),
        outcome: dec.option("outcome", |dec| {
            Ok(RecordedOutcome {
                settlement: read_settlement(dec)?,
                value: value(dec, "recorded value")?,
                delivery_order: dec.u64("delivery order")?,
            })
        })?,
    })
}

fn nondeterminism(enc: &mut Encoder, n: &Nondeterminism) {
    match n {
        Nondeterminism::Divergence {
            position,
            recorded,
            observed,
        } => {
            enc.u8(0);
            enc.u64(*position);
            signature(enc, recorded);
            signature(enc, observed);
        }
        Nondeterminism::UnreleasedOutcome {
            position,
            delivery_order,
        } => {
            enc.u8(1);
            enc.u64(*position);
            enc.u64(*delivery_order);
        }
        Nondeterminism::UnconsumedCall { position } => {
            enc.u8(2);
            enc.u64(*position);
        }
    }
}

fn read_nondeterminism(dec: &mut Decoder<'_>) -> Result<Nondeterminism, DecodeError> {
    match dec.u8("nondeterminism")? {
        0 => Ok(Nondeterminism::Divergence {
            position: dec.u64("position")?,
            recorded: read_signature(dec)?,
            observed: read_signature(dec)?,
        }),
        1 => Ok(Nondeterminism::UnreleasedOutcome {
            position: dec.u64("position")?,
            delivery_order: dec.u64("delivery order")?,
        }),
        2 => Ok(Nondeterminism::UnconsumedCall {
            position: dec.u64("position")?,
        }),
        tag => Err(DecodeError::UnknownTag {
            field: "nondeterminism",
            tag,
        }),
    }
}

fn failure(enc: &mut Encoder, f: &Failure) {
    match f {
        Failure::Script { message } => {
            enc.u8(0);
            detail(enc, message);
        }
        Failure::Nondeterminism(n) => {
            enc.u8(1);
            nondeterminism(enc, n);
        }
        Failure::ProfileViolation { kind } => {
            enc.u8(2);
            call_kind(enc, kind);
        }
        Failure::EngineMismatch { expected, actual } => {
            enc.u8(3);
            enc.fixed(&expected.0);
            enc.fixed(&actual.0);
        }
        Failure::InvalidRequest { detail: d } => {
            enc.u8(4);
            detail(enc, d);
        }
        Failure::ArgumentsTooLarge { bytes, cap } => {
            enc.u8(5);
            enc.u64(*bytes);
            enc.u64(*cap);
        }
        Failure::ResultTooLarge { bytes, cap } => {
            enc.u8(6);
            enc.u64(*bytes);
            enc.u64(*cap);
        }
        Failure::ResultNotSerializable { detail: d } => {
            enc.u8(7);
            detail(enc, d);
        }
        Failure::InvalidHostValue {
            position,
            detail: d,
        } => {
            enc.u8(8);
            enc.u64(*position);
            detail(enc, d);
        }
        Failure::HostLink { detail: d } => {
            enc.u8(9);
            detail(enc, d);
        }
        Failure::Engine { detail: d } => {
            enc.u8(10);
            detail(enc, d);
        }
    }
}

fn read_failure(dec: &mut Decoder<'_>) -> Result<Failure, DecodeError> {
    let text = |dec: &mut Decoder<'_>| dec.string("detail", MAX_DETAIL_BYTES);
    match dec.u8("failure")? {
        0 => Ok(Failure::Script {
            message: text(dec)?,
        }),
        1 => Ok(Failure::Nondeterminism(read_nondeterminism(dec)?)),
        2 => Ok(Failure::ProfileViolation {
            kind: read_call_kind(dec)?,
        }),
        3 => Ok(Failure::EngineMismatch {
            expected: PreludeHash(dec.fixed32("expected prelude")?),
            actual: PreludeHash(dec.fixed32("actual prelude")?),
        }),
        4 => Ok(Failure::InvalidRequest { detail: text(dec)? }),
        5 => Ok(Failure::ArgumentsTooLarge {
            bytes: dec.u64("bytes")?,
            cap: dec.u64("cap")?,
        }),
        6 => Ok(Failure::ResultTooLarge {
            bytes: dec.u64("bytes")?,
            cap: dec.u64("cap")?,
        }),
        7 => Ok(Failure::ResultNotSerializable { detail: text(dec)? }),
        8 => Ok(Failure::InvalidHostValue {
            position: dec.u64("position")?,
            detail: text(dec)?,
        }),
        9 => Ok(Failure::HostLink { detail: text(dec)? }),
        10 => Ok(Failure::Engine { detail: text(dec)? }),
        tag => Err(DecodeError::UnknownTag {
            field: "failure",
            tag,
        }),
    }
}

fn result(enc: &mut Encoder, r: &ActivationResult) {
    match r {
        ActivationResult::Completed { value } => {
            enc.u8(0);
            enc.str(value.as_str());
        }
        ActivationResult::Failed(f) => {
            enc.u8(1);
            failure(enc, f);
        }
        ActivationResult::Suspended { awaited } => {
            enc.u8(2);
            positions(enc, awaited);
        }
        ActivationResult::Stalled => enc.u8(3),
        ActivationResult::BudgetExhausted(kind) => {
            enc.u8(4);
            enc.u8(match kind {
                BudgetKind::JsTime => 0,
                BudgetKind::Memory => 1,
                BudgetKind::Stack => 2,
            });
        }
    }
}

fn read_result(dec: &mut Decoder<'_>) -> Result<ActivationResult, DecodeError> {
    match dec.u8("activation result")? {
        0 => Ok(ActivationResult::Completed {
            value: value(dec, "result value")?,
        }),
        1 => Ok(ActivationResult::Failed(read_failure(dec)?)),
        2 => Ok(ActivationResult::Suspended {
            awaited: read_positions(dec, "awaited")?,
        }),
        3 => Ok(ActivationResult::Stalled),
        4 => match dec.u8("budget kind")? {
            0 => Ok(ActivationResult::BudgetExhausted(BudgetKind::JsTime)),
            1 => Ok(ActivationResult::BudgetExhausted(BudgetKind::Memory)),
            2 => Ok(ActivationResult::BudgetExhausted(BudgetKind::Stack)),
            tag => Err(DecodeError::UnknownTag {
                field: "budget kind",
                tag,
            }),
        },
        tag => Err(DecodeError::UnknownTag {
            field: "activation result",
            tag,
        }),
    }
}

fn message_kind(enc: &mut Encoder, k: MessageKind) {
    enc.u8(match k {
        MessageKind::Hello => TAG_HELLO,
        MessageKind::Activate => TAG_ACTIVATE,
        MessageKind::Deliver => TAG_DELIVER,
        MessageKind::LongRunning => TAG_LONG_RUNNING,
        MessageKind::Shutdown => TAG_SHUTDOWN,
    });
}

fn read_message_kind(dec: &mut Decoder<'_>) -> Result<MessageKind, DecodeError> {
    match dec.u8("message kind")? {
        TAG_HELLO => Ok(MessageKind::Hello),
        TAG_ACTIVATE => Ok(MessageKind::Activate),
        TAG_DELIVER => Ok(MessageKind::Deliver),
        TAG_LONG_RUNNING => Ok(MessageKind::LongRunning),
        TAG_SHUTDOWN => Ok(MessageKind::Shutdown),
        tag => Err(DecodeError::UnknownTag {
            field: "message kind",
            tag,
        }),
    }
}

fn refusal(enc: &mut Encoder, r: &Refusal) {
    match r {
        Refusal::Malformed { detail: d } => {
            enc.u8(0);
            detail(enc, d);
        }
        Refusal::Oversized { declared, max } => {
            enc.u8(1);
            enc.u64(*declared);
            enc.u64(*max);
        }
        Refusal::UnexpectedFrame { received, state } => {
            enc.u8(2);
            message_kind(enc, *received);
            enc.u8(match state {
                WorkerState::AwaitingHello => 0,
                WorkerState::Idle => 1,
                WorkerState::AwaitingSyncReply => 2,
                WorkerState::Blocked => 3,
            });
        }
        Refusal::VersionMismatch {
            supported,
            requested,
        } => {
            enc.u8(3);
            enc.u32(*supported);
            enc.u32(*requested);
        }
        Refusal::UnknownPosition { position } => {
            enc.u8(4);
            enc.u64(*position);
        }
        Refusal::DeliveryOrderRegression { last, received } => {
            enc.u8(5);
            enc.u64(*last);
            enc.u64(*received);
        }
    }
}

fn read_refusal(dec: &mut Decoder<'_>) -> Result<Refusal, DecodeError> {
    match dec.u8("refusal")? {
        0 => Ok(Refusal::Malformed {
            detail: dec.string("detail", MAX_DETAIL_BYTES)?,
        }),
        1 => Ok(Refusal::Oversized {
            declared: dec.u64("declared")?,
            max: dec.u64("max")?,
        }),
        2 => Ok(Refusal::UnexpectedFrame {
            received: read_message_kind(dec)?,
            state: match dec.u8("worker state")? {
                0 => WorkerState::AwaitingHello,
                1 => WorkerState::Idle,
                2 => WorkerState::AwaitingSyncReply,
                3 => WorkerState::Blocked,
                tag => {
                    return Err(DecodeError::UnknownTag {
                        field: "worker state",
                        tag,
                    });
                }
            },
        }),
        3 => Ok(Refusal::VersionMismatch {
            supported: dec.u32("supported")?,
            requested: dec.u32("requested")?,
        }),
        4 => Ok(Refusal::UnknownPosition {
            position: dec.u64("position")?,
        }),
        5 => Ok(Refusal::DeliveryOrderRegression {
            last: dec.u64("last")?,
            received: dec.u64("received")?,
        }),
        tag => Err(DecodeError::UnknownTag {
            field: "refusal",
            tag,
        }),
    }
}

pub(crate) fn encode_parent(enc: &mut Encoder, m: &ParentMessage) {
    match m {
        ParentMessage::Hello { protocol_version } => {
            enc.u8(TAG_HELLO);
            enc.u32(*protocol_version);
        }
        ParentMessage::Activate(req) => {
            enc.u8(TAG_ACTIVATE);
            enc.u64(req.activation_id);
            profile(enc, req.profile);
            enc.fixed(&req.prelude_hash.0);
            enc.str(&req.script);
            enc.str(req.trigger.as_str());
            budgets(enc, &req.budgets);
            enc.count(req.prefix.len());
            for call in &req.prefix {
                recorded_call(enc, call);
            }
        }
        ParentMessage::Deliver(o) => {
            enc.u8(TAG_DELIVER);
            enc.u64(o.position);
            settlement(enc, o.settlement);
            enc.str(o.value.as_str());
            enc.u64(o.delivery_order);
        }
        ParentMessage::LongRunning { positions: list } => {
            enc.u8(TAG_LONG_RUNNING);
            positions(enc, list);
        }
        ParentMessage::Shutdown => enc.u8(TAG_SHUTDOWN),
    }
}

pub(crate) fn decode_parent(dec: &mut Decoder<'_>) -> Result<ParentMessage, DecodeError> {
    match dec.u8("message tag")? {
        TAG_HELLO => Ok(ParentMessage::Hello {
            protocol_version: dec.u32("protocol version")?,
        }),
        TAG_ACTIVATE => {
            let activation_id = dec.u64("activation id")?;
            let profile = read_profile(dec)?;
            let prelude_hash = PreludeHash(dec.fixed32("prelude hash")?);
            let script = dec.string("script", MAX_SCRIPT_BYTES)?;
            let trigger = value(dec, "trigger")?;
            let budgets = read_budgets(dec)?;
            let n = dec.count("prefix", MAX_LIST_ENTRIES)?;
            let mut prefix = Vec::with_capacity(n);
            for _ in 0..n {
                prefix.push(read_recorded_call(dec)?);
            }
            Ok(ParentMessage::Activate(Box::new(ActivationRequest {
                activation_id,
                profile,
                prelude_hash,
                script,
                trigger,
                budgets,
                prefix,
            })))
        }
        TAG_DELIVER => Ok(ParentMessage::Deliver(Outcome {
            position: dec.u64("position")?,
            settlement: read_settlement(dec)?,
            value: value(dec, "delivered value")?,
            delivery_order: dec.u64("delivery order")?,
        })),
        TAG_LONG_RUNNING => Ok(ParentMessage::LongRunning {
            positions: read_positions(dec, "long-running positions")?,
        }),
        TAG_SHUTDOWN => Ok(ParentMessage::Shutdown),
        tag => Err(DecodeError::UnknownTag {
            field: "parent message",
            tag,
        }),
    }
}

pub(crate) fn encode_worker(enc: &mut Encoder, m: &WorkerMessage) {
    match m {
        WorkerMessage::Welcome(w) => {
            enc.u8(TAG_WELCOME);
            enc.u32(w.protocol_version);
            detail(enc, &w.engine);
            enc.fixed(&w.prelude_hash.0);
            enc.u8(match w.confinement {
                Confinement::None => 0,
                Confinement::Seatbelt => 1,
            });
        }
        WorkerMessage::HostCall(c) => {
            enc.u8(TAG_HOST_CALL);
            enc.u64(c.position);
            call_kind(enc, &c.kind);
            enc.str(c.args.as_str());
        }
        WorkerMessage::Blocked { awaiting } => {
            enc.u8(TAG_BLOCKED);
            positions(enc, awaiting);
        }
        WorkerMessage::Finished {
            activation_id,
            result: r,
        } => {
            enc.u8(TAG_FINISHED);
            enc.u64(*activation_id);
            result(enc, r);
        }
        WorkerMessage::Refused(r) => {
            enc.u8(TAG_REFUSED);
            refusal(enc, r);
        }
    }
}

pub(crate) fn decode_worker(dec: &mut Decoder<'_>) -> Result<WorkerMessage, DecodeError> {
    match dec.u8("message tag")? {
        TAG_WELCOME => Ok(WorkerMessage::Welcome(Welcome {
            protocol_version: dec.u32("protocol version")?,
            engine: dec.string("engine", MAX_DETAIL_BYTES)?,
            prelude_hash: PreludeHash(dec.fixed32("prelude hash")?),
            confinement: match dec.u8("confinement")? {
                0 => Confinement::None,
                1 => Confinement::Seatbelt,
                tag => {
                    return Err(DecodeError::UnknownTag {
                        field: "confinement",
                        tag,
                    });
                }
            },
        })),
        TAG_HOST_CALL => Ok(WorkerMessage::HostCall(HostCall {
            position: dec.u64("position")?,
            kind: read_call_kind(dec)?,
            args: value(dec, "arguments")?,
        })),
        TAG_BLOCKED => Ok(WorkerMessage::Blocked {
            awaiting: read_positions(dec, "awaiting")?,
        }),
        TAG_FINISHED => Ok(WorkerMessage::Finished {
            activation_id: dec.u64("activation id")?,
            result: read_result(dec)?,
        }),
        TAG_REFUSED => Ok(WorkerMessage::Refused(read_refusal(dec)?)),
        tag => Err(DecodeError::UnknownTag {
            field: "worker message",
            tag,
        }),
    }
}
