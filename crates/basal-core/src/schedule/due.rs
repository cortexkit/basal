//! Due times: when a compiled schedule fires.
//!
//! **Cron patterns** are matched against wall-clock time in the schedule's
//! zone. croner finds the matching wall-clock times on a zone-free
//! calendar; turning each into an instant is done here, by these rules:
//!
//! - A wall-clock time that exists once is that instant.
//! - A wall-clock time inside a spring-forward gap (02:30 on a night the
//!   clock jumps from 02:00 to 03:00) does not exist. It fires once, at the
//!   first valid instant after the gap: the transition itself, 03:00 in the
//!   new offset. Several matches inside one gap (02:00 and 02:30 for
//!   `*/30`) collapse onto that one instant, and a match exactly at the
//!   gap's end (03:00) is the same instant too, so it fires once.
//! - A wall-clock time inside a fall-back overlap (02:30 on a night the
//!   clock goes back from 03:00 to 02:00) exists twice. It fires once, at
//!   its first occurrence. The repeated hour's second pass fires nothing,
//!   so `*/30` has a 90-minute pause that night; a schedule that must run
//!   every 30 minutes of real time is an interval, not a cron pattern.
//!
//! The mapping never goes backwards in time as wall-clock time advances, so
//! "the next due time after instant X" is the first matching wall-clock
//! time, from X's own wall-clock time onwards, whose instant is after X.
//!
//! **Intervals** are anchored at the schedule's creation: due times are the
//! anchor plus every whole multiple (one or more) of the period, in real
//! time. A late tick never shifts later due times.

use std::collections::VecDeque;

use croner::Cron;
use croner::errors::CronError;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};

use super::spec::{CompiledSchedule, When};

/// Why a due time could not be computed. These come from the schedule's
/// own data, never from a bug the caller can fix by retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueError(pub String);

impl std::fmt::Display for DueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "due time: {}", self.0)
    }
}

impl std::error::Error for DueError {}

/// How many matches a single search may look at before giving up. The
/// longest legitimate run of matches that map to instants not after the
/// start is a repeated hour plus a gap, a few hundred at most for a
/// minute-level pattern; this bound only stops a defect from spinning.
const SEARCH_GUARD: usize = 100_000;

/// The instant a wall-clock time in `tz` fires at, by the rules in the
/// module documentation.
pub fn resolve(tz: &TimeZone, civil: DateTime) -> Result<Timestamp, DueError> {
    let ambiguous = tz.to_ambiguous_timestamp(civil);
    match ambiguous.offset() {
        AmbiguousOffset::Unambiguous { .. } | AmbiguousOffset::Fold { .. } => ambiguous
            .earlier()
            .map_err(|e| DueError(format!("{civil}: {e}"))),
        AmbiguousOffset::Gap { after, .. } => {
            // Read with the offset in force after the gap, the wall-clock
            // time names an instant just before the transition (it is
            // earlier than the gap's end by exactly the time already into
            // the gap). The transition following that instant is the end
            // of the gap: the first instant whose wall-clock time is valid
            // again.
            let before_transition = after
                .to_timestamp(civil)
                .map_err(|e| DueError(format!("{civil}: {e}")))?;
            tz.following(before_transition)
                .next()
                .map(|t| t.timestamp())
                .ok_or_else(|| DueError(format!("{civil}: no transition ends the gap")))
        }
    }
}

fn cron_error(e: CronError) -> DueError {
    DueError(e.to_string())
}

/// The first instant strictly after `after` at which the cron pattern is
/// due in `tz`; `None` when it never matches again (croner's search stops
/// at the year 5000).
fn cron_next_after(
    cron: &Cron,
    tz: &TimeZone,
    after: Timestamp,
) -> Result<Option<Timestamp>, DueError> {
    let mut cursor = after.to_zoned(tz.clone()).datetime();
    let mut inclusive = true;
    for _ in 0..SEARCH_GUARD {
        let civil = match cron.find_next_occurrence(&cursor, inclusive) {
            Ok(c) => c,
            Err(CronError::TimeSearchLimitExceeded) => return Ok(None),
            Err(e) => return Err(cron_error(e)),
        };
        let at = resolve(tz, civil)?;
        if at > after {
            return Ok(Some(at));
        }
        // Either the second pass of a repeated hour, whose matches already
        // fired in the first pass, or another match collapsing onto a gap's
        // end that already fired.
        cursor = civil;
        inclusive = false;
    }
    Err(DueError(format!(
        "no due time after {after} within {SEARCH_GUARD} matches"
    )))
}

/// Up to `keep` of the newest due times in `[from, to]`, oldest first.
///
/// Searches backwards in wall-clock time. Only the second pass of a fold
/// needs a forward offset: later wall times in the first pass precede `to`.
fn cron_newest_in(
    cron: &Cron,
    tz: &TimeZone,
    from: Timestamp,
    to: Timestamp,
    keep: usize,
) -> Result<VecDeque<Timestamp>, DueError> {
    let mut out: VecDeque<Timestamp> = VecDeque::new();
    if keep == 0 {
        return Ok(out);
    }
    let start = to.to_zoned(tz.clone()).datetime();
    let ambiguous = tz.to_ambiguous_timestamp(start);
    let mut cursor = match ambiguous.offset() {
        AmbiguousOffset::Fold { .. } => {
            let earlier = ambiguous.earlier().map_err(|e| DueError(e.to_string()))?;
            let later = ambiguous.later().map_err(|e| DueError(e.to_string()))?;
            if to == later {
                start
                    .checked_add(jiff::Span::new().seconds(later.as_second() - earlier.as_second()))
                    .map_err(|e| DueError(e.to_string()))?
            } else {
                start
            }
        }
        _ => start,
    };
    let mut inclusive = true;
    for _ in 0..SEARCH_GUARD {
        let civil = match cron.find_previous_occurrence(&cursor, inclusive) {
            Ok(c) => c,
            Err(CronError::TimeSearchLimitExceeded) => return Ok(out),
            Err(e) => return Err(cron_error(e)),
        };
        cursor = civil;
        inclusive = false;
        let at = resolve(tz, civil)?;
        if at > to {
            continue;
        }
        if at < from {
            return Ok(out);
        }
        // Matches collapsing onto one gap's end are one due time.
        if out.front() != Some(&at) {
            out.push_front(at);
        }
        if out.len() == keep {
            return Ok(out);
        }
    }
    Err(DueError(format!(
        "no due times found between {from} and {to} within {SEARCH_GUARD} matches"
    )))
}

