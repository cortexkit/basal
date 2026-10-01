//! Schedules wired to installs and admission: a scheduled flow from install
//! through approval, a tick, admission and its run; owed fires after a
//! version swap running the version approved when they are admitted;
//! disable and enable around a due time; and a scheduled fire refused by
//! the run rate limit, dropped and recorded. Every test drives a manual
//! clock shared by the runtime and the scheduler.

mod common;

use std::sync::Arc;
use std::time::Duration;

use basal_core::schedule::{Approval, ScheduleState, Scheduler, fire_trigger_id};
use basal_core::{Actor, Clock, Config, NoHooks, RateLimits, RunState, Runtime};
use basal_proto::JsonText;
use basal_testkit::harness::{World, approve_spec, test_manifest};
use common::{config, finish, result};
use jiff::Timestamp;
use serde_json::{Value, json};

fn ts(text: &str) -> Timestamp {
    text.parse().expect("timestamp")
}

fn id(due: &str) -> String {
    fire_trigger_id(ts(due))
}

/// A runtime over the world's store whose clock starts at `start`, and a
/// scheduler sharing that clock.
fn open(
    world: &World,
    start: &str,
    change: impl FnOnce(&mut Config),
) -> (Runtime, Scheduler, Clock) {
    let clock = Clock::manual(ts(start).as_millisecond());
    let mut config = Config {
        clock: clock.clone(),
        ..config()
    };
    change(&mut config);
    let rt = world.runtime(Arc::new(NoHooks), config).expect("runtime");
    let sched = rt.scheduler().expect("scheduler");
    (rt, sched, clock)
}

fn scheduled(spec: Value) -> Value {
    let mut m = test_manifest();
    m["trigger"] = json!({ "schedule": spec });
    m
}

/// Installs and approves `script` under `manifest` as the next version.
fn approve(rt: &Runtime, script: &str, manifest: &Value) {
    approve_spec(rt, script, manifest, "unused", JsonText::null()).expect("approve");
}

/// The runs admitted for the test flow, as (trigger id, run id), in order.
fn runs(rt: &Runtime) -> Vec<(String, String)> {
    rt.store()
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT trigger_id, run_id FROM trigger_inbox WHERE flow_id = 'flow-test' \
                 ORDER BY rowid",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .expect("runs")
}

fn set(clock: &Clock, at: &str) {
    clock.set(ts(at).as_millisecond());
}

#[test]
fn a_scheduled_flow_runs_from_install_through_tick_admission_and_run() {
    let world = World::new("sched-e2e");
    let (rt, sched, clock) = open(&world, "2026-05-01T00:00:00Z", |_| {});
    approve(
        &rt,
        "return { kind: trigger.kind, due: trigger.due };",
        &scheduled(json!({ "interval": "15m" })),
    );
    let row = sched
        .schedule("flow-test")
        .expect("read")
        .expect("approval created the schedule");
    assert_eq!(row.next_due, Some(ts("2026-05-01T00:15:00Z")));

    // Nothing is due before 00:15.
    set(&clock, "2026-05-01T00:14:00Z");
    assert!(sched.tick().expect("tick").admitted.is_empty());
    set(&clock, "2026-05-01T00:15:10Z");
    let report = sched.tick().expect("tick");
    let new_runs = report.new_runs();
    assert_eq!(new_runs.len(), 1, "{report:?}");
    let run_id = new_runs[0].to_owned();
    // The same tick again admits nothing new.
    assert!(sched.tick().expect("tick").admitted.is_empty());

    let run = finish(&rt, &world, &run_id);
    assert_eq!(run.state, RunState::Succeeded, "{run:#?}");
    assert_eq!(run.trigger_id, id("2026-05-01T00:15:00Z"));
    assert_eq!(run.flow_version, Some(1));
    assert_eq!(
        result(&run),
        json!({ "kind": "schedule", "due": "2026-05-01T00:15:00Z" })
    );
}

/// Version 1 owed its 01:00 fire when version 2 was approved at 01:00:20.
/// The approval plans it; it is admitted at the next tick, under version 2,
/// the version approved when it is admitted.
#[test]
fn owed_fires_after_a_version_swap_run_the_version_approved_at_admission() {
    let world = World::new("sched-swap");
    let (rt, sched, clock) = open(&world, "2026-05-01T00:00:30Z", |_| {});
    approve(
        &rt,
        "return 'version 1';",
        &scheduled(json!({ "cron": "0 * * * *" })),
    );
    set(&clock, "2026-05-01T01:00:20Z");
    approve(
        &rt,
        "return 'version 2';",
        &scheduled(json!({ "cron": "30 * * * *" })),
    );
    assert!(
        runs(&rt).is_empty(),
        "the owed fire is planned, not admitted"
    );
    let report = sched.tick().expect("tick");
    assert_eq!(report.new_runs().len(), 1, "{report:?}");
    let admitted = runs(&rt);
    assert_eq!(admitted[0].0, id("2026-05-01T01:00:00Z"));
    let run = finish(&rt, &world, &admitted[0].1);
    assert_eq!(run.flow_version, Some(2));
    assert_eq!(result(&run), json!("version 2"));
    // Version 2's own pattern takes over from its approval.
    assert_eq!(
        sched
            .schedule("flow-test")
            .expect("read")
            .expect("row")
            .next_due,
        Some(ts("2026-05-01T01:30:00Z"))
    );
}

