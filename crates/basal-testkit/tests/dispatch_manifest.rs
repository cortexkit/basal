//! The manifest and install validation: decoding with unknown fields
//! refused, exactly one trigger kind, the catalog checks (events and their
//! versions, ops and their markers, shell-capable ops, agents), the loop
//! install rule and its operator override, and approval bound to the code
//! hash with a new version applying to newly admitted triggers only.

mod common;

use basal_core::{
    Admission, InstallError, InstallRequest, Manifest, ManifestError, RunState, Runtime,
    ShellDenylist, TriggerSpec, Warning,
};
use basal_host::{OpDecl, OpKind};
use basal_proto::JsonText;
use basal_testkit::harness::{World, test_manifest};
use common::{finish, result, runtime};
use serde_json::{Value, json};

/// The example in `docs/manifest.md`, read from the document itself.
fn documented_example() -> String {
    let doc = include_str!("../../../docs/manifest.md");
    let after = doc
        .split("<!-- manifest-example -->")
        .nth(1)
        .expect("the example marker");
    let body = after.split("```json").nth(1).expect("the example block");
    body.split("```")
        .next()
        .expect("the block's end")
        .to_owned()
}

fn install(
    rt: &Runtime,
    manifest: &Value,
    loop_override: bool,
) -> Result<basal_core::Installed, InstallError> {
    rt.install(&InstallRequest {
        script: "return 1;".into(),
        manifest: manifest.to_string(),
        author: "ALF".into(),
        loop_override,
    })
}

fn manifest_with(change: impl FnOnce(&mut Value)) -> Value {
    let mut m = test_manifest();
    change(&mut m);
    m
}

#[test]
fn documented_example_round_trips_and_installs() {
    let text = documented_example();
    let parsed = Manifest::parse(&text).expect("the documented example parses");
    assert_eq!(parsed.id, "synapse-xcom-inference-news");
    assert_eq!(parsed.version, 3);
    let again = serde_json::to_string(&parsed).expect("serialise");
    assert_eq!(Manifest::parse(&again).expect("reparse"), parsed);
    assert_eq!(parsed.deadline_ms(1), 600_000);
    assert_eq!(parsed.token_window_ms(), Some(86_400_000));

    let world = World::new("manifest-example");
    world.catalog.set_op(
        "cerebellum",
        "browser.read_page",
        OpDecl {
            kind: Some(OpKind::Query),
            cause_echo: false,
            shell_capable: false,
        },
    );
    let rt = runtime(&world);
    let installed = rt
        .install(&InstallRequest {
            script: "return 1;".into(),
            manifest: text.clone(),
            author: "SYNAPSE".into(),
            loop_override: false,
        })
        .expect("the documented example installs");
    assert!(installed.new);
    assert_eq!(
        installed.code_hash,
        basal_core::ids::code_hash("return 1;", &text)
    );
}

#[test]
fn manifest_refuses_unknown_fields() {
    let top = manifest_with(|m| m["permissions"] = json!(["everything"]));
    let nested = manifest_with(|m| m["sinks"][0]["digest_maximum"] = json!("wake"));
    let in_trigger = manifest_with(|m| m["trigger"]["webhook"] = json!({}));
    let in_llm = manifest_with(|m| m["llm"]["tools"] = json!(true));
    for m in [top, nested, in_trigger, in_llm] {
        match Manifest::parse(&m.to_string()) {
            Err(ManifestError::Decode(e)) => assert!(e.contains("unknown field"), "{e}"),
            other => panic!("accepted an unknown field: {other:?}"),
        }
    }
    let duplicate = r#"{"id":"a","id":"b","version":1,"purpose":"p","trigger":{"schedule":{}}}"#;
    assert!(matches!(
        Manifest::parse(duplicate),
        Err(ManifestError::Decode(_))
    ));
}

#[test]
fn trigger_needs_exactly_one_kind() {
    let both = manifest_with(|m| {
        m["trigger"]["schedule"] = json!({ "interval": "15m" });
    });
    let neither = manifest_with(|m| m["trigger"] = json!({}));
    assert_eq!(
        Manifest::parse(&both.to_string()),
        Err(ManifestError::TriggerKinds {
            events: true,
            schedule: true
        })
    );
    assert_eq!(
        Manifest::parse(&neither.to_string()),
        Err(ManifestError::TriggerKinds {
            events: false,
            schedule: false
        })
    );
}

