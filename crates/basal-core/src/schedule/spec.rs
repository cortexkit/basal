//! The schedule a flow's manifest declares under `trigger.schedule`, and its
//! validation.
//!
//! The wire shape is [`ScheduleSpec`]: exactly one of `cron` (with an
//! optional `tz`) or `interval`, a `missed` policy, and for `each` an
//! optional `each_cap`. Unknown fields are refused, so a misspelt field
//! fails at install instead of silently meaning its default.
//!
//! ```json
//! { "cron": "*/30 * * * *", "tz": "Europe/Madrid", "missed": "once" }
//! { "interval": "15m", "missed": "each", "each_cap": 5 }
//! ```
//!
//! [`validate`] turns a spec into a [`CompiledSchedule`]: the parsed cron
//! pattern and the resolved zone, or the interval's length. Nothing else in
//! the scheduler accepts an unvalidated spec, so a pattern that parses at
//! install parses the same way at every tick.

use std::fmt;
use std::sync::OnceLock;

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use jiff::civil::DateTime;
use jiff::tz::{TimeZone, TimeZoneDatabase};
use serde::{Deserialize, Serialize};

/// How many fires `each` replays when the spec does not say.
pub const DEFAULT_EACH_CAP: u32 = 3;

/// The largest `each_cap` a spec may ask for. Every replayed fire is a run,
/// so the cap bounds the burst of runs one wake-up can start.
pub const MAX_EACH_CAP: u32 = 10;

/// The shortest interval: cron's own granularity. A flow is not a
/// busy-loop; anything faster belongs in an event trigger.
pub const MIN_INTERVAL_SECS: u64 = 60;

/// The longest interval: a year. Longer periods are calendar periods and
/// read better as a cron pattern.
pub const MAX_INTERVAL_SECS: u64 = 365 * 24 * 60 * 60;

/// What a schedule does with due times that passed while nothing ticked
/// (the machine slept, basal was not running, or the clock jumped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Missed {
    /// All missed due times collapse into one catch-up fire, identified by
    /// the last missed due time and carrying how many were missed and the
    /// window they span.
    #[default]
    Once,
    /// Missed due times produce no fire.
    Skip,
    /// One fire per missed due time, up to the cap. When more were missed
    /// than the cap allows, the newest ones are kept and the oldest dropped,
    /// so a catch-up always ends having run for the latest slot; the kept
    /// fires are admitted oldest first. Every one carries the full missed
    /// count and window, so a script can see what was dropped.
    Each,
}

impl Missed {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Skip => "skip",
            Self::Each => "each",
        }
    }
}

/// `trigger.schedule` as written in a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleSpec {
    /// A five-field cron pattern (minute, hour, day of month, month, day of
    /// week), matched against wall-clock time in `tz`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    /// The IANA zone the cron pattern is read in, for example
    /// `Europe/Madrid`. Absent means UTC; the host's local zone is never
    /// used, so a flow behaves the same on every machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tz: Option<String>,
    /// A fixed period such as `90s`, `15m`, `2h` or `1d`: one whole number
    /// and one unit. Fires at the schedule's creation plus every multiple of
    /// the period, in real time, whatever the zone does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<String>,
    #[serde(default)]
    pub missed: Missed,
    /// The cap for `missed: "each"`; [`DEFAULT_EACH_CAP`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub each_cap: Option<u32>,
}

/// How missed due times are handled, with the cap resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedPolicy {
    Once,
    Skip,
    Each { cap: u32 },
}

impl MissedPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Skip => "skip",
            Self::Each { .. } => "each",
        }
    }
}

/// When a validated schedule is due.
#[derive(Debug, Clone)]
pub enum When {
    Cron {
        /// The pattern as written.
        pattern: String,
        cron: Box<Cron>,
        /// The zone's IANA name as written (validated to be canonical).
        zone: String,
        tz: TimeZone,
    },
    Interval {
        /// The period in seconds, within the allowed range.
        every_secs: u64,
    },
}

