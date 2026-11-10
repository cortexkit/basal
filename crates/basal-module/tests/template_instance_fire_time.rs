mod common;

use basal_module::caller::Caller;
use common::{Fixture, Options, T0, agent, events_manifest, fixture};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const OWNER: &str = "agent_47120287c700722b";
const DIRECTORY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../test-vectors/template-instance-fire-time-v1"
);

fn setup(tag: &str) -> Fixture {
    let f = fixture(tag, Options::default());
    f.catalog.add_named_agent(OWNER, "Owner");
    f
}

fn register(f: &Fixture, manifest: &Value, script: &str) {
    f.module
        .handle(
            &Caller::Operator,
            "package.register",
            json!({"manifest":manifest.to_string(),"script":script}),
        )
        .unwrap();
}

fn recurring(interval: &str, version: u32) -> Value {
    let mut m = events_manifest("watch");
    m["version"] = json!(version);
    m["sinks"][0]["agent"] = json!("$self");
    m["params"] = json!({"note":{"type":"string","max_bytes":4096}});
    m["template"] = json!({"max_per_agent":10,"max_lifetime":"1d"});
    m["trigger"] = json!({"schedule":{"interval":interval}});
    m
}

fn waitable(interval: &str, version: u32) -> Value {
    let mut m = recurring(interval, version);
    m["sinks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"kind":"wait"}));
    m
}

fn create(f: &Fixture, template: &str, version: u32, params: Value, lifetime: &str) -> Value {
    let reply = f
        .module
        .handle(
            &agent(OWNER),
            "flow.instantiate",
            json!({"template":template,"version":version,"params":params,"expires_in":lifetime}),
        )
        .unwrap();
    assert!(reply.get("next_fire_at_ms").is_some());
    reply
}

fn watch(f: &Fixture, version: u32, lifetime: &str) -> Value {
    create(f, "watch", version, json!({"note":"watch"}), lifetime)
}

fn id(reply: &Value) -> &str {
    reply["flow_id"].as_str().unwrap()
}

fn body_bytes(f: &Fixture, flow_id: &str, seq: i64, event: &str) -> String {
    let reply = f
        .module
        .handle(
            &Caller::Core,
            "events_get",
            json!({"event_key":format!("{flow_id}:{seq}"),"event_name":event,"version":1}),
        )
        .unwrap();
    let body = reply["body"].as_str().unwrap();
    assert_eq!(reply["digest"], format!("{:x}", Sha256::digest(body)));
    body.to_owned()
}

fn body(f: &Fixture, flow_id: &str, seq: i64, event: &str) -> Value {
    let body: Value = serde_json::from_str(&body_bytes(f, flow_id, seq, event)).unwrap();
    assert!(body.get("next_fire_at_ms").is_some());
    body
}

fn live(f: &Fixture) -> Value {
    let instance = f
        .module
        .handle(&Caller::Core, "flow.instance.live", json!({}))
        .unwrap()["instances"][0]
        .clone();
    assert!(instance.get("next_fire_at_ms").is_some());
    instance
}

fn remind(f: &Fixture) -> Value {
    register(
        f,
        &serde_json::from_str(include_str!("../../../templates/remind-at/manifest.json")).unwrap(),
        include_str!("../../../templates/remind-at/flow.js"),
    );
    create(
        f,
        "remind-at",
        4,
        json!({"at":"2026-05-01T01:00:00Z","note":"Review the release"}),
        "1d",
    )
}

#[test]
fn next_fire_at_remind_at_reply_created_live_and_vectors_agree() {
    let f = setup("next-fire-reminder");
    let reply = remind(&f);
    assert_eq!(reply["next_fire_at_ms"], T0 + 3_600_000);
    let created_bytes = body_bytes(&f, id(&reply), 1, "flow_instance_created");
    let created: Value = serde_json::from_str(&created_bytes).unwrap();
    assert_eq!(created["next_fire_at_ms"], reply["next_fire_at_ms"]);
    assert_eq!(live(&f), created);
    let mut old_reply = reply.clone();
    old_reply.as_object_mut().unwrap().remove("next_fire_at_ms");
    assert_eq!(
        old_reply,
        json!({"flow_id":id(&reply),"template":"remind-at","version":4,"agent_id":OWNER,
            "params":{"at":"2026-05-01T01:00:00Z","note":"Review the release"},
            "expires_at":T0+86_400_000,"new":true})
    );
    let mut old_created = created;
    old_created
        .as_object_mut()
        .unwrap()
        .remove("next_fire_at_ms");
    assert_eq!(
        old_created,
        json!({"flow_id":id(&reply),"template":"remind-at","version":4,"agent_id":OWNER,
            "expires_at":T0+86_400_000,"seq":1})
    );

    // Preserve the real handlers' canonical JSON bytes, with one file newline.
    let repeating = setup("next-fire-rescheduled-vector");
    register(&repeating, &waitable("1m", 1), "return 1;");
    let instance = watch(&repeating, 1, "1h");
    repeating.clock.advance(60_000);
    repeating.module.rt.scheduler().unwrap().tick().unwrap();
    let rescheduled_bytes = body_bytes(&repeating, id(&instance), 2, "flow_instance_rescheduled");
    let samples = [
        ("instantiate-reply.json", format!("{reply}\n")),
        ("created-event-body.json", format!("{created_bytes}\n")),
        (
            "rescheduled-event-body.json",
            format!("{rescheduled_bytes}\n"),
        ),
    ];
    let sums = samples
        .iter()
        .map(|(name, bytes)| format!("{:x}  {name}\n", Sha256::digest(bytes)))
        .collect::<String>();
    if std::env::var_os("BASAL_WRITE_VECTORS").is_some() {
        std::fs::create_dir_all(DIRECTORY).unwrap();
        for (name, bytes) in &samples {
            std::fs::write(format!("{DIRECTORY}/{name}"), bytes).unwrap();
        }
        std::fs::write(format!("{DIRECTORY}/SHA256SUMS"), &sums).unwrap();
    }
    for (name, bytes) in &samples {
        assert_eq!(
            std::fs::read(format!("{DIRECTORY}/{name}")).unwrap(),
            bytes.as_bytes(),
            "regenerate with BASAL_WRITE_VECTORS=1"
        );
    }
    assert_eq!(
        std::fs::read_to_string(format!("{DIRECTORY}/SHA256SUMS")).unwrap(),
        sums
    );
}

#[test]
fn next_fire_at_waitable_interval_reschedules_once_per_advance() {
    let f = setup("next-fire-rescheduled-interval");
    register(&f, &waitable("1m", 1), "return 1;");
    let first = watch(&f, 1, "1h");
    // A newer unapproved package must not remove the instance's wait grant.
    register(&f, &recurring("1m", 2), "return 2;");
    let scheduler = f.module.rt.scheduler().unwrap();
    for advance in 1..=2 {
        f.clock.set(T0 + advance * 60_000);
        assert_eq!(scheduler.tick().unwrap().planned.len(), 1);
        assert!(scheduler.tick().unwrap().planned.is_empty());
        let events = f.module.rt.pending_template_events(32).unwrap();
        let rescheduled: Vec<_> = events
            .iter()
            .filter(|e| e.event_name == "flow_instance_rescheduled")
            .collect();
        assert_eq!(rescheduled.len(), advance as usize);
        let event = rescheduled.last().unwrap();
        assert_eq!(event.seq, advance + 1);
        let snapshot = body(&f, id(&first), event.seq, "flow_instance_rescheduled");
        assert_eq!(
            snapshot,
            json!({
                "flow_id":id(&first),"template":"watch","version":1,"agent_id":OWNER,
                "expires_at":T0+3_600_000,"seq":advance+1,
                "next_fire_at_ms":T0+(advance+1)*60_000
            })
        );
        assert_eq!(live(&f), snapshot);
    }
}

#[test]
fn next_fire_at_without_wait_grant_emits_no_rescheduled_event() {
    let f = setup("next-fire-no-wait");
    register(&f, &recurring("1m", 1), "return 1;");
    let first = watch(&f, 1, "1h");
    // A newer package's grant does not authorize notices for the approved one.
    register(&f, &waitable("1m", 2), "return 2;");
    for advance in 1..=2 {
        f.clock.set(T0 + advance * 60_000);
        f.module.rt.scheduler().unwrap().tick().unwrap();
        assert_eq!(live(&f)["next_fire_at_ms"], T0 + (advance + 1) * 60_000);
        let events = f.module.rt.pending_template_events(32).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_name, "flow_instance_created");
        assert_eq!(events[0].flow_id, id(&first));
        assert_eq!(live(&f)["seq"], 1);
    }
}