/// `trigger.schedule` is the scheduler's typed spec: its shape is decoded
/// with unknown fields refused, and its content compiled by the
/// scheduler's validation (a cron pattern, a zone, an interval range).
#[test]
fn schedule_triggers_are_typed_and_validated_by_the_scheduler() {
    let schedule = |spec: Value| manifest_with(|m| m["trigger"] = json!({ "schedule": spec }));
    let ok = Manifest::parse(
        &schedule(json!({ "cron": "*/30 * * * *", "tz": "Europe/Madrid", "missed": "each", "each_cap": 5 }))
            .to_string(),
    )
    .expect("a valid schedule");
    let spec = ok.trigger.schedule.expect("typed schedule");
    assert_eq!(spec.cron.as_deref(), Some("*/30 * * * *"));
    for shape in [
        json!("every minute"),
        json!({ "cron": "0 * * * *", "timezone": "UTC" }),
    ] {
        assert!(
            matches!(
                Manifest::parse(&schedule(shape.clone()).to_string()),
                Err(ManifestError::Decode(_))
            ),
            "{shape}"
        );
    }
    for content in [
        json!({ "cron": "not a pattern" }),
        json!({ "cron": "0 * * * *", "tz": "Nowhere/Land" }),
        json!({ "interval": "10s" }),
        json!({ "cron": "0 * * * *", "interval": "1h" }),
    ] {
        assert!(
            matches!(
                Manifest::parse(&schedule(content.clone()).to_string()),
                Err(ManifestError::Schedule(_))
            ),
            "{content}"
        );
    }
}

fn events_manifest(events: Value, ops: Value) -> Value {
    manifest_with(|m| {
        m["trigger"] = json!({ "events": events });
        m["ops"] = ops;
    })
}

#[test]
fn install_refuses_unknown_events_and_versions() {
    let world = World::new("manifest-events");
    let rt = runtime(&world);
    let unknown = events_manifest(
        json!([{"module": "plexus", "name": "push", "version": 1}]),
        json!([]),
    );
    let wrong_version = events_manifest(
        json!([{"module": "plexus", "name": "pull_request_review", "version": 2}]),
        json!([]),
    );
    for m in [unknown, wrong_version] {
        assert!(
            matches!(
                install(&rt, &m, false),
                Err(InstallError::UnknownEvent { .. })
            ),
            "{m}"
        );
    }
    let known = events_manifest(
        json!([{"module": "plexus", "name": "pull_request_review", "version": 1}]),
        json!([]),
    );
    install(&rt, &known, false).expect("a declared event and version install");
}

#[test]
fn install_refuses_unknown_and_unmarked_ops() {
    let world = World::new("manifest-ops");
    let rt = runtime(&world);
    let unknown = manifest_with(|m| m["ops"] = json!([{"module": "mock", "op": "nonexistent"}]));
    assert!(matches!(
        install(&rt, &unknown, false),
        Err(InstallError::UnknownOp { .. })
    ));
    let unmarked = manifest_with(|m| m["ops"] = json!([{"module": "mock", "op": "unmarked"}]));
    assert!(matches!(
        install(&rt, &unmarked, false),
        Err(InstallError::UnmarkedOp { .. })
    ));
}

#[test]
fn install_refuses_shell_capable_ops_listed_by_marker_or_denylist() {
    let world = World::new("manifest-shell");
    // codemode is in the catalog as an ordinary mutation; only the
    // denylist knows it can run commands.
    world.catalog.set_op(
        "basal",
        "codemode",
        OpDecl {
            kind: Some(OpKind::Mutate),
            cause_echo: false,
            shell_capable: false,
        },
    );
    // An op only the catalog marks shell-capable: the denylist does not
    // name it, so the marker alone must refuse it.
    world.catalog.set_op(
        "mock",
        "runner",
        OpDecl {
            kind: Some(OpKind::Mutate),
            cause_echo: false,
            shell_capable: true,
        },
    );
    let rt = runtime(&world);
    let marked = manifest_with(|m| m["ops"] = json!([{"module": "mock", "op": "runner"}]));
    let denylisted = manifest_with(|m| m["ops"] = json!([{"module": "basal", "op": "codemode"}]));
    let both = manifest_with(|m| m["ops"] = json!([{"module": "aft", "op": "bash"}]));
    for m in [marked, denylisted, both] {
        assert!(
            matches!(
                install(&rt, &m, true),
                Err(InstallError::ShellCapable { .. })
            ),
            "{m}"
        );
    }
    // The denylist ignores ASCII case, so a differently spelt name is
    // refused too.
    assert!(ShellDenylist::default().contains("AFT", "Bash"));
}

