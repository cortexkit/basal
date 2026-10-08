//! One activation: a fresh QuickJS runtime and context, the lockdown prelude,
//! the script, replay of the recorded prefix, and new calls through the
//! host link.
//!
//! The activation is a loop the worker drives from Rust. It runs every
//! pending JavaScript job, then hands the VM the next recorded outcome in
//! delivery order; once the recorded prefix is exhausted it asks the parent
//! for the next outcome. JavaScript never waits inside the engine: a script
//! blocked on a host call simply has no jobs to run, and the worker decides
//! whether that is a wait, a suspension or a stall.

// Errors here are whole activation results, returned once per activation or
// per host call; boxing them would buy nothing measurable.
#![allow(clippy::result_large_err)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use basal_proto::{
    ActivationRequest, ActivationResult, ArgsDigest, BudgetKind, CallKind, CallSignature, Failure,
    HostCall, JsonText, MAX_DETAIL_BYTES, MAX_NAME_BYTES, MAX_VALUE_BYTES, Nondeterminism,
    PreludeHash, Primitive, Profile, RecordedCall, Settlement,
};
use rquickjs::context::{EvalOptions, intrinsic};
use rquickjs::{Array, Context, Ctx, Exception, Function, Object, Persistent, Runtime, Value, qjs};

use crate::clock::JsClock;
use crate::harden::harden;
use crate::link::{HostLink, WaitReply};

/// The lockdown prelude, embedded so a worker binary has exactly one.
pub const PRELUDE: &str = include_str!("prelude.js");

/// The engine and binding, reported in the handshake as part of a run's
/// runtime fingerprint. The binding is pinned to an exact version in the
/// workspace manifest, which pins the bundled engine with it.
pub const ENGINE: &str = "quickjs-ng 0.16.2 via rquickjs 0.14.0";

pub fn prelude_hash() -> PreludeHash {
    static HASH: OnceLock<PreludeHash> = OnceLock::new();
    *HASH.get_or_init(|| PreludeHash::of(PRELUDE))
}

/// The intrinsics a context gets. `Performance`, `WeakRef` and
/// `DOMException` are never created; the prelude deletes the other
/// forbidden globals (eval, Function, timers, Atomics, Intl and so on). `Eval` is the engine's ability to compile source at
/// all, which the worker needs for the prelude and the script; the prelude
/// removes every route by which the script could reach it.
type Intrinsics = (
    intrinsic::Date,
    intrinsic::Eval,
    intrinsic::RegExpCompiler,
    intrinsic::RegExp,
    intrinsic::Json,
    intrinsic::Proxy,
    intrinsic::MapSet,
    intrinsic::TypedArrays,
    intrinsic::Promise,
);

const MIN_MEMORY_BYTES: u64 = 64 * 1024;
const MAX_MEMORY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MIN_STACK_BYTES: u64 = 16 * 1024;
// The worker runs activations on its main thread, whose stack is 8 MiB on
// macOS. QuickJS's limit must stay well inside the real stack, or deep
// recursion would overflow the thread before the engine noticed.
const MAX_STACK_BYTES: u64 = 4 * 1024 * 1024;
// Positions cross into JavaScript as numbers, which are exact up to 2^53.
const MAX_POSITION: u64 = 1 << 53;

/// A recorded asynchronous outcome waiting to be released.
#[derive(Debug, Clone, Copy)]
struct Release {
    order: u64,
    position: u64,
}

/// Replay and issue state, shared between the driving loop and the native
/// functions the prelude calls.
struct Bridge {
    profile: Profile,
    max_value: usize,
    prefix: Vec<RecordedCall>,
    /// Recorded outcomes of asynchronous calls, in delivery order.
    release: VecDeque<Release>,
    /// Recorded outcomes of synchronous calls not yet consumed, as
    /// (delivery order, position).
    pending_sync: BTreeSet<(u64, u64)>,
    next_position: u64,
    /// Asynchronous calls issued and not yet settled.
    outstanding: BTreeSet<u64>,
    /// Outstanding calls the parent reported as long-running.
    long_running: BTreeSet<u64>,
    last_order: Option<u64>,
    /// Set when a native call ends the activation (a profile violation, a
    /// divergence, a broken link). Checked after every engine entry.
    halt: Option<ActivationResult>,
}

struct Shared {
    clock: JsClock,
    /// Mirrors `bridge.halt.is_some()` for the interrupt handler, which must
    /// not borrow the bridge.
    halted: Cell<bool>,
    bridge: RefCell<Bridge>,
    link: Rc<RefCell<dyn HostLink>>,
    memory_exhausted: Rc<Cell<bool>>,
}

