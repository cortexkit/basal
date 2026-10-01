//! The scheduler: due times in a named zone across daylight saving changes,
//! interval anchoring, missed fires, restarts, and the schedule lifecycle.
//!
//! Every test drives a manual clock: no test reads wall time, so machine
//! load cannot change an outcome.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use basal_core::hooks::Step;
use basal_core::schedule::spec::from_value;
use basal_core::schedule::{
    Approval, ManualClock, ScheduleState, ScheduledFlow, Scheduler, SchedulerConfig, TickHooks,
    TickPoint, validate,
};
use basal_core::{Admission, CoreError, Durability, Store};
use basal_testkit::harness::scratch;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("timestamp")
}

fn flow(id: &str, version: u64, spec: Value) -> ScheduledFlow {
    ScheduledFlow {
        flow_id: id.into(),
        version,
        spec: from_value(spec).expect("spec"),
        script: format!("return 'version {version}';"),
        manifest: format!("{{\"id\":\"{id}\",\"version\":{version}}}"),
    }
}

/// A store in a scratch directory, a manual clock, and a scheduler over
/// them that can be dropped and reopened like a restarted process.
struct Fixture {
    dir: PathBuf,
    clock: Arc<ManualClock>,
    sched: Option<Scheduler>,
}

fn open(dir: &Path, clock: &Arc<ManualClock>, hooks: Option<Arc<dyn TickHooks>>) -> Scheduler {
    let store = Store::open(dir.join("basal.db"), Durability { fullfsync: false }).expect("open");
    let sched = Scheduler::new(Arc::new(store), clock.clone(), SchedulerConfig::default());
    match hooks {
        Some(h) => sched.with_hooks(h),
        None => sched,
    }
}

impl Fixture {
    fn new(tag: &str, start: &str) -> Self {
        Self::with_hooks(tag, start, None)
    }

    fn with_hooks(tag: &str, start: &str, hooks: Option<Arc<dyn TickHooks>>) -> Self {
        let dir = scratch(tag);
        let clock = Arc::new(ManualClock::new(ts(start)));
        let sched = Some(open(&dir, &clock, hooks));
        Self { dir, clock, sched }
    }

    fn s(&self) -> &Scheduler {
        self.sched.as_ref().expect("scheduler open")
    }

    fn set(&self, now: &str) {
        self.clock.set(ts(now));
    }

    fn approve(&self, flow: &ScheduledFlow) -> Approval {
        self.s().approve(flow).expect("approve")
    }

    /// Ticks at `now` and returns the trigger ids of the new runs.
    fn tick(&self, now: &str) -> Vec<String> {
        self.set(now);
        new_runs(&self.s().tick().expect("tick"))
    }

    /// Drops the scheduler and its store, as a process exit does, and
    /// opens the same store again.
    fn restart(&mut self) {
        self.sched = None;
        self.sched = Some(open(&self.dir, &self.clock, None));
    }