#[test]
fn install_refuses_unknown_agents() {
    let world = World::new("manifest-agents");
    let rt = runtime(&world);
    let cases = [
        manifest_with(|m| m["sinks"][0]["agent"] = json!("NOBODY")),
        manifest_with(|m| m["status"] = json!(["NOBODY"])),
        manifest_with(|m| m["claims"] = json!([{"agent": "NOBODY", "source_kind": "idleness"}])),
        manifest_with(|m| m["facts"]["targets"] = json!(["NOBODY"])),
    ];
    for m in cases {
        assert!(
            matches!(
                install(&rt, &m, false),
                Err(InstallError::UnknownAgent { .. })
            ),
            "{m}"
        );
    }
}

#[test]
fn loop_install_rule_needs_the_operator_override() {
    let world = World::new("manifest-loop");
    let rt = runtime(&world);
    let trigger = json!([{"module": "plexus", "name": "pull_request_review", "version": 1}]);
    // A mutation on the trigger's own module that does not echo causes.
    let looping = events_manifest(
        trigger.clone(),
        json!([{"module": "plexus", "op": "pr.comment"}]),
    );
    assert_eq!(
        install(&rt, &looping, false),
        Err(InstallError::LoopRule {
            module: "plexus".into(),
            op: "pr.comment".into()
        })
    );
    let overridden = install(&rt, &looping, true).expect("the operator override installs it");
    assert_eq!(
        overridden.warnings,
        vec![Warning::LoopOverride {
            module: "plexus".into(),
            op: "pr.comment".into()
        }]
    );
    // A mutation that echoes causes, and a query, need no override.
    let mut echoing = events_manifest(
        trigger,
        json!([{"module": "plexus", "op": "pr.label"}, {"module": "plexus", "op": "pr.get"}]),
    );
    echoing["version"] = json!(2);
    install(&rt, &echoing, false).expect("cause echo satisfies the rule");
}

/// Approval binds to the code hash; a run keeps the version it was
/// admitted under after another is approved; only the approved version's
/// exact bytes are admitted.
#[test]
fn new_version_applies_to_newly_admitted_triggers_only() {
    let world = World::new("manifest-versions");
    let rt = runtime(&world);
    let script = |v: u32| format!("const l = await ops.call('mock', 'long', {{}}); return {v};");
    let manifest = |v: u32| manifest_with(|m| m["version"] = json!(v)).to_string();
    let request = |v: u32| InstallRequest {
        script: script(v),
        manifest: manifest(v),
        author: "ALF".into(),
        loop_override: false,
    };
    let v1 = rt.install(&request(1)).expect("install v1");
    assert_eq!(
        rt.approve("flow-test", 1, &[0; 32], "card-1"),
        Err(InstallError::HashMismatch {
            flow_id: "flow-test".into(),
            version: 1
        })
    );
    rt.approve("flow-test", 1, &v1.code_hash, "card-1")
        .expect("approve v1");
    let trigger = JsonText::new("{}").expect("small");
    let first = rt
        .admit_trigger("flow-test", "t1", trigger.clone())
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();
    // Hold the first run mid-flight while v2 is approved.
    assert!(matches!(
        rt.resume(&first).expect("activation"),
        basal_core::ActivationEnd::Suspended { .. }
    ));
    // The same version with other code is a new version, not an edit.
    let mut edited = request(1);
    edited.script.push(' ');
    assert!(matches!(
        rt.install(&edited),
        Err(InstallError::VersionExists { .. })
    ));
    let v2 = rt.install(&request(2)).expect("install v2");
    rt.approve("flow-test", 2, &v2.code_hash, "card-2")
        .expect("approve v2");
    let old = TriggerSpec {
        flow_id: "flow-test".into(),
        trigger_id: "t-old".into(),
        trigger: trigger.clone(),
        script: script(1),
        manifest: manifest(1),
    };
    assert_eq!(rt.admit(&old).expect("admit"), Admission::NotApproved);
    let second = rt
        .admit_trigger("flow-test", "t2", trigger)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();

    let first_run = finish(&rt, &world, &first);
    let second_run = finish(&rt, &world, &second);
    assert_eq!(first_run.flow_version, Some(1));
    assert_eq!(second_run.flow_version, Some(2));
    assert_eq!(first_run.state, RunState::Succeeded, "{first_run:#?}");
    assert_eq!(result(&first_run), json!(1));
    assert_eq!(result(&second_run), json!(2));
}