enum Issued {
    Async(u64),
    Sync { fulfilled: bool, value: JsonText },
}

enum Prepared {
    Done(Issued),
    Announce(HostCall),
    AskSync(HostCall, Option<u64>),
}

fn failed(failure: Failure) -> ActivationResult {
    ActivationResult::Failed(failure)
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_DETAIL_BYTES {
        let end = basal_proto::utf8_prefix(&text, MAX_DETAIL_BYTES).len();
        text.truncate(end);
    }
    text
}

/// Whether `text` is a JSON number. Used for clock and random values, which
/// the prelude hands straight to arithmetic.
fn json_number(text: &str) -> Option<f64> {
    let t = text.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'));
    let bytes = t.as_bytes();
    let mut i = 0;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            while matches!(bytes.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        _ => return None,
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while matches!(bytes.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let start = i;
        while matches!(bytes.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    if i != bytes.len() {
        return None;
    }
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

impl Shared {
    fn halt(&self, result: ActivationResult) {
        let mut bridge = self.bridge.borrow_mut();
        if bridge.halt.is_none() {
            bridge.halt = Some(result);
        }
        self.halted.set(true);
    }

    fn take_halt(&self) -> Option<ActivationResult> {
        self.bridge.borrow_mut().halt.take()
    }

    /// Allocates the next position for a call and decides how it is served:
    /// from the recorded prefix, by announcing it to the parent, or (for a
    /// synchronous call) by asking the parent and waiting.
    fn prepare(&self, kind: CallKind, args: String) -> Result<Prepared, ActivationResult> {
        let mut bridge = self.bridge.borrow_mut();
        if bridge.halt.is_some() {
            return Err(failed(Failure::Engine {
                detail: "call issued after the activation halted".into(),
            }));
        }
        // Unattended flows must never reach a shell. The prelude does not
        // install the `sh` global in the flow profile; this check refuses a
        // shell call again in case a script reaches the native bridge by some
        // other route.
        if bridge.profile == Profile::Flow && kind.is_shell() {
            return Err(failed(Failure::ProfileViolation { kind }));
        }
        if args.len() > bridge.max_value {
            return Err(failed(Failure::ArgumentsTooLarge {
                bytes: args.len() as u64,
                cap: bridge.max_value as u64,
            }));
        }
        let args = JsonText::new(args).expect("arguments were checked against the hard value cap");
        let position = bridge.next_position;
        if position >= MAX_POSITION {
            return Err(failed(Failure::Engine {
                detail: "call positions exhausted".into(),
            }));
        }
        bridge.next_position += 1;
        let digest = ArgsDigest::of(&args);
        let synchronous = kind.is_synchronous();

        let Some(recorded) = bridge.prefix.get_mut(position as usize) else {
            // A call beyond the recorded prefix. In a replay that matches the
            // journal, every recorded outcome has been released before the
            // script reaches such a call: the parent journals each call when
            // it is issued, so a call issued before some recorded delivery
            // would itself be in the prefix.
            if let Some(next) = bridge.release.front() {
                return Err(failed(Failure::Nondeterminism(
                    Nondeterminism::UnreleasedOutcome {
                        position: next.position,
                        delivery_order: next.order,
                    },
                )));
            }
            let call = HostCall {
                position,
                kind,
                args,
            };
            if synchronous {
                return Ok(Prepared::AskSync(call, bridge.last_order));
            }
            bridge.outstanding.insert(position);
            return Ok(Prepared::Announce(call));
        };

        if recorded.kind != kind || recorded.args_digest != digest {
            return Err(failed(Failure::Nondeterminism(
                Nondeterminism::Divergence {
                    position,
                    recorded: CallSignature {
                        kind: recorded.kind.clone(),
                        args_digest: recorded.args_digest,
                    },
                    observed: CallSignature {
                        kind,
                        args_digest: digest,
                    },
                },
            )));
        }
        if !synchronous {
            // Served from the prefix: its recorded outcome (if any) is
            // released in delivery order by the driving loop, and one not yet
            // recorded is still owed by the parent. Either way it does not
            // cross the channel again.
            bridge.outstanding.insert(position);
            return Ok(Prepared::Done(Issued::Async(position)));
        }
        match recorded.outcome.take() {
            Some(outcome) => {
                // A recorded clock read or random sample is answered at once,
                // which is only consistent if every asynchronous outcome
                // recorded before it has already been released.
                if let Some(next) = bridge.release.front()
                    && next.order < outcome.delivery_order
                {
                    return Err(failed(Failure::Nondeterminism(
                        Nondeterminism::UnreleasedOutcome {
                            position: next.position,
                            delivery_order: next.order,
                        },
                    )));
                }
                bridge
                    .pending_sync
                    .remove(&(outcome.delivery_order, position));
                bridge.last_order = Some(outcome.delivery_order);
                Ok(Prepared::Done(Issued::Sync {
                    fulfilled: outcome.settlement == Settlement::Fulfilled,
                    value: outcome.value,
                }))
            }
            // The call was journaled but its outcome was not (the journal
            // was cut between the two), so ask the parent for the outcome.
            None => Ok(Prepared::AskSync(
                HostCall {
                    position,
                    kind,
                    args,
                },
                bridge.last_order,
            )),
        }
    }

    /// Issues a call from the script. `Err` means the activation has halted.
    fn issue(&self, kind: CallKind, args: String) -> Result<Issued, ()> {
        let value_kind = kind.clone();
        let prepared = match self.prepare(kind, args) {
            Ok(p) => p,
            Err(result) => {
                self.halt(result);
                return Err(());
            }
        };
        let issued = match prepared {
            Prepared::Done(issued) => issued,
            Prepared::Announce(call) => {
                let sent = self.link.borrow_mut().issue(&call);
                if let Err(e) = sent {
                    self.halt(failed(Failure::HostLink { detail: e.0 }));
                    return Err(());
                }
                Issued::Async(call.position)
            }
            Prepared::AskSync(call, last_order) => {
                // Waiting on the parent is not engine time.
                self.clock.leave();
                let reply = self.link.borrow_mut().issue_sync(&call, last_order);
                self.clock.enter();
                match reply {
                    Ok(outcome) => {
                        self.bridge.borrow_mut().last_order = Some(outcome.delivery_order);
                        Issued::Sync {
                            fulfilled: outcome.settlement == Settlement::Fulfilled,
                            value: outcome.value,
                        }
                    }
                    Err(e) => {
                        self.halt(failed(Failure::HostLink { detail: e.0 }));
                        return Err(());
                    }
                }
            }
        };
        if let Issued::Sync {
            fulfilled: true,
            value,
        } = &issued
        {
            let position = self.bridge.borrow().next_position - 1;
            let valid = match (&value_kind, json_number(value.as_str())) {
                (CallKind::Primitive(Primitive::Now), Some(_)) => true,
                (CallKind::Primitive(Primitive::Random), Some(v)) => (0.0..1.0).contains(&v),
                _ => false,
            };
            if !valid {
                self.halt(failed(Failure::InvalidHostValue {
                    position,
                    detail: format!("{value_kind} must return a finite number, got {value:?}"),
                }));
                return Err(());
            }
        }
        Ok(issued)
    }
}

/// Throws an error the script cannot catch, so a halted activation stops at
/// once instead of letting the script carry on past a refused call.
fn halt_exception<'js>(ctx: &Ctx<'js>) -> rquickjs::Error {
    match Exception::from_message(ctx.clone(), "activation halted by the host") {
        Ok(exception) => {
            let value = exception.into_value();
            // SAFETY: both pointers come from live rquickjs handles for this
            // context; JS_SetUncatchableError only flips a flag on an Error
            // object and ignores anything else.
            unsafe { qjs::JS_SetUncatchableError(ctx.as_raw().as_ptr(), value.as_raw()) };
            ctx.throw(value)
        }
        Err(e) => e,
    }
}

fn primitive_from_code<'js>(ctx: &Ctx<'js>, code: i32) -> rquickjs::Result<Primitive> {
    u8::try_from(code)
        .ok()
        .and_then(Primitive::from_code)
        .ok_or_else(|| Exception::throw_type(ctx, "unknown host primitive"))
}

/// Builds the object of native entry points handed to the prelude.
fn native_bridge<'js>(ctx: &Ctx<'js>, shared: &Rc<Shared>) -> rquickjs::Result<Object<'js>> {
    let native = Object::new(ctx.clone())?;

    let s = shared.clone();
    native.set(
        "issueOp",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>,
                  module: String,
                  op: String,
                  args: String|
                  -> rquickjs::Result<f64> {
                if module.is_empty()
                    || op.is_empty()
                    || module.len() > MAX_NAME_BYTES
                    || op.len() > MAX_NAME_BYTES
                {
                    return Err(Exception::throw_range(
                        &ctx,
                        "module and op names must contain 1 to 128 bytes",
                    ));
                }
                match s.issue(CallKind::Op { module, op }, args) {
                    Ok(Issued::Async(position)) => Ok(position as f64),
                    _ => Err(halt_exception(&ctx)),
                }
            },
        )?,
    )?;

    let s = shared.clone();
    native.set(
        "issuePrimitive",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, code: i32, args: String| -> rquickjs::Result<f64> {
                let primitive = primitive_from_code(&ctx, code)?;
                let kind = CallKind::Primitive(primitive);
                if kind.is_synchronous() {
                    return Err(Exception::throw_type(
                        &ctx,
                        "synchronous primitive issued asynchronously",
                    ));
                }
                match s.issue(kind, args) {
                    Ok(Issued::Async(position)) => Ok(position as f64),
                    _ => Err(halt_exception(&ctx)),
                }
            },
        )?,
    )?;

    let s = shared.clone();
    native.set(
        "issueSync",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'js>, code: i32, args: String| -> rquickjs::Result<Array<'js>> {
                let primitive = primitive_from_code(&ctx, code)?;
                let kind = CallKind::Primitive(primitive);
                if !kind.is_synchronous() {
                    return Err(Exception::throw_type(
                        &ctx,
                        "asynchronous primitive issued synchronously",
                    ));
                }
                let position = s.bridge.borrow().next_position;
                match s.issue(kind, args) {
                    Ok(Issued::Sync { fulfilled, value }) => {
                        let reply = Array::new(ctx.clone())?;
                        reply.set(0, fulfilled)?;
                        reply.set(1, value.as_str())?;
                        reply.set(2, position as f64)?;
                        Ok(reply)
                    }
                    _ => Err(halt_exception(&ctx)),
                }
            },
        )?,
    )?;

    Ok(native)
}

