//! Settlement and reporting for historical flow token reservations.
//!
//! Reservations count fresh input, cache writes and output in fixed windows
//! anchored at the Unix epoch. Cached input is recorded but not counted.

use crate::error::{CoreError, Result};
use basal_host::HostOutcome;
use basal_proto::Settlement;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

/// Token usage as Broca reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg(any(test, feature = "test-support"))]
pub struct Usage {
    /// Fresh input.
    pub input_tokens: u64,
    pub cache_write_tokens: u64,
    /// Output, reasoning included.
    pub output_tokens: u64,
    /// Input served from the provider's cache: recorded, not capped.
    pub cached_input_tokens: u64,
}

use crate::model::to_u64;

/// The start of the window containing `now_ms`.
pub fn window_start(now_ms: i64, window_ms: i64) -> i64 {
    if window_ms <= 0 {
        return now_ms;
    }
    now_ms.div_euclid(window_ms) * window_ms
}

/// What a settled call reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Report {
    /// Broca's usage.
    #[cfg(any(test, feature = "test-support"))]
    Usage(Usage),
    /// Token measurements attached to the outcome, with missing fields
    /// retained as absent instead of being converted to zero.
    Metadata(basal_host::TokenUsage),
    /// The call provably had no effect (a send that never left), so it
    /// cost nothing.
    NoEffect,
    /// The call ended without a usage report. Its reservation, an upper
    /// bound, is charged in full rather than guessed lower.
    Unreported,
}

/// Which reservation to settle.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Key<'a> {
    #[cfg(any(test, feature = "test-support"))]
    SendId(&'a str),
    Call {
        run_id: &'a str,
        position: u64,
    },
}