#[test]
fn next_fire_at_waitable_cron_reschedules_after_catch_up() {
    let f = setup("next-fire-rescheduled-cron");
    let mut m = waitable("1m", 1);
    m["trigger"] = json!({"schedule":{"cron":"*/5 * * * *","tz":"UTC"}});
    register(&f, &m, "return 1;");
    let first = watch(&f, 1, "1h");
    f.clock.advance(960_000);
    f.module.rt.scheduler().unwrap().tick().unwrap();
    assert_eq!(
        body(&f, id(&first), 2, "flow_instance_rescheduled")["next_fire_at_ms"],
        T0 + 1_200_000
    );
    assert_eq!(f.module.rt.pending_template_events(32).unwrap().len(), 2);
}

#[test]
fn next_fire_at_unchanged_outbox_snapshot_is_not_rescheduled_again() {
    let f = setup("next-fire-rescheduled-unchanged");
    register(&f, &waitable("1m", 1), "return 1;");
    let first = watch(&f, 1, "1h");
    f.clock.advance(60_000);
    let scheduler = f.module.rt.scheduler().unwrap();
    scheduler.tick().unwrap();
    assert_eq!(
        body(&f, id(&first), 2, "flow_instance_rescheduled")["seq"],
        2
    );
    // Replaying an old due time must not reannounce an already queued snapshot.
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE schedules SET next_due_ms=?2 WHERE flow_id=?1",
                rusqlite::params![id(&first), T0 + 60_000],
            )?;
            Ok(())
        })
        .unwrap();
    scheduler.tick().unwrap();
    assert_eq!(f.module.rt.pending_template_events(32).unwrap().len(), 2);
    assert_eq!(live(&f)["seq"], 2);
}

