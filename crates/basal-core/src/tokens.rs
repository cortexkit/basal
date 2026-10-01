//! Token reservations for `llm` and `classify`.
//!
//! The cap counts, per flow and window, fresh input plus cache write plus
//! output tokens (reasoning is already inside output in Broca's usage).
//! Cached input is recorded and shown but not capped: context re-reads
//! dominate token volume and would trip a cap long before real work.
//!
//! - Before a model call is journaled, its liability is reserved against
//!   the flow's current window, in the same transaction as its journal row:
//!   an input estimate that is an upper bound in practice plus the clamped
//!   maximum output. A call that does not fit is refused as a journaled
//!   rejection, so two calls of one run cannot both spend the same
//!   remainder.
//! - The request sent is built here once (the clamp) and journaled with the
//!   intent, so a re-issue after a crash sends identical bytes under the
//!   same `send_id`, which Broca deduplicates on.
//! - When the call's outcome arrives, its reservation is replaced by the
//!   reported usage, once per `send_id`, in the window the reservation was
//!   made in, however long the run was suspended and whatever window is
//!   current by then.
//!
//! Windows are fixed intervals anchored at the Unix epoch in UTC: a call's
//! window is `floor(now / length)`. That makes "the window a reservation
//! was made in" one number that survives restarts, and a `"1d"` window a
//! UTC calendar day. A rolling window would have no single window to
//! attribute usage to, and windows anchored at approval would reset with
//! every new version.

use basal_host::HostOutcome;
use basal_proto::{JsonText, Primitive, Settlement};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use crate::authorize::{Refusal, codes};
use crate::error::{CoreError, Result};
use crate::manifest::LlmGrant;

/// Added to every input estimate for what the request's bytes do not show:
/// the provider's own framing of a request (roles, system wrapping,
/// message separators).
pub const INPUT_OVERHEAD_TOKENS: u64 = 256;

/// The output ceiling of a `classify` call: it answers with one label.
pub const CLASSIFY_MAX_OUTPUT: u32 = 64;

/// Token usage as Broca reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Fresh input.
    pub input_tokens: u64,
    pub cache_write_tokens: u64,
    /// Output, reasoning included.
    pub output_tokens: u64,
    /// Input served from the provider's cache: recorded, not capped.
    pub cached_input_tokens: u64,
}

impl Usage {
    /// What counts against the cap.
    pub fn capped(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.output_tokens)
    }

    /// Reads Broca's usage object. Every field must be a whole number.
    pub fn from_value(v: &Value) -> Option<Self> {
        let field = |name: &str| v.get(name).and_then(Value::as_u64);
        Some(Self {
            input_tokens: field("input_tokens")?,
            cache_write_tokens: field("cache_write_tokens")?,
            output_tokens: field("output_tokens")?,
            cached_input_tokens: field("cached_input_tokens")?,
        })
    }
}

fn to_i64(n: u64) -> Result<i64> {
    i64::try_from(n).map_err(|_| CoreError::Invalid(format!("{n} tokens")))
}

fn to_u64(n: i64, what: &str) -> Result<u64> {
    crate::model::to_u64(n, what)
}

/// A model call ready to journal: the exact request to send, and the
/// tokens to reserve for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clamped {
    pub request: JsonText,
    pub max_output: u32,
    pub reserve: u64,
}