/// The prelude's entry points, kept alive across engine entries.
struct Hooks {
    deliver: Persistent<Function<'static>>,
    status: Persistent<Function<'static>>,
}

enum ScriptState {
    Pending,
    Fulfilled(String),
    Rejected {
        kind: String,
        text: String,
        host_position: Option<u64>,
    },
}

/// Classifies an exception that escaped into Rust without calling its getters.
fn classify_exception<'js>(value: Value<'js>) -> ActivationResult {
    // Only native Error objects are inspected here. A proxy or an arbitrary
    // thrown object could execute code even while reading a descriptor.
    // SAFETY: value is a live handle; this only checks the engine's class tag.
    if !unsafe { qjs::JS_IsError(value.as_raw()) } {
        return failed(Failure::Script {
            message: truncate(
                value
                    .as_string()
                    .and_then(|s| s.to_string().ok())
                    .unwrap_or_else(|| format!("uncaught {}", value.type_name())),
            ),
        });
    }
    let ctx = value.ctx();
    let own_string = |key: &str| -> Option<String> {
        let key = std::ffi::CString::new(key).ok()?;
        // SAFETY: ctx and value remain live, and found descriptors transfer
        // ownership of value/getter/setter references to the caller.
        unsafe {
            let raw_ctx = ctx.as_raw().as_ptr();
            let atom = qjs::JS_NewAtom(raw_ctx, key.as_ptr());
            let mut desc = std::mem::MaybeUninit::<qjs::JSPropertyDescriptor>::uninit();
            let found = qjs::JS_GetOwnProperty(raw_ctx, desc.as_mut_ptr(), value.as_raw(), atom);
            qjs::JS_FreeAtom(raw_ctx, atom);
            if found != 1 {
                return None;
            }
            let desc = desc.assume_init();
            let data = Value::from_raw(ctx.clone(), desc.value);
            drop(Value::from_raw(ctx.clone(), desc.getter));
            drop(Value::from_raw(ctx.clone(), desc.setter));
            data.as_string().and_then(|s| s.to_string().ok())
        }
    };
    let message = own_string("message").unwrap_or_default();
    // QuickJS has no public origin flag for stack overflow. Keep the existing
    // stack contract until the engine can report that origin independently.
    let range: Option<Object> = ctx
        .globals()
        .get::<_, Object>("RangeError")
        .ok()
        .and_then(|ctor| ctor.get("prototype").ok());
    let is_range = value.as_object().and_then(|obj| obj.get_prototype()) == range;
    if is_range && message == "Maximum call stack size exceeded" {
        return ActivationResult::BudgetExhausted(BudgetKind::Stack);
    }
    let name = own_string("name").unwrap_or_else(|| "Error".into());
    let mut text = format!("{name}: {message}");
    if let Some(stack) = own_string("stack").filter(|s| !s.is_empty()) {
        text.push('\n');
        text.push_str(&stack);
    }
    failed(Failure::Script {
        message: truncate(text),
    })
}