/// Disabled at 00:30, the schedule does not fire at 01:00 and nothing is
/// planned or dropped for it; enabled at 01:30, it fires at 02:00 and the
/// hour it spent disabled is not a catch-up.
#[test]
fn disabling_and_enabling_a_flow_stops_and_restarts_its_schedule() {
    let world = World::new("sched-disable");
    let (rt, sched, clock) = open(&world, "2026-05-01T00:00:30Z", |_| {});
    approve(&rt, "return 1;", &scheduled(json!({ "cron": "0 * * * *" })));
    set(&clock, "2026-05-01T00:30:00Z");
    assert!(
        rt.disable_flow("flow-test", &Actor::Operator("ufuk".into()), "maintenance")
            .expect("disable")
    );
    assert_eq!(
        sched
            .schedule("flow-test")
            .expect("read")
            .expect("row")
            .state,
        ScheduleState::Disabled
    );
    set(&clock, "2026-05-01T01:00:00Z");
    let report = sched.tick().expect("tick");
    assert!(
        report.planned.is_empty() && report.admitted.is_empty(),
        "{report:?}"
    );
    assert!(sched.dropped().expect("dropped").is_empty());

    set(&clock, "2026-05-01T01:30:00Z");
    assert!(rt.enable_flow("flow-test").expect("enable"));
    let row = sched.schedule("flow-test").expect("read").expect("row");
    assert_eq!(row.state, ScheduleState::Active);
    assert_eq!(row.next_due, Some(ts("2026-05-01T02:00:00Z")));
    set(&clock, "2026-05-01T02:00:00Z");
    let report = sched.tick().expect("tick");
    assert_eq!(report.new_runs().len(), 1, "{report:?}");
    let admitted = runs(&rt);
    assert_eq!(admitted.len(), 1);
    assert_eq!(admitted[0].0, id("2026-05-01T02:00:00Z"));
    let run = rt.run(&admitted[0].1).expect("run");
    let trigger: Value = serde_json::from_str(run.trigger.as_str()).expect("trigger");
    assert!(trigger.get("catch_up").is_none(), "{trigger}");
}

/// Three missed hourly fires under `each`, with a limit of one run per rate
/// window: the first is admitted, the other two are refused by the run
/// rate limit, dropped, and recorded with their reason.
#[test]
fn a_rate_limited_scheduled_fire_is_dropped_and_recorded() {
    let world = World::new("sched-rate");
    let (rt, sched, clock) = open(&world, "2026-05-01T00:00:30Z", |c| {
        c.rate = RateLimits {
            window: Duration::from_secs(60),
            max_runs: 1,
            ..RateLimits::default()
        };
    });
    approve(
        &rt,
        "return 1;",
        &scheduled(json!({ "cron": "0 * * * *", "missed": "each", "each_cap": 3 })),
    );
    set(&clock, "2026-05-01T03:20:00Z");
    let report = sched.tick().expect("tick");
    assert_eq!(report.admitted.len(), 3, "{report:?}");
    assert_eq!(report.new_runs().len(), 1, "{report:?}");
    assert_eq!(report.waiting, 0, "refused fires are not kept");
    assert_eq!(
        runs(&rt).into_iter().map(|r| r.0).collect::<Vec<_>>(),
        [id("2026-05-01T01:00:00Z")]
    );
    let dropped = sched.dropped().expect("dropped");
    let dropped: Vec<(String, String)> = dropped
        .into_iter()
        .map(|d| (d.trigger_id, d.reason))
        .collect();
    assert_eq!(
        dropped,
        [
            (id("2026-05-01T02:00:00Z"), "rate_limited".to_owned()),
            (id("2026-05-01T03:00:00Z"), "rate_limited".to_owned()),
        ]
    );
    // A later tick does not bring them back.
    set(&clock, "2026-05-01T03:30:00Z");
    assert!(sched.tick().expect("tick").admitted.is_empty());
    assert_eq!(runs(&rt).len(), 1);
}

/// A version swap reports what happened to the schedule.
#[test]
fn approving_versions_reports_the_schedule_change() {
    let world = World::new("sched-approval");
    let (rt, sched, _clock) = open(&world, "2026-05-01T00:00:30Z", |_| {});
    let install = |script: &str, manifest: &Value| {
        let spec =
            approve_spec(&rt, script, manifest, "unused", JsonText::null()).expect("approve");
        let installed = rt
            .install(&basal_core::InstallRequest {
                script: spec.script.clone(),
                manifest: spec.manifest.clone(),
                author: "ALF".into(),
                loop_override: false,
            })
            .expect("same version installs again");
        let version = basal_core::Manifest::parse(&spec.manifest)
            .expect("manifest")
            .version;
        rt.approve("flow-test", version, &installed.code_hash, "again")
            .expect("approve again")
    };
    assert_eq!(
        install("return 1;", &scheduled(json!({ "cron": "0 * * * *" }))),
        Some(Approval::Unchanged)
    );
    assert_eq!(install("return 2;", &test_manifest()), None);
    assert!(sched.schedule("flow-test").expect("read").is_none());
}