fn interval_ms(every_secs: u64) -> Result<i128, DueError> {
    i128::from(every_secs)
        .checked_mul(1000)
        .filter(|ms| *ms > 0)
        .ok_or_else(|| DueError(format!("interval of {every_secs} s")))
}

fn timestamp_ms(ms: i128) -> Option<Timestamp> {
    i64::try_from(ms)
        .ok()
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
}

/// The first `anchor + k * every` (k at least 1) strictly after `after`;
/// `None` past the end of representable time.
fn interval_next_after(
    every_secs: u64,
    anchor: Timestamp,
    after: Timestamp,
) -> Result<Option<Timestamp>, DueError> {
    let every = interval_ms(every_secs)?;
    let anchor_ms = i128::from(anchor.as_millisecond());
    let after_ms = i128::from(after.as_millisecond());
    let k = if after_ms < anchor_ms {
        1
    } else {
        (after_ms - anchor_ms) / every + 1
    };
    Ok(timestamp_ms(anchor_ms + k * every))
}

/// The due times from `first` (itself a due time) up to `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueScan {
    /// How many due times lie in `[first, now]`. Exact unless
    /// `count_is_lower_bound`.
    pub count: u64,
    /// The scan stopped counting at the limit; `count` is that limit.
    pub count_is_lower_bound: bool,
    pub first: Timestamp,
    /// The newest due times in `[first, now]`, oldest first, at most as
    /// many as asked for and at least one.
    pub newest: Vec<Timestamp>,
    /// The first due time after `now`, if the schedule fires again.
    pub next: Option<Timestamp>,
}

impl CompiledSchedule {
    /// The first due time strictly after `after`. `anchor` is the
    /// schedule's creation, which intervals count from.
    pub fn next_due_after(
        &self,
        anchor: Timestamp,
        after: Timestamp,
    ) -> Result<Option<Timestamp>, DueError> {
        match &self.when {
            When::Cron { cron, tz, .. } => cron_next_after(cron, tz, after),
            When::Interval { every_secs } => interval_next_after(*every_secs, anchor, after),
        }
    }

    /// Lists the due times in `[first, now]`, where `first` is a due time
    /// not after `now` (for an interval, one counted from the anchor, so the
    /// later ones follow from it by whole periods): how many, the newest `keep` (at least one), and the
    /// next one after `now`. Counting a cron pattern walks every due time,
    /// so it stops at `count_limit` and reports the count as a lower bound;
    /// the newest due times and the next one are found directly and are
    /// exact either way.
    pub fn scan(
        &self,
        first: Timestamp,
        now: Timestamp,
        keep: usize,
        count_limit: u64,
    ) -> Result<DueScan, DueError> {
        if first > now {
            return Err(DueError(format!("first due time {first} is after {now}")));
        }
        let keep = keep.max(1);
        let count_limit = count_limit.max(1);
        match &self.when {
            When::Interval { every_secs } => {
                let every = interval_ms(*every_secs)?;
                let first_ms = i128::from(first.as_millisecond());
                let span = i128::from(now.as_millisecond()) - first_ms;
                let count = span / every + 1;
                let newest_ms = first_ms + (count - 1) * every;
                let kept = count.min(keep as i128);
                let newest = ((count - kept)..count)
                    .map(|k| {
                        timestamp_ms(first_ms + k * every)
                            .ok_or_else(|| DueError(format!("due time {k} after {first}")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let next = timestamp_ms(newest_ms + every);
                let count =
                    u64::try_from(count).map_err(|_| DueError(format!("{count} due times")))?;
                let (count, count_is_lower_bound) = if count > count_limit {
                    (count_limit, true)
                } else {
                    (count, false)
                };
                Ok(DueScan {
                    count,
                    count_is_lower_bound,
                    first,
                    newest,
                    next,
                })
            }
            When::Cron { cron, tz, .. } => {
                let mut count: u64 = 1;
                let mut at = first;
                let mut count_is_lower_bound = false;
                loop {
                    match cron_next_after(cron, tz, at)? {
                        Some(next) if next <= now => {
                            if count == count_limit {
                                count_is_lower_bound = true;
                                break;
                            }
                            count += 1;
                            at = next;
                        }
                        _ => break,
                    }
                }
                let newest: Vec<Timestamp> = cron_newest_in(cron, tz, first, now, keep)?
                    .into_iter()
                    .collect();
                if newest.is_empty() {
                    return Err(DueError(format!(
                        "{first} is not a due time of the pattern"
                    )));
                }
                let next = cron_next_after(cron, tz, now)?;
                Ok(DueScan {
                    count,
                    count_is_lower_bound,
                    first,
                    newest,
                    next,
                })
            }
        }
    }
}
