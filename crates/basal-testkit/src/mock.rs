//! A deterministic mock of basal's hosts.
//!
//! Op behaviour is chosen by the call itself, so a test script states what it
//! expects the host to do:
//!
//! | Call | Behaviour |
//! |---|---|
//! | `ops.call('mock', 'echo', args)` | fulfils with `args` |
//! | `ops.call('mock', 'fail', args)` | rejects with `{message: args.message or "failed", code: "denied"}` |
//! | `ops.call('mock', 'raw', {raw})` | fulfils with the JSON text `raw`, verbatim |
//! | `ops.call('mock', 'long', args)` | long-running until the test completes it |
//! | `ops.call('mock', 'sleep', {ms})` | waits `ms` of real time, then fulfils with `args` |
//! | any other op | fulfils with `args` |
//!
//! An `args.delay` number puts the outcome that many ticks of virtual time
//! after the call was issued; the parent always delivers the earliest ready
//! outcome first, so delays decide races deterministically. `llm` is
//! long-running; the other primitives answer with fixed data.

use std::time::Duration;

use basal_proto::{CallKind, HostCall, JsonText, Primitive, Settlement};
use serde_json::{Value, json};

/// How the mock answers a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Ready {
        delay_ticks: u64,
        settlement: Settlement,
        value: JsonText,
        /// Real time to spend before delivering (to test that waiting on the
        /// host does not consume the JS budget).
        sleep: Duration,
    },
    LongRunning,
}

/// Clock, randomness and op behaviour.
#[derive(Debug, Clone)]
pub struct MockHost {
    /// The next clock read returns this, in milliseconds since the epoch.
    pub clock_ms: u64,
    /// Added to the clock after every read, so reads are monotonic.
    pub clock_step_ms: u64,
    /// Real time to wait before answering a clock read or random sample.
    pub sync_sleep: Duration,
    random_state: u64,
}

impl Default for MockHost {
    fn default() -> Self {
        Self {
            clock_ms: 1_767_225_600_000, // 2026-01-01T00:00:00Z
            clock_step_ms: 1,
            sync_sleep: Duration::ZERO,
            random_state: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

fn text(value: &Value) -> JsonText {
    JsonText::new(value.to_string()).unwrap_or_else(|_| JsonText::null())
}

impl MockHost {
    pub fn advance(&mut self, by: Duration) {
        self.clock_ms += by.as_millis() as u64;
    }

    fn next_random(&mut self) -> f64 {
        // xorshift64*: deterministic, and plenty for a mock.
        let mut x = self.random_state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.random_state = x;
        let bits = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        bits as f64 / (1u64 << 53) as f64
    }

    /// Answers a synchronous call (a clock read or a random sample).
    pub fn answer_sync(&mut self, call: &HostCall) -> (Settlement, JsonText) {
        if !self.sync_sleep.is_zero() {
            std::thread::sleep(self.sync_sleep);
        }
        match call.kind {
            CallKind::Primitive(Primitive::Now) => {
                let now = self.clock_ms;
                self.clock_ms += self.clock_step_ms;
                (Settlement::Fulfilled, text(&json!(now)))
            }
            CallKind::Primitive(Primitive::Random) => {
                (Settlement::Fulfilled, text(&json!(self.next_random())))
            }
            _ => (
                Settlement::Rejected,
                text(&json!({"message": "not a synchronous call"})),
            ),
        }
    }

    /// Answers an asynchronous call.
    pub fn answer(&mut self, call: &HostCall) -> Answer {
        let args: Value = serde_json::from_str(call.args.as_str()).unwrap_or(Value::Null);
        let delay_ticks = args.get("delay").and_then(Value::as_u64).unwrap_or(0);
        let ready = |settlement, value: JsonText| Answer::Ready {
            delay_ticks,
            settlement,
            value,
            sleep: Duration::ZERO,
        };
        match &call.kind {
            CallKind::Tool { .. } => ready(
                Settlement::Rejected,
                text(
                    &json!({"code": "profile_violation", "message": "catalog tools are not flow host calls"}),
                ),
            ),
            CallKind::Op { module, op } if module == "mock" => match op.as_str() {
                "fail" => {
                    let message = args
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("failed");
                    ready(
                        Settlement::Rejected,
                        text(&json!({"message": message, "code": "denied"})),
                    )
                }
                "raw" => {
                    let raw = args.get("raw").and_then(Value::as_str).unwrap_or("null");
                    ready(
                        Settlement::Fulfilled,
                        JsonText::new(raw).unwrap_or_else(|_| JsonText::null()),
                    )
                }
                "long" => Answer::LongRunning,
                "sleep" => Answer::Ready {
                    delay_ticks,
                    settlement: Settlement::Fulfilled,
                    value: call.args.clone(),
                    sleep: Duration::from_millis(
                        args.get("ms").and_then(Value::as_u64).unwrap_or(0),
                    ),
                },
                _ => ready(Settlement::Fulfilled, call.args.clone()),
            },
            CallKind::Op { .. } => ready(Settlement::Fulfilled, call.args.clone()),
            CallKind::Primitive(p) => match p {
                Primitive::Llm => Answer::LongRunning,
                Primitive::Facts => ready(
                    Settlement::Fulfilled,
                    text(&json!({"agent": args.get("agent"), "activity": {"state": "idle"}})),
                ),
                Primitive::Classify => ready(
                    Settlement::Fulfilled,
                    text(
                        args.get("labels")
                            .and_then(|l| l.get(0))
                            .unwrap_or(&Value::Null),
                    ),
                ),
                Primitive::SinkDigest | Primitive::SinkStatus => {
                    ready(Settlement::Fulfilled, text(&json!({"accepted": true})))
                }
                Primitive::KvGet => ready(Settlement::Fulfilled, JsonText::null()),
                Primitive::KvSet | Primitive::KvDelete => {
                    ready(Settlement::Fulfilled, text(&json!(true)))
                }
                Primitive::Sh => ready(
                    Settlement::Fulfilled,
                    text(&json!({"stdout": "mock shell", "exit": 0})),
                ),
                Primitive::Now | Primitive::Random => ready(
                    Settlement::Rejected,
                    text(&json!({"message": "synchronous call issued asynchronously"})),
                ),
                // The built-ins echo their arguments: this parent has no
                // manifest, so nothing here touches a file or the network.
                Primitive::FsRead
                | Primitive::FsList
                | Primitive::FsStat
                | Primitive::FsWrite
                | Primitive::GitLog
                | Primitive::GitRevParse
                | Primitive::GitDescribeTags
                | Primitive::GitShow
                | Primitive::GitDiff
                | Primitive::NetFetch => ready(Settlement::Fulfilled, call.args.clone()),
            },
        }
    }
}