struct Activation {
    // Field order is drop order: persistent handles must be released before
    // the context, and the context before the runtime.
    hooks: Option<Hooks>,
    ctx: Context,
    rt: Runtime,
    shared: Rc<Shared>,
    #[cfg(test)]
    expose_raw_bridge: bool,
}

/// Runs one activation to its end.
pub fn run_activation(
    request: &ActivationRequest,
    link: Rc<RefCell<dyn HostLink>>,
) -> ActivationResult {
    run(request, link, false, request.prefix.clone())
}

/// Runs an owned IPC request without copying its recorded values.
pub fn run_activation_owned(
    mut request: ActivationRequest,
    link: Rc<RefCell<dyn HostLink>>,
) -> ActivationResult {
    let prefix = std::mem::take(&mut request.prefix);
    run(&request, link, false, prefix)
}

/// Runs an activation with the raw native bridge exposed to the script as
/// `__rawBridge`, standing in for a script that has escaped the prelude.
/// Only tests can build this.
#[cfg(test)]
pub(crate) fn run_activation_with_raw_bridge(
    request: &ActivationRequest,
    link: Rc<RefCell<dyn HostLink>>,
) -> ActivationResult {
    run(request, link, true, request.prefix.clone())
}

fn run(
    request: &ActivationRequest,
    link: Rc<RefCell<dyn HostLink>>,
    #[allow(unused_variables)] expose_raw_bridge: bool,
    prefix: Vec<RecordedCall>,
) -> ActivationResult {
    let bridge = match plan(request, prefix) {
        Ok(bridge) => bridge,
        Err(result) => return result,
    };
    let shared = Rc::new(Shared {
        clock: JsClock::new(Duration::from_micros(request.budgets.js_time_micros)),
        halted: Cell::new(false),
        bridge: RefCell::new(bridge),
        link,
        memory_exhausted: Rc::new(Cell::new(false)),
    });
    let rt = match Runtime::new_with_alloc(allocator::BudgetAllocator::new(
        request.budgets.memory_bytes as usize,
        shared.memory_exhausted.clone(),
    )) {
        Ok(rt) => rt,
        Err(rquickjs::Error::Allocation) => {
            return ActivationResult::BudgetExhausted(BudgetKind::Memory);
        }
        Err(e) => {
            return failed(Failure::Engine {
                detail: format!("creating the runtime: {e}"),
            });
        }
    };
    // The budget allocator caps the bytes the engine's heap may hold at the
    // activation's memory budget, so QuickJS's own limit is turned off. A
    // refused allocation sets the `memory_exhausted` flag, which lives
    // outside the VM where no script can set it, so a memory refusal is told
    // apart from a script that merely throws an out-of-memory error.
    rt.set_memory_limit(0);
    rt.set_max_stack_size(request.budgets.stack_bytes as usize);
    let interrupt = shared.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        interrupt.halted.get() || interrupt.clock.over_budget()
    })));
    let ctx = match Context::custom::<Intrinsics>(&rt) {
        Ok(ctx) => ctx,
        Err(rquickjs::Error::Allocation) => {
            return ActivationResult::BudgetExhausted(BudgetKind::Memory);
        }
        Err(e) => {
            return failed(Failure::Engine {
                detail: format!("creating the context: {e}"),
            });
        }
    };
    let mut activation = Activation {
        hooks: None,
        ctx,
        rt,
        shared,
        #[cfg(test)]
        expose_raw_bridge,
    };
    let result = activation.drive(request);
    // Drop the VM before answering, so nothing of this activation's heap
    // outlives it in the worker.
    drop(activation);
    result
}