/// The input estimate for a request of `bytes` UTF-8 bytes.
///
/// Byte-level tokenizers never produce more tokens than the UTF-8 bytes
/// they encode (each token covers at least one byte), so the byte count is
/// an upper bound on the text's tokens, and JSON quoting and the envelope's
/// own fields only add bytes that are not tokens. The fixed overhead covers
/// the provider's framing.
pub fn estimate_input(bytes: usize) -> u64 {
    (bytes as u64).saturating_add(INPUT_OVERHEAD_TOKENS)
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Builds the request basal sends for an `llm` or `classify` call: the
/// script's own argument bytes, unchanged, inside an envelope that carries
/// the `send_id`, the flow's work class and session, and the clamped
/// output ceiling. llm steps get no tools, so a request asking for them is
/// refused.
pub fn clamp(
    primitive: Primitive,
    args_text: &JsonText,
    args: &Value,
    grant: &LlmGrant,
    flow_id: &str,
    run_id: &str,
    send_id: &str,
) -> std::result::Result<Clamped, Refusal> {
    let invalid = |m: &str| Refusal::new(codes::INVALID_ARGUMENTS, m);
    let max_output = match primitive {
        Primitive::Llm => {
            if !args.is_object() {
                return Err(invalid("llm(request) needs an object"));
            }
            if args.get("tools").is_some_and(|t| !t.is_null()) {
                return Err(Refusal::denied("model calls from flows get no tools"));
            }
            let requested = match args.get("max_output") {
                None | Some(Value::Null) => grant.max_output,
                Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                    Some(n) if n > 0 => n,
                    _ => return Err(invalid("max_output must be a positive whole number")),
                },
            };
            requested.min(grant.max_output)
        }
        Primitive::Classify => {
            if !args.get("text").is_some_and(Value::is_string) {
                return Err(invalid("classify(text, labels) needs text"));
            }
            let labels_ok = args
                .get("labels")
                .and_then(Value::as_array)
                .is_some_and(|l| !l.is_empty() && l.iter().all(Value::is_string));
            if !labels_ok {
                return Err(invalid("classify(text, labels) needs a list of labels"));
            }
            CLASSIFY_MAX_OUTPUT.min(grant.max_output)
        }
        _ => return Err(invalid("not a model call")),
    };
    let text = format!(
        "{{\"send_id\":{},\"work_class\":{},\"session\":{},\"op\":{},\"max_output\":{},\"request\":{}}}",
        json_string(send_id),
        json_string(&format!("flow:{flow_id}")),
        json_string(&format!("basal:flow-{flow_id}:{run_id}")),
        json_string(primitive.name()),
        max_output,
        args_text.as_str(),
    );
    let request = JsonText::new(text)
        .map_err(|e| Refusal::new(codes::TOO_LARGE, format!("the model request: {e}")))?;
    let reserve = estimate_input(request.len()).saturating_add(u64::from(max_output));
    Ok(Clamped {
        request,
        max_output,
        reserve,
    })
}

/// The start of the window containing `now_ms`.
pub fn window_start(now_ms: i64, window_ms: i64) -> i64 {
    if window_ms <= 0 {
        return now_ms;
    }
    now_ms.div_euclid(window_ms) * window_ms
}

/// One reservation to make.
#[derive(Debug, Clone)]
pub struct Reservation<'a> {
    pub send_id: &'a str,
    pub flow_id: &'a str,
    pub run_id: &'a str,
    pub position: u64,
    pub window_ms: i64,
    pub cap: u64,
    pub amount: u64,
    pub now_ms: i64,
}