#[test]
fn next_fire_at_interval_upgrade_disable_enable_and_reopen() {
    let f = setup("next-fire-interval");
    register(&f, &recurring("1m", 1), "return 1;");
    let first = watch(&f, 1, "1h");
    assert_eq!(first["next_fire_at_ms"], T0 + 60_000);
    assert_eq!(
        body(&f, id(&first), 1, "flow_instance_created")["next_fire_at_ms"],
        T0 + 60_000
    );

    register(&f, &recurring("2m", 2), "return 2;");
    let upgraded = watch(&f, 2, "1h");
    assert_eq!(upgraded["next_fire_at_ms"], T0 + 120_000);
    assert_eq!(
        body(&f, id(&first), 2, "flow_instance_extended")["next_fire_at_ms"],
        upgraded["next_fire_at_ms"]
    );
    f.module
        .handle(&agent(OWNER), "flow.disable", json!({"flow_id":id(&first)}))
        .unwrap();
    assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
    let disabled = watch(&f, 2, "2h");
    assert_eq!(disabled["next_fire_at_ms"], Value::Null);
    assert_eq!(
        body(&f, id(&first), 3, "flow_instance_extended")["next_fire_at_ms"],
        Value::Null
    );
    f.clock.advance(180_000);
    f.module
        .handle(&agent(OWNER), "flow.enable", json!({"flow_id":id(&first)}))
        .unwrap();
    assert_eq!(live(&f)["next_fire_at_ms"], T0 + 240_000);
    assert_eq!(watch(&f, 2, "2h")["next_fire_at_ms"], T0 + 240_000);
    f.module
        .handle(
            &agent(OWNER),
            "flow.instance.end",
            json!({"flow_id":id(&first)}),
        )
        .unwrap();
    assert_eq!(
        body(&f, id(&first), 5, "flow_instance_ended")["next_fire_at_ms"],
        Value::Null
    );
    let reopened = watch(&f, 2, "2h");
    assert_eq!(reopened["next_fire_at_ms"], T0 + 240_000);
    assert_eq!(
        body(&f, id(&first), 6, "flow_instance_created")["next_fire_at_ms"],
        reopened["next_fire_at_ms"]
    );
}

#[test]
fn next_fire_at_expiry_is_null_before_tick_and_on_expire_then_renew() {
    let f = setup("next-fire-expiry");
    register(&f, &recurring("2m", 1), "return 1;");
    let first = watch(&f, 1, "1m");
    f.clock.advance(60_000);
    assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
    let renewed = watch(&f, 1, "1h");
    assert_eq!(
        body(&f, id(&first), 2, "flow_instance_expired")["next_fire_at_ms"],
        Value::Null
    );
    assert_eq!(renewed["next_fire_at_ms"], T0 + 120_000);
    assert_eq!(
        body(&f, id(&first), 3, "flow_instance_extended")["next_fire_at_ms"],
        renewed["next_fire_at_ms"]
    );
    f.clock.set(renewed["expires_at"].as_i64().unwrap());
    f.module.rt.scheduler().unwrap().tick().unwrap();
    assert_eq!(
        body(&f, id(&first), 4, "flow_instance_expired")["next_fire_at_ms"],
        Value::Null
    );
    assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
}