/// Checks the request and builds the replay state.
fn plan(
    request: &ActivationRequest,
    prefix: Vec<RecordedCall>,
) -> Result<Bridge, ActivationResult> {
    let invalid = |detail: String| Err(failed(Failure::InvalidRequest { detail }));
    let actual = prelude_hash();
    if request.prelude_hash != actual {
        return Err(failed(Failure::EngineMismatch {
            expected: request.prelude_hash,
            actual,
        }));
    }
    let b = &request.budgets;
    if b.js_time_micros == 0 {
        return invalid("the JS time budget must be positive".into());
    }
    if !(MIN_MEMORY_BYTES..=MAX_MEMORY_BYTES).contains(&b.memory_bytes) {
        return invalid(format!(
            "memory budget {} outside {MIN_MEMORY_BYTES}..={MAX_MEMORY_BYTES}",
            b.memory_bytes
        ));
    }
    if !(MIN_STACK_BYTES..=MAX_STACK_BYTES).contains(&b.stack_bytes) {
        return invalid(format!(
            "stack budget {} outside {MIN_STACK_BYTES}..={MAX_STACK_BYTES}",
            b.stack_bytes
        ));
    }
    let max_value = b.max_value_bytes as usize;
    // Even the absent result is encoded as the four-byte JSON literal null.
    if !("null".len()..=MAX_VALUE_BYTES).contains(&max_value) {
        return invalid(format!(
            "value cap {max_value} outside 4..={MAX_VALUE_BYTES}"
        ));
    }

    let mut orders = BTreeSet::new();
    let mut release = Vec::new();
    let mut pending_sync = BTreeSet::new();
    let mut last_sync_order = None;
    for (index, call) in prefix.iter().enumerate() {
        if call.position != index as u64 {
            return invalid(format!(
                "prefix entry {index} has position {}; positions must run 0, 1, 2, ...",
                call.position
            ));
        }
        if let Some(outcome) = &call.outcome {
            if !orders.insert(outcome.delivery_order) {
                return invalid(format!(
                    "delivery order {} appears twice in the prefix",
                    outcome.delivery_order
                ));
            }
            if call.kind.is_synchronous() {
                if last_sync_order.is_some_and(|last| outcome.delivery_order <= last) {
                    return invalid("synchronous delivery orders must strictly increase".into());
                }
                last_sync_order = Some(outcome.delivery_order);
                pending_sync.insert((outcome.delivery_order, call.position));
            } else {
                release.push(Release {
                    order: outcome.delivery_order,
                    position: call.position,
                });
            }
        }
    }
    release.sort_by_key(|r| r.order);

    Ok(Bridge {
        profile: request.profile,
        max_value,
        prefix,
        release: release.into(),
        pending_sync,
        next_position: 0,
        outstanding: BTreeSet::new(),
        long_running: BTreeSet::new(),
        last_order: None,
        halt: None,
    })
}

