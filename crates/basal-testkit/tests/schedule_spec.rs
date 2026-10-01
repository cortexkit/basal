//! Schedule specs: what `trigger.schedule` accepts and what it refuses.

use basal_core::schedule::spec::{from_value, parse};
use basal_core::schedule::{
    DEFAULT_EACH_CAP, MAX_EACH_CAP, MissedPolicy, ScheduleSpec, SpecError, When, validate,
};
use serde_json::json;

fn check(value: serde_json::Value) -> Result<basal_core::schedule::CompiledSchedule, SpecError> {
    validate(&from_value(value)?)
}

#[test]
fn valid_specs_compile_with_their_defaults() {
    let cron = check(json!({ "cron": "*/30 * * * *", "tz": "Europe/Madrid" })).expect("cron");
    assert_eq!(cron.missed, MissedPolicy::Once);
    match &cron.when {
        When::Cron { zone, pattern, .. } => {
            assert_eq!(zone, "Europe/Madrid");
            assert_eq!(pattern, "*/30 * * * *");
        }
        other => panic!("{other:?}"),
    }

    // No zone means UTC, never the host's local zone.
    let utc = check(json!({ "cron": "0 3 * * *", "missed": "skip" })).expect("utc");
    assert_eq!(utc.missed, MissedPolicy::Skip);
    match &utc.when {
        When::Cron { zone, tz, .. } => {
            assert_eq!(zone, "UTC");
            assert_eq!(tz.iana_name(), Some("UTC"));
        }
        other => panic!("{other:?}"),
    }

    let each = check(json!({ "interval": "15m", "missed": "each" })).expect("each");
    assert_eq!(
        each.missed,
        MissedPolicy::Each {
            cap: DEFAULT_EACH_CAP
        }
    );
    assert!(matches!(each.when, When::Interval { every_secs: 900 }));

    let capped = check(json!({ "interval": "1d", "missed": "each", "each_cap": MAX_EACH_CAP }))
        .expect("capped");
    assert_eq!(capped.missed, MissedPolicy::Each { cap: MAX_EACH_CAP });

    for (text, secs) in [("60s", 60), ("2h", 7200), ("365d", 365 * 86_400)] {
        let c = check(json!({ "interval": text })).expect(text);
        assert!(
            matches!(c.when, When::Interval { every_secs } if every_secs == secs),
            "{text}"
        );
    }

    // The spec round-trips through JSON unchanged.
    let spec: ScheduleSpec =
        from_value(json!({ "cron": "0 * * * *", "tz": "Asia/Tokyo", "missed": "once" }))
            .expect("spec");
    let text = serde_json::to_string(&spec).expect("serialise");
    parse(&text).expect("parse again");
}

#[test]
fn validation_refuses_malformed_cron() {
    for pattern in [
        "",
        "* * * *",
        "61 * * * *",
        "* 24 * * *",
        "* * * 13 *",
        "not a pattern",
        // Seconds and years are refused: a minute is the finest grain.
        "0 0 * * * *",
        "0 0 1 1 * 2030",
    ] {
        let err = check(json!({ "cron": pattern })).expect_err(pattern);
        assert!(matches!(err, SpecError::Cron { .. }), "{pattern}: {err:?}");
    }
}

#[test]
fn validation_refuses_a_cron_that_never_fires() {
    let err = check(json!({ "cron": "0 0 31 2 *" })).expect_err("31 February");
    assert!(matches!(err, SpecError::CronNeverFires { .. }), "{err:?}");
}

#[test]
fn validation_refuses_unknown_zones() {
    for zone in [
        "Europe/Atlantis",
        "Mars/Olympus_Mons",
        "",
        // The database matches names case-insensitively; the spec does not,
        // so the name on the card is the name the scheduler uses.
        "europe/madrid",
        "Etc/Unknown",
    ] {
        let err = check(json!({ "cron": "0 * * * *", "tz": zone })).expect_err(zone);
        assert!(matches!(err, SpecError::UnknownZone(_)), "{zone}: {err:?}");
    }
    let err = check(json!({ "interval": "1h", "tz": "Europe/Madrid" })).expect_err("tz");
    assert_eq!(err, SpecError::ZoneWithInterval);
}

#[test]
fn validation_refuses_zero_and_out_of_range_intervals() {
    for interval in ["0s", "0m", "0d", "59s", "366d", "99999999999999999999d"] {
        let err = check(json!({ "interval": interval })).expect_err(interval);
        assert!(
            matches!(err, SpecError::IntervalOutOfRange { .. }),
            "{interval}: {err:?}"
        );
    }
    for interval in ["", "m", "15", "15 m", "1h30m", "-5m", "1.5h", "15M", "15ms"] {
        let err = check(json!({ "interval": interval })).expect_err(interval);
        assert!(
            matches!(err, SpecError::IntervalSyntax(_)),
            "{interval}: {err:?}"
        );
    }
}

#[test]
fn validation_refuses_a_missed_cap_above_the_limit() {
    for cap in [0, MAX_EACH_CAP + 1, 1000] {
        let err =
            check(json!({ "interval": "1h", "missed": "each", "each_cap": cap })).expect_err("cap");
        assert_eq!(err, SpecError::CapOutOfRange(cap));
    }
    let err =
        check(json!({ "interval": "1h", "missed": "once", "each_cap": 2 })).expect_err("once");
    assert_eq!(err, SpecError::CapWithoutEach);
    let err = check(json!({ "interval": "1h", "missed": "each", "each_cap": -1 })).expect_err("-1");
    assert!(matches!(err, SpecError::Shape(_)), "{err:?}");
}

#[test]
fn validation_refuses_unknown_fields_and_values() {
    for value in [
        json!({ "cron": "0 * * * *", "timezone": "Europe/Madrid" }),
        json!({ "cron": "0 * * * *", "jitter": "5m" }),
        json!({ "interval": "1h", "missed": "all" }),
        json!({ "interval": "1h", "missed": "Once" }),
        json!({ "cron": 5 }),
        json!("0 * * * *"),
    ] {
        let err = check(value.clone()).expect_err("shape");
        assert!(matches!(err, SpecError::Shape(_)), "{value}: {err:?}");
    }
    assert_eq!(check(json!({})).expect_err("empty"), SpecError::NoTiming);
    assert_eq!(
        check(json!({ "cron": "0 * * * *", "interval": "1h" })).expect_err("both"),
        SpecError::BothTimings
    );
}