/// Reserves `amount` against the flow's current window, or refuses when
/// what the window already counts (outstanding reservations included) plus
/// `amount` exceeds the cap. Runs in the transaction that journals the
/// call, so the check and the reservation are one atomic step.
pub fn reserve(tx: &Transaction, r: &Reservation<'_>) -> Result<std::result::Result<(), Refusal>> {
    let start = window_start(r.now_ms, r.window_ms);
    tx.execute(
        "INSERT OR IGNORE INTO token_windows (flow_id, window_ms, window_start) VALUES (?1, ?2, ?3)",
        params![r.flow_id, r.window_ms, start],
    )?;
    let used: i64 = tx.query_row(
        "SELECT reserved + input_tokens + cache_write_tokens + output_tokens + unreported_tokens \
         FROM token_windows WHERE flow_id = ?1 AND window_ms = ?2 AND window_start = ?3",
        params![r.flow_id, r.window_ms, start],
        |row| row.get(0),
    )?;
    let used = to_u64(used, "window usage")?;
    if used.saturating_add(r.amount) > r.cap {
        return Ok(Err(Refusal::new(
            codes::TOKEN_CAP,
            format!(
                "the call needs up to {} tokens and the window has {} of {} left",
                r.amount,
                r.cap.saturating_sub(used),
                r.cap
            ),
        )));
    }
    let amount = to_i64(r.amount)?;
    tx.execute(
        "UPDATE token_windows SET reserved = reserved + ?4 \
         WHERE flow_id = ?1 AND window_ms = ?2 AND window_start = ?3",
        params![r.flow_id, r.window_ms, start, amount],
    )?;
    let position = i64::try_from(r.position)
        .map_err(|_| CoreError::Invalid(format!("position {}", r.position)))?;
    tx.execute(
        "INSERT INTO token_ledger (send_id, flow_id, run_id, position, window_ms, window_start, \
         reserved, state, reserved_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'reserved', ?8)",
        params![
            r.send_id,
            r.flow_id,
            r.run_id,
            position,
            r.window_ms,
            start,
            amount,
            r.now_ms
        ],
    )?;
    Ok(Ok(()))
}

/// What a settled call reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Report {
    /// Broca's usage.
    Usage(Usage),
    /// The call provably had no effect (a send that never left), so it
    /// cost nothing.
    NoEffect,
    /// The call ended without a usage report. Its reservation, an upper
    /// bound, is charged in full rather than guessed lower.
    Unreported,
}