    /// Every run admitted for `flow_id`, in admission order: its trigger
    /// id, the trigger payload the script sees, and the script it runs.
    fn fires(&self, flow_id: &str) -> Vec<(String, Value, String)> {
        self.s()
            .store()
            .read(|c| {
                let mut stmt = c.prepare(
                    "SELECT i.trigger_id, r.trigger, r.script FROM trigger_inbox i \
                     JOIN runs r ON r.run_id = i.run_id WHERE i.flow_id = ?1 ORDER BY i.rowid",
                )?;
                let rows = stmt
                    .query_map([flow_id], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .expect("fires")
            .into_iter()
            .map(|(id, trigger, script)| {
                (id, serde_json::from_str(&trigger).expect("trigger"), script)
            })
            .collect()
    }

    fn fire_ids(&self, flow_id: &str) -> Vec<String> {
        self.fires(flow_id).into_iter().map(|f| f.0).collect()
    }

    fn next_due(&self, flow_id: &str) -> Option<Timestamp> {
        self.s()
            .schedule(flow_id)
            .expect("schedule")
            .expect("exists")
            .next_due
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.sched = None;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn new_runs(report: &basal_core::schedule::TickReport) -> Vec<String> {
    report
        .admitted
        .iter()
        .filter(|f| matches!(f.admission, Admission::Admitted { .. }))
        .map(|f| f.trigger_id.clone())
        .collect()
}

fn id(due: &str) -> String {
    format!("schedule:{due}")
}

/// Ticks once a minute from `from` to `to` inclusive and returns, for
/// every new run, the tick time and its trigger id.
fn walk_minutes(fx: &Fixture, from: &str, to: &str) -> Vec<(Timestamp, String)> {
    let mut now = ts(from);
    let end = ts(to);
    let mut out = Vec::new();
    while now <= end {
        fx.clock.set(now);
        for trigger_id in new_runs(&fx.s().tick().expect("tick")) {
            out.push((now, trigger_id));
        }
        now = now
            .checked_add(SignedDuration::from_secs(60))
            .expect("minute");
    }
    out
}

fn catch_up(trigger: &Value) -> &Value {
    &trigger["catch_up"]
}

// ---- Due times in a named zone ----------------------------------------------

/// Europe/Madrid springs forward on 2026-03-29 at 02:00 CET (01:00 UTC):
/// the wall clock jumps to 03:00 CEST, so 02:30 does not exist that night.
#[test]
fn spring_forward_gap_fires_once_at_the_first_valid_instant() {
    let fx = Fixture::new("sched-spring", "2026-03-27T12:00:00Z");
    fx.approve(&flow(
        "madrid",
        1,
        json!({ "cron": "30 2 * * *", "tz": "Europe/Madrid" }),
    ));
    let fired = walk_minutes(&fx, "2026-03-28T00:00:00Z", "2026-03-30T03:00:00Z");
    assert_eq!(
        fired,
        vec![
            // 02:30 CET.
            (ts("2026-03-28T01:30:00Z"), id("2026-03-28T01:30:00Z")),
            // 02:30 does not exist; the first valid instant after the gap is
            // 03:00 CEST, the transition itself.
            (ts("2026-03-29T01:00:00Z"), id("2026-03-29T01:00:00Z")),
            // 02:30 CEST.
            (ts("2026-03-30T00:30:00Z"), id("2026-03-30T00:30:00Z")),
        ]
    );
    for (_, trigger, _) in fx.fires("madrid") {
        assert!(trigger.get("catch_up").is_none(), "{trigger}");
    }
}

/// Europe/Madrid falls back on 2026-10-25 at 03:00 CEST (01:00 UTC): the
/// wall clock returns to 02:00 CET, so 02:30 happens twice that night.
#[test]
fn fall_back_overlap_fires_once_at_the_first_occurrence() {
    let fx = Fixture::new("sched-fall", "2026-10-23T12:00:00Z");
    fx.approve(&flow(
        "madrid",
        1,
        json!({ "cron": "30 2 * * *", "tz": "Europe/Madrid" }),
    ));
    let fired = walk_minutes(&fx, "2026-10-24T00:00:00Z", "2026-10-26T03:00:00Z");
    assert_eq!(
        fired,
        vec![
            // 02:30 CEST.
            (ts("2026-10-24T00:30:00Z"), id("2026-10-24T00:30:00Z")),
            // 02:30 CEST, the first of the two; 02:30 CET (01:30 UTC) that
            // night fires nothing.
            (ts("2026-10-25T00:30:00Z"), id("2026-10-25T00:30:00Z")),
            // 02:30 CET.
            (ts("2026-10-26T01:30:00Z"), id("2026-10-26T01:30:00Z")),
        ]
    );
}

/// A half-hourly pattern across both changes: the matches inside the gap
/// collapse onto its end, and the repeated hour's second pass is skipped.
#[test]
fn half_hourly_cron_collapses_the_gap_and_skips_the_repeated_pass() {
    let compiled = validate(
        &from_value(json!({ "cron": "*/30 * * * *", "tz": "Europe/Madrid" })).expect("spec"),
    )
    .expect("valid");
    let walk = |from: &str, n: usize| {
        let mut at = ts(from);
        let mut out = Vec::new();
        for _ in 0..n {
            at = compiled
                .next_due_after(ts(from), at)
                .expect("due")
                .expect("fires again");
            out.push(at.to_string());
        }
        out
    };
    assert_eq!(
        walk("2026-03-29T00:00:00Z", 4),
        [
            "2026-03-29T00:30:00Z", // 01:30 CET
            "2026-03-29T01:00:00Z", // 02:00, 02:30 and 03:00 all land on 03:00 CEST
            "2026-03-29T01:30:00Z", // 03:30 CEST
            "2026-03-29T02:00:00Z", // 04:00 CEST
        ]
    );
    assert_eq!(
        walk("2026-10-24T23:30:00Z", 4),
        [
            "2026-10-25T00:00:00Z", // 02:00 CEST
            "2026-10-25T00:30:00Z", // 02:30 CEST
            // 02:00 CET and 02:30 CET (01:00 and 01:30 UTC) already fired.
            "2026-10-25T02:00:00Z", // 03:00 CET
            "2026-10-25T02:30:00Z", // 03:30 CET
        ]
    );
}

#[test]
fn interval_due_times_are_the_anchor_plus_whole_periods() {
    // Approved at 10:00:07.25; the anchor is the whole second, 10:00:07.
    let fx = Fixture::new("sched-interval", "2026-05-01T10:00:07.250Z");
    fx.approve(&flow("every", 1, json!({ "interval": "15m" })));
    assert_eq!(fx.next_due("every"), Some(ts("2026-05-01T10:15:07Z")));
    // A tick 30 s late, within the grace period: the fire is on time and
    // the next due time stays on the anchor's grid.
    assert_eq!(
        fx.tick("2026-05-01T10:15:37Z"),
        [id("2026-05-01T10:15:07Z")]
    );
    assert_eq!(fx.next_due("every"), Some(ts("2026-05-01T10:30:07Z")));
    assert!(fx.tick("2026-05-01T10:30:06Z").is_empty());
    assert_eq!(
        fx.tick("2026-05-01T10:30:07Z"),
        [id("2026-05-01T10:30:07Z")]
    );
    assert_eq!(fx.next_due("every"), Some(ts("2026-05-01T10:45:07Z")));
}

// ---- Missed fires -----------------------------------------------------------

/// An hourly schedule fires at 01:00, then the machine sleeps until 06:20:
/// 02:00 to 06:00 pass unseen (06:00 is 20 minutes old at the wake, beyond
/// the one-minute grace period).
fn sleep_through_five(fx: &Fixture, flow_id: &str, missed: Value) -> Vec<String> {
    let mut spec = json!({ "cron": "0 * * * *" });
    if let (Some(obj), Some(extra)) = (spec.as_object_mut(), missed.as_object()) {
        obj.extend(extra.clone());
    }
    fx.approve(&flow(flow_id, 1, spec));
    assert_eq!(
        fx.tick("2026-05-01T01:00:00Z"),
        [id("2026-05-01T01:00:00Z")]
    );
    fx.tick("2026-05-01T06:20:00Z")
}

#[test]
fn a_sleep_gap_under_once_makes_one_catch_up_fire_with_count_and_window() {
    let fx = Fixture::new("sched-once", "2026-05-01T00:00:30Z");
    let woke = sleep_through_five(&fx, "hourly", json!({ "missed": "once" }));
    assert_eq!(woke, [id("2026-05-01T06:00:00Z")]);
    let fires = fx.fires("hourly");
    assert_eq!(fires.len(), 2);
    let trigger = &fires[1].1;
    assert_eq!(
        trigger,
        &json!({
            "kind": "schedule",
            "due": "2026-05-01T06:00:00Z",
            "catch_up": {
                "policy": "once",
                "missed_count": 5,
                "missed_window": { "first": "2026-05-01T02:00:00Z", "last": "2026-05-01T06:00:00Z" },
            },
        })
    );
    assert_eq!(fx.next_due("hourly"), Some(ts("2026-05-01T07:00:00Z")));
    // Back on schedule.
    assert_eq!(
        fx.tick("2026-05-01T07:00:10Z"),
        [id("2026-05-01T07:00:00Z")]
    );
}

#[test]
fn a_sleep_gap_under_skip_makes_no_fire() {
    let fx = Fixture::new("sched-skip", "2026-05-01T00:00:30Z");
    let woke = sleep_through_five(&fx, "hourly", json!({ "missed": "skip" }));
    assert!(woke.is_empty(), "{woke:?}");
    assert_eq!(fx.fire_ids("hourly"), [id("2026-05-01T01:00:00Z")]);
    assert_eq!(fx.next_due("hourly"), Some(ts("2026-05-01T07:00:00Z")));
    assert_eq!(
        fx.tick("2026-05-01T07:00:10Z"),
        [id("2026-05-01T07:00:00Z")]
    );
}

#[test]
fn a_sleep_gap_under_each_fires_the_newest_up_to_the_cap_oldest_first() {
    let fx = Fixture::new("sched-each", "2026-05-01T00:00:30Z");
    let woke = sleep_through_five(&fx, "hourly", json!({ "missed": "each" }));
    // Five missed, cap 3: the newest three, oldest first.
    assert_eq!(
        woke,
        [
            id("2026-05-01T04:00:00Z"),
            id("2026-05-01T05:00:00Z"),
            id("2026-05-01T06:00:00Z"),
        ]
    );
    for (trigger_id, trigger, _) in fx.fires("hourly").into_iter().skip(1) {
        assert_eq!(
            trigger_id,
            format!("schedule:{}", trigger["due"].as_str().expect("due"))
        );
        assert_eq!(
            catch_up(&trigger),
            &json!({
                "policy": "each",
                "missed_count": 5,
                "missed_window": { "first": "2026-05-01T02:00:00Z", "last": "2026-05-01T06:00:00Z" },
            })
        );
    }
    assert_eq!(fx.next_due("hourly"), Some(ts("2026-05-01T07:00:00Z")));
}

/// Sleep counts as missed even when only one due time passed: a daily
/// schedule seen six hours late is a catch-up under `once` and nothing
/// under `skip`.
#[test]
fn a_single_due_time_seen_after_the_grace_period_is_missed() {
    let fx = Fixture::new("sched-late", "2026-05-01T12:00:00Z");
    fx.approve(&flow(
        "skip",
        1,
        json!({ "cron": "0 3 * * *", "missed": "skip" }),
    ));
    fx.approve(&flow(
        "once",
        1,
        json!({ "cron": "0 3 * * *", "missed": "once" }),
    ));
    let woke = fx.tick("2026-05-02T09:00:00Z");
    assert_eq!(woke, [id("2026-05-02T03:00:00Z")]);
    assert!(fx.fires("skip").is_empty());
    let once = fx.fires("once");
    assert_eq!(once.len(), 1);
    assert_eq!(
        catch_up(&once[0].1),
        &json!({
            "policy": "once",
            "missed_count": 1,
            "missed_window": { "first": "2026-05-02T03:00:00Z", "last": "2026-05-02T03:00:00Z" },
        })
    );
    assert_eq!(fx.next_due("skip"), Some(ts("2026-05-03T03:00:00Z")));
}

/// Two missed due times and an on-time one: the policy handles the missed
/// ones, and the on-time one fires as usual after them.
#[test]
fn an_on_time_due_time_fires_after_the_catch_up() {
    let fx = Fixture::new("sched-ontime", "2026-05-01T00:00:00Z");
    fx.approve(&flow("five", 1, json!({ "cron": "*/5 * * * *" })));
    let woke = fx.tick("2026-05-01T00:15:20Z");
    assert_eq!(
        woke,
        [id("2026-05-01T00:10:00Z"), id("2026-05-01T00:15:00Z")]
    );
    let fires = fx.fires("five");
    assert_eq!(catch_up(&fires[0].1)["missed_count"], 2);
    assert!(fires[1].1.get("catch_up").is_none());
}

/// Counting missed due times walks each one, so it stops at the configured
/// limit and says the count is a lower bound; the fire itself is still the
/// last missed due time.
#[test]
fn a_missed_count_past_the_limit_is_reported_as_a_lower_bound() {
    let dir = scratch("sched-limit");
    let clock = Arc::new(ManualClock::new(ts("2026-05-01T00:00:30Z")));
    let store = Store::open(dir.join("basal.db"), Durability { fullfsync: false }).expect("open");
    let config = SchedulerConfig {
        count_limit: 3,
        ..SchedulerConfig::default()
    };
    let sched = Scheduler::new(Arc::new(store), clock.clone(), config);
    sched
        .approve(&flow("tens", 1, json!({ "cron": "*/10 * * * *" })))
        .expect("approve");
    // Twelve due times, 00:10 to 02:00, all missed.
    clock.set(ts("2026-05-01T02:05:00Z"));
    let report = sched.tick().expect("tick");
    assert_eq!(new_runs(&report), [id("2026-05-01T02:00:00Z")]);
    let planned = &report.planned[0].fires[0];
    assert_eq!(
        planned.payload["catch_up"],
        json!({
            "policy": "once",
            "missed_count": 3,
            "missed_count_is_lower_bound": true,
            "missed_window": { "first": "2026-05-01T00:10:00Z", "last": "2026-05-01T02:00:00Z" },
        })
    );
    drop(sched);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- Restarts and repeated ticks ---------------------------------------------

/// Crashes the process (cuts the store) the first time the scheduler
/// reaches a point of the given kind.
struct CrashAt {
    planned: bool,
    fired: AtomicBool,
}

impl TickHooks for CrashAt {
    fn at(&self, point: &TickPoint) -> Step {
        let hit = match point {
            TickPoint::Planned { .. } => self.planned,
            TickPoint::Admitted { .. } => !self.planned,
        };
        if hit && !self.fired.swap(true, Ordering::SeqCst) {
            Step::Crash
        } else {
            Step::Continue
        }
    }
}

fn crash_at(planned: bool) -> Option<Arc<dyn TickHooks>> {
    Some(Arc::new(CrashAt {
        planned,
        fired: AtomicBool::new(false),
    }))
}

#[test]
fn a_restart_after_planning_a_catch_up_mints_no_second_fire() {
    let mut fx = Fixture::new("sched-restart", "2026-05-01T00:00:30Z");
    fx.approve(&flow("hourly", 1, json!({ "cron": "0 * * * *" })));
    assert_eq!(
        fx.tick("2026-05-01T01:00:00Z"),
        [id("2026-05-01T01:00:00Z")]
    );
    // From here on, the process dies at its next plan commit.
    fx.sched = None;
    fx.sched = Some(open(&fx.dir, &fx.clock, crash_at(true)));

    // Wake at 04:20: 02:00 to 04:00 were missed. The catch-up is planned
    // and the schedule advanced, then the process dies before admitting.
    fx.set("2026-05-01T04:20:00Z");
    let err = fx.s().tick().expect_err("crash");
    assert_eq!(err, CoreError::Cut);
    fx.restart();
    assert_eq!(fx.fire_ids("hourly"), [id("2026-05-01T01:00:00Z")]);

    // The restarted process ticks later, after 05:00 has also passed.
    let woke = fx.tick("2026-05-01T05:10:00Z");
    assert_eq!(
        woke,
        [id("2026-05-01T04:00:00Z"), id("2026-05-01T05:00:00Z")]
    );
    let fires = fx.fires("hourly");
    assert_eq!(fires.len(), 3);
    // The catch-up planned before the crash, unchanged.
    assert_eq!(
        catch_up(&fires[1].1),
        &json!({
            "policy": "once",
            "missed_count": 3,
            "missed_window": { "first": "2026-05-01T02:00:00Z", "last": "2026-05-01T04:00:00Z" },
        })
    );
    // 05:00 alone, ten minutes late: not counted again with 02:00 to 04:00.
    assert_eq!(
        catch_up(&fires[2].1),
        &json!({
            "policy": "once",
            "missed_count": 1,
            "missed_window": { "first": "2026-05-01T05:00:00Z", "last": "2026-05-01T05:00:00Z" },
        })
    );
}

#[test]
fn a_restart_between_admitting_planned_fires_admits_each_once() {
    let mut fx = Fixture::with_hooks(
        "sched-restart-each",
        "2026-05-01T00:00:30Z",
        crash_at(false),
    );
    fx.approve(&flow(
        "hourly",
        1,
        json!({ "cron": "0 * * * *", "missed": "each" }),
    ));
    fx.set("2026-05-01T06:20:00Z");
    let err = fx.s().tick().expect_err("crash after the first admission");
    assert_eq!(err, CoreError::Cut);
    fx.restart();
    assert_eq!(fx.fire_ids("hourly"), [id("2026-05-01T04:00:00Z")]);
    let woke = fx.tick("2026-05-01T06:20:00Z");
    assert_eq!(
        woke,
        [id("2026-05-01T05:00:00Z"), id("2026-05-01T06:00:00Z")]
    );
    assert_eq!(
        fx.fire_ids("hourly"),
        [
            id("2026-05-01T04:00:00Z"),
            id("2026-05-01T05:00:00Z"),
            id("2026-05-01T06:00:00Z"),
        ]
    );
}

#[test]
fn a_repeated_tick_with_the_same_now_admits_nothing_new() {
    let fx = Fixture::new("sched-repeat", "2026-05-01T00:00:30Z");
    fx.approve(&flow(
        "hourly",
        1,
        json!({ "cron": "0 * * * *", "missed": "each" }),
    ));
    let first = fx.tick("2026-05-01T04:20:00Z");
    assert_eq!(first.len(), 3);
    let before = fx.fire_ids("hourly");
    for _ in 0..3 {
        let again = fx.s().tick().expect("tick again");
        assert!(again.admitted.is_empty(), "{again:?}");
    }
    assert_eq!(fx.fire_ids("hourly"), before);
}

// ---- Lifecycle ---------------------------------------------------------------

#[test]
fn approving_a_newer_version_does_not_fire_for_the_gap_the_swap_creates() {
    let fx = Fixture::new("sched-replace", "2026-05-01T00:00:30Z");
    fx.approve(&flow("f", 1, json!({ "cron": "0 * * * *" })));
    assert_eq!(
        fx.tick("2026-05-01T01:00:00Z"),
        [id("2026-05-01T01:00:00Z")]
    );
    // At 01:30 a quarter-hourly version is approved. Its 01:15 and 01:30
    // fell before it was approved: they are not its to fire.
    fx.set("2026-05-01T01:30:00Z");
    assert_eq!(
        fx.approve(&flow("f", 2, json!({ "cron": "*/15 * * * *" }))),
        Approval::Replaced {
            previous_version: 1
        }
    );
    assert_eq!(fx.next_due("f"), Some(ts("2026-05-01T01:45:00Z")));
    assert!(fx.tick("2026-05-01T01:30:00Z").is_empty());
    assert_eq!(
        fx.tick("2026-05-01T01:45:00Z"),
        [id("2026-05-01T01:45:00Z")]
    );
    let scripts: Vec<String> = fx.fires("f").into_iter().map(|f| f.2).collect();
    assert_eq!(scripts, ["return 'version 1';", "return 'version 2';"]);
}

#[test]
fn approving_a_newer_version_first_fires_what_the_old_version_owed() {
    let fx = Fixture::new("sched-replace-owed", "2026-05-01T00:00:30Z");
    fx.approve(&flow("f", 1, json!({ "cron": "0 * * * *" })));
    // 01:00 has passed but no tick has run yet when version 2 arrives.
    fx.set("2026-05-01T01:00:20Z");
    fx.approve(&flow("f", 2, json!({ "cron": "30 * * * *" })));
    let fires = fx.fires("f");
    assert_eq!(fires.len(), 1);
    assert_eq!(fires[0].0, id("2026-05-01T01:00:00Z"));
    assert_eq!(fires[0].2, "return 'version 1';");
    assert_eq!(fx.next_due("f"), Some(ts("2026-05-01T01:30:00Z")));
}

#[test]
fn approval_is_idempotent_and_refuses_older_or_conflicting_versions() {
    let fx = Fixture::new("sched-approve", "2026-05-01T00:00:30Z");
    let v2 = flow("f", 2, json!({ "cron": "0 * * * *" }));
    assert_eq!(fx.approve(&v2), Approval::Created);
    assert_eq!(fx.approve(&v2), Approval::Unchanged);
    let older = fx
        .s()
        .approve(&flow("f", 1, json!({ "cron": "0 * * * *" })));
    assert!(matches!(older, Err(CoreError::Invalid(_))), "{older:?}");
    let mut conflicting = v2.clone();
    conflicting.script = "return 'other';".into();
    let conflict = fx.s().approve(&conflicting);
    assert!(
        matches!(conflict, Err(CoreError::Invalid(_))),
        "{conflict:?}"
    );
    let invalid = fx.s().approve(&ScheduledFlow {
        spec: from_value(json!({ "cron": "0 * * * *", "tz": "Nowhere/Land" })).expect("shape"),
        ..flow("g", 1, json!({ "cron": "0 * * * *" }))
    });
    assert!(matches!(invalid, Err(CoreError::Invalid(_))), "{invalid:?}");
    assert!(fx.s().schedule("g").expect("read").is_none());
}

#[test]
fn a_disabled_schedule_does_not_tick_and_does_not_catch_up_when_enabled() {
    let fx = Fixture::new("sched-disable", "2026-05-01T00:00:30Z");
    fx.approve(&flow("f", 1, json!({ "cron": "0 * * * *" })));
    fx.set("2026-05-01T00:30:00Z");
    assert!(fx.s().disable("f").expect("disable"));
    for now in [
        "2026-05-01T01:00:00Z",
        "2026-05-01T02:00:00Z",
        "2026-05-01T03:30:00Z",
    ] {
        assert!(fx.tick(now).is_empty(), "{now}");
    }
    let row = fx.s().schedule("f").expect("read").expect("row");
    assert_eq!(row.state, ScheduleState::Disabled);

    // Enabled again at 03:40: the hours it spent disabled are not missed.
    fx.set("2026-05-01T03:40:00Z");
    assert!(fx.s().enable("f").expect("enable"));
    assert_eq!(fx.next_due("f"), Some(ts("2026-05-01T04:00:00Z")));
    assert!(fx.tick("2026-05-01T03:40:00Z").is_empty());
    assert_eq!(
        fx.tick("2026-05-01T04:00:00Z"),
        [id("2026-05-01T04:00:00Z")]
    );
    assert_eq!(fx.fire_ids("f"), [id("2026-05-01T04:00:00Z")]);
}

#[test]
fn removing_a_schedule_stops_it_and_drops_its_planned_fires() {
    let fx = Fixture::new("sched-remove", "2026-05-01T00:00:30Z");
    fx.approve(&flow("f", 1, json!({ "cron": "0 * * * *" })));
    // While admission drains, the 01:00 fire is planned and waits.
    fx.s()
        .store()
        .write(|tx| basal_core::admission::set_draining(tx, true))
        .expect("drain");
    fx.set("2026-05-01T01:00:00Z");
    let report = fx.s().tick().expect("tick");
    assert_eq!(report.waiting, 1);
    assert!(fx.s().remove("f").expect("remove"));
    assert!(fx.s().schedule("f").expect("read").is_none());
    fx.s()
        .store()
        .write(|tx| basal_core::admission::set_draining(tx, false))
        .expect("undrain");
    assert!(fx.tick("2026-05-01T02:00:00Z").is_empty());
    assert!(fx.fires("f").is_empty());
}

#[test]
fn fires_planned_while_admission_drains_wait_and_are_admitted_after() {
    let fx = Fixture::new("sched-drain", "2026-05-01T00:00:30Z");
    fx.approve(&flow("f", 1, json!({ "cron": "0 * * * *" })));
    fx.s()
        .store()
        .write(|tx| basal_core::admission::set_draining(tx, true))
        .expect("drain");
    fx.set("2026-05-01T01:00:00Z");
    let report = fx.s().tick().expect("tick");
    assert!(report.admitted.is_empty());
    assert_eq!(report.waiting, 1);
    fx.s()
        .store()
        .write(|tx| basal_core::admission::set_draining(tx, false))
        .expect("undrain");
    // Admitted at the next tick as planned: on time, not a catch-up.
    assert_eq!(
        fx.tick("2026-05-01T01:40:00Z"),
        [id("2026-05-01T01:00:00Z")]
    );
    let fires = fx.fires("f");
    assert!(fires[0].1.get("catch_up").is_none());
}