/// A schedule that passed [`validate`].
#[derive(Debug, Clone)]
pub struct CompiledSchedule {
    pub when: When,
    pub missed: MissedPolicy,
}

/// Why a schedule spec was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// The JSON did not have the spec's shape: a wrong type, an unknown
    /// field, or an unknown `missed` value.
    Shape(String),
    /// Neither `cron` nor `interval` was given.
    NoTiming,
    /// Both `cron` and `interval` were given.
    BothTimings,
    /// The cron pattern does not parse.
    Cron { pattern: String, reason: String },
    /// The cron pattern parses but no date ever matches it (for example
    /// 31 February).
    CronNeverFires { pattern: String },
    /// `tz` is not a canonical name in the bundled IANA database.
    UnknownZone(String),
    /// `tz` was given with an interval, which runs in real time and has no
    /// use for a zone.
    ZoneWithInterval,
    /// The interval is not one whole number followed by `s`, `m`, `h` or
    /// `d`.
    IntervalSyntax(String),
    /// The interval is zero or outside the allowed range.
    IntervalOutOfRange { interval: String, secs: u64 },
    /// `each_cap` was given with a policy other than `each`.
    CapWithoutEach,
    /// `each_cap` is zero or above [`MAX_EACH_CAP`].
    CapOutOfRange(u32),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape(e) => write!(f, "schedule: {e}"),
            Self::NoTiming => write!(f, "schedule: give either `cron` or `interval`"),
            Self::BothTimings => write!(f, "schedule: give `cron` or `interval`, not both"),
            Self::Cron { pattern, reason } => {
                write!(f, "schedule: cron pattern {pattern:?} is invalid: {reason}")
            }
            Self::CronNeverFires { pattern } => {
                write!(f, "schedule: cron pattern {pattern:?} never matches a date")
            }
            Self::UnknownZone(z) => write!(
                f,
                "schedule: {z:?} is not a canonical IANA time zone name (for example Europe/Madrid)"
            ),
            Self::ZoneWithInterval => write!(
                f,
                "schedule: `tz` applies to cron patterns only; an interval runs in real time"
            ),
            Self::IntervalSyntax(i) => write!(
                f,
                "schedule: interval {i:?} must be a whole number followed by s, m, h or d"
            ),
            Self::IntervalOutOfRange { interval, secs } => write!(
                f,
                "schedule: interval {interval:?} is {secs} s; it must be between \
                 {MIN_INTERVAL_SECS} s and {MAX_INTERVAL_SECS} s"
            ),
            Self::CapWithoutEach => {
                write!(f, "schedule: `each_cap` applies only to `missed: \"each\"`")
            }
            Self::CapOutOfRange(c) => write!(
                f,
                "schedule: each_cap {c} must be between 1 and {MAX_EACH_CAP}"
            ),
        }
    }
}

impl std::error::Error for SpecError {}

/// The time zone database every schedule uses: the IANA data compiled into
/// the binary. Reading the host's zoneinfo instead would make due times
/// depend on which tzdata release the machine happens to carry.
pub(crate) fn zones() -> &'static TimeZoneDatabase {
    static DB: OnceLock<TimeZoneDatabase> = OnceLock::new();
    DB.get_or_init(TimeZoneDatabase::bundled)
}

/// Looks a zone up by its exact canonical name. The database itself matches
/// names case-insensitively; requiring the exact spelling keeps the name a
/// card shows identical to the one the scheduler uses.
fn zone(name: &str) -> Result<TimeZone, SpecError> {
    let tz = zones()
        .get(name)
        .map_err(|_| SpecError::UnknownZone(name.to_owned()))?;
    if tz.iana_name() != Some(name) {
        return Err(SpecError::UnknownZone(name.to_owned()));
    }
    Ok(tz)
}