#[test]
fn next_fire_at_cron_and_unscheduled_templates_use_schedule_rows() {
    let f = setup("next-fire-cron");
    let mut m = recurring("1m", 1);
    m["trigger"] = json!({"schedule":{"cron":"*/5 * * * *","tz":"UTC"}});
    register(&f, &m, "return 1;");
    let first = watch(&f, 1, "1h");
    assert_eq!(first["next_fire_at_ms"], T0 + 300_000);
    assert_eq!(
        body(&f, id(&first), 1, "flow_instance_created")["next_fire_at_ms"],
        T0 + 300_000
    );
    m["version"] = json!(2);
    m["trigger"] = events_manifest("watch")["trigger"].clone();
    register(&f, &m, "return 2;");
    let unscheduled = watch(&f, 2, "1h");
    assert_eq!(unscheduled["next_fire_at_ms"], Value::Null);
    assert_eq!(
        body(&f, id(&first), 2, "flow_instance_extended")["next_fire_at_ms"],
        Value::Null
    );
    assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
}

#[test]
fn next_fire_at_one_shot_consumed_or_dropped_and_revoked_is_null() {
    for revoked in [false, true] {
        let f = setup(if revoked {
            "next-fire-dropped"
        } else {
            "next-fire-consumed"
        });
        let first = remind(&f);
        if revoked {
            f.module
                .rt
                .store()
                .write(|tx| {
                    basal_core::install::revoke(tx, id(&first), 4, "revoked", T0)?;
                    Ok(())
                })
                .unwrap();
            // Package revocation retains an active schedule and its stale due
            // time, but it prevents that version from admitting another run.
            assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
            let renewed = create(&f, "remind-at", 4, first["params"].clone(), "2d");
            assert_eq!(renewed["next_fire_at_ms"], Value::Null);
            assert_eq!(
                body(&f, id(&first), 2, "flow_instance_extended")["next_fire_at_ms"],
                Value::Null
            );
        }
        f.clock.advance(3_600_000);
        let scheduler = f.module.rt.scheduler().unwrap();
        let tick = scheduler.tick().unwrap();
        assert_eq!(tick.new_runs().len(), usize::from(!revoked));
        assert_eq!(scheduler.dropped().unwrap().len(), usize::from(revoked));
        assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
        assert!(
            f.module
                .rt
                .pending_template_events(32)
                .unwrap()
                .iter()
                .all(|event| event.event_name != "flow_instance_rescheduled")
        );
        assert_eq!(
            scheduler.schedule(id(&first)).unwrap().unwrap().next_due,
            None
        );
        f.module.rt.end_instance(id(&first)).unwrap();
        assert_eq!(
            body(
                &f,
                id(&first),
                if revoked { 3 } else { 2 },
                "flow_instance_ended"
            )["next_fire_at_ms"],
            Value::Null
        );
        // The created event is an immutable historical snapshot, not a live query.
        assert_eq!(
            body(&f, id(&first), 1, "flow_instance_created")["next_fire_at_ms"],
            T0 + 3_600_000
        );
    }
}

#[test]
fn next_fire_at_quarantined_schedule_does_not_expose_retained_due_time() {
    let f = setup("next-fire-quarantine");
    register(&f, &recurring("1m", 1), "return 1;");
    let first = watch(&f, 1, "1h");
    f.module
        .rt
        .store()
        .write(|tx| {
            tx.execute(
                "UPDATE schedules SET spec='invalid' WHERE flow_id=?1",
                [id(&first)],
            )?;
            Ok(())
        })
        .unwrap();
    f.clock.advance(60_000);
    assert!(
        f.module
            .rt
            .scheduler()
            .unwrap()
            .tick()
            .unwrap()
            .new_runs()
            .is_empty()
    );
    let (state, due): (String, i64) = f
        .module
        .rt
        .store()
        .read(|c| {
            Ok(c.query_row(
                "SELECT state,next_due_ms FROM schedules WHERE flow_id=?1",
                [id(&first)],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .unwrap();
    assert_eq!(state, "disabled");
    assert_eq!(due, T0 + 60_000);
    assert_eq!(live(&f)["next_fire_at_ms"], Value::Null);
    let renewed = watch(&f, 1, "2h");
    assert_eq!(renewed["next_fire_at_ms"], Value::Null);
    assert_eq!(
        body(&f, id(&first), 2, "flow_instance_extended")["next_fire_at_ms"],
        Value::Null
    );
}