impl Activation {
    /// The result for an engine error, preferring the reason the activation
    /// was stopped over the exception that stopping it produced.
    fn engine_error(&self, error: rquickjs::Error) -> ActivationResult {
        if let Some(halt) = self.shared.take_halt() {
            return halt;
        }
        if self.shared.clock.exhausted() {
            return ActivationResult::BudgetExhausted(BudgetKind::JsTime);
        }
        if self.shared.memory_exhausted.get() {
            return ActivationResult::BudgetExhausted(BudgetKind::Memory);
        }
        match error {
            rquickjs::Error::Exception => self.ctx.with(|ctx| classify_exception(ctx.catch())),
            rquickjs::Error::Allocation => ActivationResult::BudgetExhausted(BudgetKind::Memory),
            other => failed(Failure::Engine {
                detail: truncate(other.to_string()),
            }),
        }
    }

    /// After every engine entry: did a native call or the budget end it?
    ///
    /// The budget is checked here as well as in the interrupt handler,
    /// because the engine polls its handler only every few thousand
    /// operations, and a script that does a little work between many host
    /// calls could otherwise outrun its budget between polls.
    fn interrupted(&self) -> Option<ActivationResult> {
        if let Some(halt) = self.shared.take_halt() {
            return Some(halt);
        }
        if self.shared.clock.over_budget() {
            return Some(ActivationResult::BudgetExhausted(BudgetKind::JsTime));
        }
        None
    }