/// Parses a five-field cron pattern. Seconds and years are refused: a
/// minute is the finest granularity a flow schedule has.
fn cron(pattern: &str) -> Result<Cron, SpecError> {
    let parsed = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
        .parse(pattern)
        .map_err(|e| SpecError::Cron {
            pattern: pattern.to_owned(),
            reason: e.to_string(),
        })?;
    // A pattern such as "0 0 31 2 *" parses but never matches. Looking for
    // one match from a fixed date proves the pattern can fire at all; the
    // search is bounded by croner's own year limit.
    let probe = DateTime::constant(2000, 1, 1, 0, 0, 0, 0);
    if parsed.find_next_occurrence(&probe, true).is_err() {
        return Err(SpecError::CronNeverFires {
            pattern: pattern.to_owned(),
        });
    }
    Ok(parsed)
}

/// Parses `<whole number><unit>` into seconds.
fn interval_secs(text: &str) -> Result<u64, SpecError> {
    let syntax = || SpecError::IntervalSyntax(text.to_owned());
    let unit_at = text
        .char_indices()
        .last()
        .map(|(i, _)| i)
        .ok_or_else(syntax)?;
    let (digits, unit) = text.split_at(unit_at);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(syntax());
    }
    let scale: u64 = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(syntax()),
    };
    // A number too large for u64 is out of range, not malformed.
    let out_of_range = || SpecError::IntervalOutOfRange {
        interval: text.to_owned(),
        secs: u64::MAX,
    };
    let n: u64 = digits.parse().map_err(|_| out_of_range())?;
    let secs = n.checked_mul(scale).ok_or_else(out_of_range)?;
    if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) {
        return Err(SpecError::IntervalOutOfRange {
            interval: text.to_owned(),
            secs,
        });
    }
    Ok(secs)
}

/// Checks a spec and compiles it. This is the only way to get a
/// [`CompiledSchedule`].
pub fn validate(spec: &ScheduleSpec) -> Result<CompiledSchedule, SpecError> {
    let when = match (&spec.cron, &spec.interval) {
        (None, None) => return Err(SpecError::NoTiming),
        (Some(_), Some(_)) => return Err(SpecError::BothTimings),
        (Some(pattern), None) => {
            let zone_name = spec.tz.as_deref().unwrap_or("UTC");
            When::Cron {
                pattern: pattern.clone(),
                cron: Box::new(cron(pattern)?),
                zone: zone_name.to_owned(),
                tz: zone(zone_name)?,
            }
        }
        (None, Some(interval)) => {
            if spec.tz.is_some() {
                return Err(SpecError::ZoneWithInterval);
            }
            When::Interval {
                every_secs: interval_secs(interval)?,
            }
        }
    };
    let missed = match (spec.missed, spec.each_cap) {
        (Missed::Once, None) => MissedPolicy::Once,
        (Missed::Skip, None) => MissedPolicy::Skip,
        (Missed::Once | Missed::Skip, Some(_)) => return Err(SpecError::CapWithoutEach),
        (Missed::Each, cap) => {
            let cap = cap.unwrap_or(DEFAULT_EACH_CAP);
            if cap == 0 || cap > MAX_EACH_CAP {
                return Err(SpecError::CapOutOfRange(cap));
            }
            MissedPolicy::Each { cap }
        }
    };
    Ok(CompiledSchedule { when, missed })
}

/// Reads a spec from the JSON value found at `trigger.schedule`.
pub fn from_value(value: serde_json::Value) -> Result<ScheduleSpec, SpecError> {
    serde_json::from_value(value).map_err(|e| SpecError::Shape(e.to_string()))
}

/// Reads a spec from JSON text and validates it.
pub fn parse(json: &str) -> Result<CompiledSchedule, SpecError> {
    let spec: ScheduleSpec =
        serde_json::from_str(json).map_err(|e| SpecError::Shape(e.to_string()))?;
    validate(&spec)
}