/// Replaces a reservation with what was reported. Applied once per send
/// id: a reservation already settled is left alone, so a duplicate report
/// changes nothing. Returns whether this report was the one applied.
pub(crate) fn settle(tx: &Transaction, key: Key<'_>, report: Report, now_ms: i64) -> Result<bool> {
    let row: Option<(String, String, i64, i64, i64)> = match key {
        #[cfg(any(test, feature = "test-support"))]
        Key::SendId(send_id) => tx
            .query_row(
                "SELECT send_id, flow_id, window_ms, window_start, reserved FROM token_ledger \
                 WHERE send_id = ?1 AND state = 'reserved'",
                [send_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?,
        Key::Call { run_id, position } => {
            let position = crate::model::position_i64(position)?;
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
    let (usage, mut unreported) = match report {
        #[cfg(any(test, feature = "test-support"))]
        Report::Usage(u) => (
            basal_host::TokenUsage {
                input_tokens: Some(u.input_tokens),
                cache_write_tokens: Some(u.cache_write_tokens),
                output_tokens: Some(u.output_tokens),
                cached_input_tokens: Some(u.cached_input_tokens),
                ..Default::default()
            },
            0,
        ),
        Report::Metadata(u) => (u, 0),
        Report::NoEffect => (
            basal_host::TokenUsage {
                input_tokens: Some(0),
                cache_write_tokens: Some(0),
                output_tokens: Some(0),
                cached_input_tokens: Some(0),
                ..Default::default()
            },
            0,
        ),
        Report::Unreported => (basal_host::TokenUsage::default(), reserved),
    };
    let (input, cache_write, output, cached) = (
        usage.input_tokens.map(saturate_usage),
        usage.cache_write_tokens.map(saturate_usage),
        usage.output_tokens.map(saturate_usage),
        usage.cached_input_tokens.map(saturate_usage),
    );
    // Missing capped measurements retain the unmeasured part of the upper
    // bound. Nullable ledger columns still distinguish absence from zero.
    if input.is_none() || cache_write.is_none() || output.is_none() {
        unreported = reserved
            .saturating_sub(
                input
                    .unwrap_or(0)
                    .saturating_add(cache_write.unwrap_or(0))
                    .saturating_add(output.unwrap_or(0)),
            )
            .max(0);
    }
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
          input_tokens = MIN(9223372036854775807, input_tokens + ?5), cache_write_tokens = MIN(9223372036854775807, cache_write_tokens + ?6), \
          output_tokens = MIN(9223372036854775807, output_tokens + ?7), cached_input_tokens = MIN(9223372036854775807, cached_input_tokens + ?8), \
          unreported_tokens = MIN(9223372036854775807, unreported_tokens + ?9) \
         WHERE flow_id = ?1 AND window_ms = ?2 AND window_start = ?3",
        params![
            flow_id,
            window_ms,
            start,
            reserved,
            input.unwrap_or(0),
            cache_write.unwrap_or(0),
            output.unwrap_or(0),
            cached.unwrap_or(0),
            unreported
        ],
    )?;
    Ok(true)
}

// Usage is provider-controlled. Keep an over-cap charge without preventing
// the outcome from committing or letting SQLite promote overflowing totals to REAL.
fn saturate_usage(tokens: u64) -> i64 {
    i64::try_from(tokens).unwrap_or(i64::MAX)
}

/// Settles the reservation of the call at (run, position), if it has one
/// still open, from the outcome that arrived for it.
pub(crate) fn settle_outcome(
    tx: &Transaction,
    run_id: &str,
    position: u64,
    outcome: &HostOutcome,
    now_ms: i64,
) -> Result<bool> {
    let report = match outcome.usage {
        Some(usage) => Report::Metadata(usage),
        None if outcome.settlement == Settlement::Rejected
            && serde_json::from_str::<Value>(outcome.value.as_str())
                .ok()
                .is_some_and(|value| {
                    value.get("code").and_then(Value::as_str) == Some(UNAVAILABLE_CODE)
                }) =>
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
pub(crate) fn window_usage(
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
    window_counts(
        window_ms,
        window_start,
        [reserved, input, cache_write, output, cached, unreported],
    )
}

fn window_counts(window_ms: i64, window_start: i64, counts: [i64; 6]) -> Result<WindowUsage> {
    let [reserved, input, cache_write, output, cached, unreported] = counts;
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

/// Read approved manifests and their current token windows in one statement.
/// Package instances prefer the package approval, just as `install::approved`
/// does; a missing ledger row still means an empty window, not a missing flow.
pub(crate) fn window_usage_many(
    conn: &Connection,
    flow_ids: &[&str],
    at_ms: i64,
) -> Result<std::collections::BTreeMap<String, WindowUsage>> {
    let ids = serde_json::json!(flow_ids).to_string();
    let mut stmt = conn.prepare(
        "SELECT f.flow_id, COALESCE(p.version, i.version),
                COALESCE(p.code_hash, i.code_hash), COALESCE(p.script, i.script),
                COALESCE(p.manifest, i.manifest), w.window_ms, w.window_start,
                w.reserved, w.input_tokens, w.cache_write_tokens, w.output_tokens,
                w.cached_input_tokens, w.unreported_tokens
         FROM flows f
         LEFT JOIN package_versions p ON p.package=f.package AND p.version=f.approved_version
             AND f.removed=0 AND NOT EXISTS (
                 SELECT 1 FROM instance_revocations r
                 WHERE r.flow_id=f.flow_id AND r.version=f.approved_version)
         LEFT JOIN installs i ON i.flow_id=f.flow_id AND i.version=f.approved_version
             AND i.state='approved'
         LEFT JOIN token_windows w ON w.flow_id=f.flow_id
             AND w.window_start<=?2 AND (?2-w.window_start)<w.window_ms
         WHERE f.flow_id IN (SELECT value FROM json_each(?1))
             AND (p.package IS NOT NULL OR i.flow_id IS NOT NULL)
         ORDER BY f.flow_id",
    )?;
    let mut rows = stmt.query(params![ids, at_ms])?;
    let mut windows = std::collections::BTreeMap::new();
    while let Some(row) = rows.next()? {
        let flow_id: String = row.get(0)?;
        if !windows.contains_key(&flow_id) {
            let version: i64 = row.get(1)?;
            u32::try_from(version).map_err(|_| CoreError::Corrupt(format!("version {version}")))?;
            crate::model::digest(row.get(2)?)?;
            let _: String = row.get(3)?;
            let manifest: String = row.get(4)?;
            let manifest = crate::manifest::Manifest::parse_stored(&manifest)
                .map_err(|e| CoreError::Corrupt(e.to_string()))?;
            windows.insert(
                flow_id.clone(),
                manifest.token_window_ms().map(|window_ms| WindowUsage {
                    window_ms,
                    window_start: window_start(at_ms, window_ms),
                    reserved: 0,
                    input_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: 0,
                    cached_input_tokens: 0,
                    unreported_tokens: 0,
                }),
            );
        }
        if let Some(Some(window)) = windows.get_mut(&flow_id)
            && row.get::<_, Option<i64>>(5)? == Some(window.window_ms)
            && row.get::<_, Option<i64>>(6)? == Some(window.window_start)
        {
            *window = window_counts(
                window.window_ms,
                window.window_start,
                [
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ],
            )?;
        }
    }
    Ok(windows
        .into_iter()
        .filter_map(|(id, window)| window.map(|w| (id, w)))
        .collect())
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