    /// Runs a closure inside the engine with the JS clock running.
    fn enter<R>(
        &self,
        f: impl FnOnce(&Ctx<'_>) -> rquickjs::Result<R>,
    ) -> Result<R, ActivationResult> {
        self.shared.clock.enter();
        let result = self.ctx.with(|ctx| f(&ctx));
        self.shared.clock.leave();
        let value = result.map_err(|e| self.engine_error(e))?;
        match self.interrupted() {
            Some(halt) => Err(halt),
            None => Ok(value),
        }
    }

    /// Evaluates the prelude and compiles and starts the script.
    fn setup(&mut self, request: &ActivationRequest) -> Result<(), ActivationResult> {
        let shared = self.shared.clone();
        let codemode = request.profile == Profile::Codemode;
        #[cfg(test)]
        let expose = self.expose_raw_bridge;
        // The prelude's own work is not charged to the script's budget.
        let hooks = self.ctx.with(
            |ctx| -> rquickjs::Result<(Hooks, Persistent<Function<'static>>)> {
                let native = native_bridge(&ctx, &shared)?;
                #[cfg(test)]
                if expose {
                    ctx.globals().set("__rawBridge", native.clone())?;
                }
                let mut options = EvalOptions::default();
                options.filename = Some("prelude.js".into());
                let prelude: Function = ctx.eval_with_options(PRELUDE, options)?;
                let hooks: Object = prelude.call((
                    native,
                    codemode,
                    request.trigger.as_str(),
                    request.self_input.as_str(),
                ))?;
                let deliver: Function = hooks.get("deliver")?;
                let start: Function = hooks.get("start")?;
                let status: Function = hooks.get("status")?;
                let roots: Vec<Value> = hooks.get("roots")?;
                harden(&ctx, roots)?;
                Ok((
                    Hooks {
                        deliver: Persistent::save(&ctx, deliver),
                        status: Persistent::save(&ctx, status),
                    },
                    Persistent::save(&ctx, start),
                ))
            },
        );
        let (hooks, start) = match hooks {
            Ok(h) => h,
            Err(e) => {
                return Err(match self.engine_error(e) {
                    ActivationResult::Failed(Failure::Script { message }) => {
                        failed(Failure::Engine {
                            detail: truncate(format!("lockdown prelude failed: {message}")),
                        })
                    }
                    other => other,
                });
            }
        };
        self.hooks = Some(hooks);

        // Evaluation is an engine entry under the activation's budgets and
        // lockdown, including any top-level code accepted by the parser.
        let source = format!("(async function () {{{}\n}})", request.script);
        let started = self.enter(|ctx| {
            let start = start.clone().restore(ctx)?;
            let mut options = EvalOptions::default();
            options.filename = Some("flow.js".into());
            let main: Value = ctx.eval_with_options(source, options)?;
            if !main.is_function() {
                return Err(Exception::throw_type(
                    ctx,
                    "the script must be a function body",
                ));
            }
            start.call::<_, ()>((main,))
        });
        // Drop the start handle inside the context's lifetime either way.
        drop(start);
        started
    }

    fn deliver(
        &self,
        position: u64,
        settlement: Settlement,
        value: &JsonText,
    ) -> Result<(), ActivationResult> {
        let Some(hooks) = &self.hooks else {
            return Err(failed(Failure::Engine {
                detail: "delivery before setup".into(),
            }));
        };
        let code = self.enter(|ctx| {
            let deliver = hooks.deliver.clone().restore(ctx)?;
            deliver.call::<_, i32>((
                position as f64,
                settlement == Settlement::Fulfilled,
                value.as_str(),
            ))
        })?;
        match code {
            0 => Ok(()),
            1 => Err(failed(Failure::InvalidHostValue {
                position,
                detail: "the value is not valid JSON".into(),
            })),
            _ => Err(failed(Failure::Engine {
                detail: format!("no pending call at position {position}"),
            })),
        }
    }

    fn drain_jobs(&self) -> Result<(), ActivationResult> {
        while self.rt.is_job_pending() {
            self.shared.clock.enter();
            let ran = self.rt.execute_pending_job();
            self.shared.clock.leave();
            if ran.is_err() {
                return Err(self.engine_error(rquickjs::Error::Exception));
            }
            if let Some(halt) = self.interrupted() {
                return Err(halt);
            }
        }
        Ok(())
    }

    fn script_state(&self) -> Result<ScriptState, ActivationResult> {
        let Some(hooks) = &self.hooks else {
            return Err(failed(Failure::Engine {
                detail: "status before setup".into(),
            }));
        };
        self.enter(|ctx| {
            let status = hooks.status.clone().restore(ctx)?;
            let reply: Array = status.call(())?;
            let state: i32 = reply.get(0)?;
            let first: String = reply.get(1)?;
            let second: String = reply.get(2)?;
            let host_position: Option<u64> = reply.get(3)?;
            Ok(match state {
                0 => ScriptState::Pending,
                1 => ScriptState::Fulfilled(first),
                _ => ScriptState::Rejected {
                    kind: first,
                    text: second,
                    host_position,
                },
            })
        })
    }

    fn drive(&mut self, request: &ActivationRequest) -> ActivationResult {
        match self.drive_inner(request) {
            Ok(result) | Err(result) => result,
        }
    }

    fn drive_inner(
        &mut self,
        request: &ActivationRequest,
    ) -> Result<ActivationResult, ActivationResult> {
        self.setup(request)?;
        loop {
            self.drain_jobs()?;

            // Release the next recorded outcome, in recorded delivery order,
            // until the prefix is exhausted.
            let next = {
                let mut bridge = self.shared.bridge.borrow_mut();
                match bridge.release.front().copied() {
                    None => None,
                    Some(next) => {
                        let sync_due = bridge
                            .pending_sync
                            .first()
                            .copied()
                            .filter(|(order, _)| *order < next.order);
                        if let Some((order, position)) = sync_due {
                            return Err(failed(Failure::Nondeterminism(
                                Nondeterminism::UnreleasedOutcome {
                                    position,
                                    delivery_order: order,
                                },
                            )));
                        }
                        if !bridge.outstanding.remove(&next.position) {
                            return Err(failed(Failure::Nondeterminism(
                                Nondeterminism::UnreleasedOutcome {
                                    position: next.position,
                                    delivery_order: next.order,
                                },
                            )));
                        }
                        bridge.release.pop_front();
                        bridge.last_order = Some(next.order);
                        let outcome = bridge.prefix[next.position as usize].outcome.take();
                        Some((next.position, outcome))
                    }
                }
            };
            if let Some((position, outcome)) = next {
                let Some(outcome) = outcome else {
                    return Err(failed(Failure::Engine {
                        detail: format!("recorded outcome missing at position {position}"),
                    }));
                };
                self.deliver(position, outcome.settlement, &outcome.value)?;
                continue;
            }

            // The prefix is exhausted. A faithful replay has by now issued
            // every recorded call.
            {
                let bridge = self.shared.bridge.borrow();
                if (bridge.next_position as usize) < bridge.prefix.len() {
                    return Err(failed(Failure::Nondeterminism(
                        Nondeterminism::UnconsumedCall {
                            position: bridge.next_position,
                        },
                    )));
                }
            }

            let state = self.script_state()?;
            let (outstanding, all_long) = {
                let bridge = self.shared.bridge.borrow();
                let outstanding: Vec<u64> = bridge.outstanding.iter().copied().collect();
                let all_long = outstanding.iter().all(|p| bridge.long_running.contains(p));
                (outstanding, all_long)
            };
            match state {
                ScriptState::Rejected {
                    kind,
                    text,
                    host_position,
                } if outstanding.is_empty() => {
                    if self.shared.memory_exhausted.get() {
                        return Ok(ActivationResult::BudgetExhausted(BudgetKind::Memory));
                    }
                    return Ok(match kind.as_str() {
                        "stack" => ActivationResult::BudgetExhausted(BudgetKind::Stack),
                        "unserializable" => failed(Failure::ResultNotSerializable {
                            detail: truncate(text),
                        }),
                        "host_rejection" if host_position.is_some() => {
                            failed(Failure::ScriptHostRejection {
                                position: host_position.expect("host rejection position"),
                                message: truncate(text),
                            })
                        }
                        _ => failed(Failure::Script {
                            message: truncate(text),
                        }),
                    });
                }
                ScriptState::Fulfilled(text) if outstanding.is_empty() => {
                    let cap = self.shared.bridge.borrow().max_value;
                    if text.len() > cap {
                        return Ok(failed(Failure::ResultTooLarge {
                            bytes: text.len() as u64,
                            cap: cap as u64,
                        }));
                    }
                    let value =
                        JsonText::new(text).expect("result was checked against the hard value cap");
                    return Ok(ActivationResult::Completed { value });
                }
                ScriptState::Pending if outstanding.is_empty() => {
                    return Ok(ActivationResult::Stalled);
                }
                // Still waiting on calls. A script that has finished still
                // waits for every call it dispatched, so none is left without
                // a recorded outcome.
                _ => {}
            }
            if all_long {
                return Ok(ActivationResult::Suspended {
                    awaited: outstanding,
                });
            }

            let last_order = self.shared.bridge.borrow().last_order;
            let reply = self.shared.link.borrow_mut().wait(&outstanding, last_order);
            match reply {
                Ok(WaitReply::Deliver(outcome)) => {
                    {
                        let mut bridge = self.shared.bridge.borrow_mut();
                        bridge.outstanding.remove(&outcome.position);
                        bridge.long_running.remove(&outcome.position);
                        bridge.last_order = Some(outcome.delivery_order);
                    }
                    self.deliver(outcome.position, outcome.settlement, &outcome.value)?;
                }
                Ok(WaitReply::LongRunning(positions)) => {
                    self.shared
                        .bridge
                        .borrow_mut()
                        .long_running
                        .extend(positions);
                }
                Err(e) => return Err(failed(Failure::HostLink { detail: e.0 })),
            }
        }
    }
}

mod allocator;
#[cfg(test)]
mod tests;