/// Which reservation to settle.
#[derive(Debug, Clone, Copy)]
pub enum Key<'a> {
    SendId(&'a str),
    Call { run_id: &'a str, position: u64 },
}

/// Replaces a reservation with what was reported. Applied once per send
/// id: a reservation already settled is left alone, so a duplicate report
/// changes nothing. Returns whether this report was the one applied.
pub fn settle(tx: &Transaction, key: Key<'_>, report: Report, now_ms: i64) -> Result<bool> {
    let row: Option<(String, String, i64, i64, i64)> = match key {
        Key::SendId(send_id) => tx
            .query_row(
                "SELECT send_id, flow_id, window_ms, window_start, reserved FROM token_ledger \
                 WHERE send_id = ?1 AND state = 'reserved'",
                [send_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?,
        Key::Call { run_id, position } => {
            let position = i64::try_from(position)
                .map_err(|_| CoreError::Invalid(format!("position {position}")))?;
            tx.query_row(
                "SELECT send_id, flow_id, window_ms, window_start, reserved FROM token_ledger \
                 WHERE run_id = ?1 AND position = ?2 AND state = 'reserved'",
                params![run_id, position],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?
        }
    };
    let Some((send_id, flow_id, window_ms, start, reserved)) = row else {
        return Ok(false);
    };
    let (usage, unreported) = match report {
        Report::Usage(u) => (u, 0),
        Report::NoEffect => (Usage::default(), 0),
        Report::Unreported => (Usage::default(), reserved),
    };
    let (input, cache_write, output, cached) = (
        to_i64(usage.input_tokens)?,
        to_i64(usage.cache_write_tokens)?,
        to_i64(usage.output_tokens)?,
        to_i64(usage.cached_input_tokens)?,
    );
    let changed = tx.execute(
        "UPDATE token_ledger SET state = 'settled', input_tokens = ?2, cache_write_tokens = ?3, \
         output_tokens = ?4, cached_input_tokens = ?5, unreported_tokens = ?6, settled_at = ?7 \
         WHERE send_id = ?1 AND state = 'reserved'",
        params![
            send_id,
            input,
            cache_write,
            output,
            cached,
            unreported,
            now_ms
        ],
    )?;
    if changed == 0 {
        return Ok(false);
    }
    // The reservation's own window, not the current one: a call reserved
    // before a long suspension or a window rollover is charged where it
    // was counted.
    tx.execute(
        "UPDATE token_windows SET reserved = reserved - ?4, \
         input_tokens = input_tokens + ?5, cache_write_tokens = cache_write_tokens + ?6, \
         output_tokens = output_tokens + ?7, cached_input_tokens = cached_input_tokens + ?8, \
         unreported_tokens = unreported_tokens + ?9 \
         WHERE flow_id = ?1 AND window_ms = ?2 AND window_start = ?3",
        params![
            flow_id,
            window_ms,
            start,
            reserved,
            input,
            cache_write,
            output,
            cached,
            unreported
        ],
    )?;
    Ok(true)
}

/// Settles the reservation of the call at (run, position), if it has one
/// still open, from the outcome that arrived for it.
pub fn settle_outcome(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    outcome: &HostOutcome,
    now_ms: i64,
) -> Result<bool> {
    let p =
        i64::try_from(position).map_err(|_| CoreError::Invalid(format!("position {position}")))?;
    let open: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM token_ledger WHERE run_id = ?1 AND position = ?2 \
         AND state = 'reserved')",
        params![run_id, p],
        |r| r.get(0),
    )?;
    if !open {
        return Ok(false);
    }
    let value: Value = serde_json::from_str(outcome.value.as_str()).unwrap_or(Value::Null);
    let report = match value.get("usage").and_then(Usage::from_value) {
        Some(usage) => Report::Usage(usage),
        None if outcome.settlement == Settlement::Rejected
            && value.get("code").and_then(Value::as_str) == Some(UNAVAILABLE_CODE) =>
        {
            Report::NoEffect
        }
        None => Report::Unreported,
    };
    settle(tx, Key::Call { run_id, position }, report, now_ms)
}

/// The code of the rejection the runtime records when a call provably
/// never reached its host.
const UNAVAILABLE_CODE: &str = "unavailable";

/// What one window of one flow counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowUsage {
    pub window_ms: i64,
    pub window_start: i64,
    /// Outstanding reservations.
    pub reserved: u64,
    pub input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    /// Calls that ended without a usage report, charged at their
    /// reservation.
    pub unreported_tokens: u64,
}

impl WindowUsage {
    /// What counts against the cap.
    pub fn counted(&self) -> u64 {
        self.reserved
            .saturating_add(self.input_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.output_tokens)
            .saturating_add(self.unreported_tokens)
    }
}

/// What the window starting at `window_start` counts for the flow.
pub fn window_usage(
    conn: &Connection,
    flow_id: &str,
    window_ms: i64,
    window_start: i64,
) -> Result<WindowUsage> {
    let row: Option<(i64, i64, i64, i64, i64, i64)> = conn
        .query_row(
            "SELECT reserved, input_tokens, cache_write_tokens, output_tokens, \
             cached_input_tokens, unreported_tokens FROM token_windows \
             WHERE flow_id = ?1 AND window_ms = ?2 AND window_start = ?3",
            params![flow_id, window_ms, window_start],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let (reserved, input, cache_write, output, cached, unreported) =
        row.unwrap_or((0, 0, 0, 0, 0, 0));
    Ok(WindowUsage {
        window_ms,
        window_start,
        reserved: to_u64(reserved, "reserved tokens")?,
        input_tokens: to_u64(input, "input tokens")?,
        cache_write_tokens: to_u64(cache_write, "cache write tokens")?,
        output_tokens: to_u64(output, "output tokens")?,
        cached_input_tokens: to_u64(cached, "cached input tokens")?,
        unreported_tokens: to_u64(unreported, "unreported tokens")?,
    })
}

/// Model-call reservations not yet settled, for a run.
pub fn open_reservations(conn: &Connection, run_id: &str) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM token_ledger WHERE run_id = ?1 AND state = 'reserved'",
        [run_id],
        |r| r.get(0),
    )?;
    to_u64(n, "open reservations")
}
