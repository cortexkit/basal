//! The install gate: before every activation the runtime asks core whether
//! it still stands behind the run's flow version, and activates only on
//! `active` with the code hash of the code the run is about to execute.

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use basal_core::InstallRequest;
use basal_core::admission::Admission;
use basal_core::ids::hex;
use basal_core::{
    ActivationEnd, Clock, Config, InstallGate, NoHooks, RevokeCause, Run, RunState, Runtime,
};
use basal_host::InstallStatus;
use basal_proto::JsonText;
use basal_testkit::harness::{World, test_manifest};
use common::{admit, config};

/// The manual clock's start in the backoff test: 2026-05-01T00:00:00Z.
const T0: i64 = 1_777_593_600_000;

/// A runtime that asks core before every activation, on `clock`.
fn gated(world: &World, clock: &Clock) -> Runtime {
    world
        .runtime(
            Arc::new(NoHooks),
            Config {
                install_gate: InstallGate::Core,
                clock: clock.clone(),
                ..config()
            },
        )
        .expect("open runtime")
}

fn run(rt: &Runtime, run_id: &str) -> Run {
    rt.run(run_id).expect("run")
}

/// Core's `active` answer carrying the run's own code hash.
fn active(run: &Run) -> Option<InstallStatus> {
    Some(InstallStatus::Active {
        code_hash: hex(&run.code_hash),
    })
}

fn activations_sent(world: &World) -> usize {
    world.source.counts.activate.load(Ordering::SeqCst)
}

/// Asserts what the gate leaves behind when core does not stand behind the
/// run's version (revoked, unknown, or another code hash): the run cancelled
/// before any activation, the version recorded as revoked, and the flow left
/// with no approved version and disabled by core, with a `flow.disabled`
/// notification for its owner naming the reason.
fn assert_refused(rt: &Runtime, world: &World, run_id: &str, end: &ActivationEnd) -> String {
    let ActivationEnd::Revoked {
        version, detail, ..
    } = end
    else {
        panic!("expected a refusal: {end:?}");
    };
    assert_eq!(*version, 1);
    let r = run(rt, run_id);
    assert_eq!(r.state, RunState::Cancelled, "{r:#?}");
    assert_eq!(r.error_detail.as_deref(), Some(detail.as_str()));
    assert_eq!(activations_sent(world), 0, "nothing reached a worker");
    assert_eq!(rt.activation_count(run_id).expect("count"), 0);
    let reason = rt
        .store()
        .read(|c| basal_core::install::revocation(c, &r.flow_id, 1))
        .expect("read")
        .expect("the version is recorded as revoked");
    let flow = rt.flow(&r.flow_id).expect("flow").expect("record");
    assert_eq!(flow.approved_version, None, "{flow:#?}");
    assert!(!flow.enabled, "{flow:#?}");
    assert_eq!(flow.disabled_by.as_deref(), Some("core"), "{flow:#?}");
    let told = rt.outbox().expect("outbox");
    assert!(
        told.iter().any(|n| n.kind == "flow.disabled"
            && n.body.contains("\"by\":\"core\"")
            && n.body.contains(&reason)),
        "{told:#?}"
    );
    // No new trigger is admitted under the revoked version.
    let again = rt
        .admit_trigger(&r.flow_id, "after", JsonText::null())
        .expect("admit");
    assert!(
        matches!(again, Admission::Disabled | Admission::NotApproved),
        "{again:?}"
    );
    reason
}

/// Core stands behind exactly this code: the run activates, after one
/// question about its flow and version.
#[test]
fn active_with_the_runs_code_hash_activates() {
    let world = World::new("gate-active");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    world.mock.set_install_status(&r.flow_id, 1, active(&r));
    let end = rt.resume(&run_id).expect("activation");
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    assert_eq!(world.mock.install_queries(), vec![(r.flow_id.clone(), 1)]);
    assert_eq!(activations_sent(&world), 1);
}

/// Core approved other code under this version: basal and core disagree
/// about what was approved, so the run is treated as revoked, with both
/// hashes in the reason.
#[test]
fn active_with_another_code_hash_is_revoked() {
    let world = World::new("gate-mismatch");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    let other = "ab".repeat(32);
    world.mock.set_install_status(
        &r.flow_id,
        1,
        Some(InstallStatus::Active {
            code_hash: other.clone(),
        }),
    );
    let end = rt.resume(&run_id).expect("activation");
    let ActivationEnd::Revoked {
        cause: RevokeCause::HashMismatch { core, run: ours },
        ..
    } = &end
    else {
        panic!("expected a hash mismatch: {end:?}");
    };
    assert_eq!(*core, other);
    assert_eq!(*ours, hex(&r.code_hash));
    let reason = assert_refused(&rt, &world, &run_id, &end);
    assert!(
        reason.contains(&other) && reason.contains(&hex(&r.code_hash)),
        "{reason}"
    );
}

/// The operator revoked the version in core.
#[test]
fn revoked_cancels_the_run_and_disables_the_flow() {
    let world = World::new("gate-revoked");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    world.mock.set_install_status(
        &r.flow_id,
        1,
        Some(InstallStatus::Revoked {
            code_hash: hex(&r.code_hash),
        }),
    );
    let end = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(
            end,
            ActivationEnd::Revoked {
                cause: RevokeCause::Revoked,
                ..
            }
        ),
        "{end:?}"
    );
    assert_refused(&rt, &world, &run_id, &end);
}

/// Core holds no install of a version basal approved: core does not stand
/// behind it.
#[test]
fn unknown_cancels_the_run_and_disables_the_flow() {
    let world = World::new("gate-unknown");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    world
        .mock
        .set_install_status(&r.flow_id, 1, Some(InstallStatus::Unknown));
    let end = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(
            end,
            ActivationEnd::Revoked {
                cause: RevokeCause::Unknown,
                ..
            }
        ),
        "{end:?}"
    );
    assert_refused(&rt, &world, &run_id, &end);
}

/// Core unreachable: nothing is activated, the run is neither failed nor
/// cancelled and its deadline is not started, and it is not offered or asked
/// about again until a backoff passes, which doubles while core stays
/// unreachable. Once core answers, the run activates.
#[test]
fn unreachable_core_defers_the_run_with_a_doubling_backoff() {
    let world = World::new("gate-unreachable");
    let clock = Clock::manual(T0);
    let rt = gated(&world, &clock);
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    world.mock.set_install_status(&r.flow_id, 1, None);

    let end = rt.resume(&run_id).expect("activation");
    let ActivationEnd::Deferred { retry_in, .. } = end else {
        panic!("expected a deferral: {end:?}");
    };
    assert_eq!(retry_in, Duration::from_secs(1));
    let after = run(&rt, &run_id);
    assert_eq!(after.state, RunState::Pending, "{after:#?}");
    assert_eq!(after.deadline_at, None, "waiting for core is not run time");
    assert_eq!(after.error_kind, None);
    assert_eq!(rt.activation_count(&run_id).expect("count"), 0);
    assert_eq!(activations_sent(&world), 0);

    // Inside the backoff: not offered, and not asked about again.
    assert!(rt.startable().expect("startable").is_empty());
    let end = rt.resume(&run_id).expect("activation");
    assert!(matches!(end, ActivationEnd::Deferred { .. }), "{end:?}");
    assert_eq!(world.mock.install_queries().len(), 1);

    clock.advance(1_000);
    assert_eq!(rt.startable().expect("startable").len(), 1);
    let end = rt.resume(&run_id).expect("activation");
    let ActivationEnd::Deferred { retry_in, .. } = end else {
        panic!("expected a deferral: {end:?}");
    };
    assert_eq!(retry_in, Duration::from_secs(2), "the backoff doubles");
    assert_eq!(world.mock.install_queries().len(), 2);

    world.mock.set_install_status(&r.flow_id, 1, active(&r));
    clock.advance(2_000);
    let end = rt.resume(&run_id).expect("activation");
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    assert_eq!(world.mock.install_queries().len(), 3);
    assert_eq!(rt.activation_count(&run_id).expect("count"), 1);
}

/// A run resumed after a suspension is gated again: core revoked the
/// version while the run waited, so the resume cancels it. The call it had
/// dispatched keeps its obligation: its outcome is still recorded.
#[test]
fn a_resume_after_suspension_is_gated() {
    let world = World::new("gate-resume");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(
        &rt,
        &world,
        "const a = await llm({ prompt: 'p' }); return a;",
    );
    let r = run(&rt, &run_id);
    world.mock.set_install_status(&r.flow_id, 1, active(&r));
    let first = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(first, ActivationEnd::Suspended { .. }),
        "{first:?}"
    );
    world.mock.set_install_status(
        &r.flow_id,
        1,
        Some(InstallStatus::Revoked {
            code_hash: hex(&r.code_hash),
        }),
    );
    world.mock.complete_all();
    rt.wait_for(&run_id, Duration::from_secs(5), |s| s == RunState::Pending)
        .expect("wait");
    let end = rt.resume(&run_id).expect("activation");
    assert!(
        matches!(
            end,
            ActivationEnd::Revoked {
                cause: RevokeCause::Revoked,
                ..
            }
        ),
        "{end:?}"
    );
    assert_eq!(
        world.mock.install_queries().len(),
        2,
        "each activation asks"
    );
    assert_eq!(run(&rt, &run_id).state, RunState::Cancelled);
    assert_eq!(activations_sent(&world), 1, "only the first activation ran");
    rt.quiesce();
    let calls = rt.calls(&run_id).expect("calls");
    assert_eq!(calls.len(), 1);
}

/// Once basal holds a version as revoked, every other run of it is
/// cancelled before activating without asking core again.
#[test]
fn other_runs_of_a_revoked_version_are_cancelled_without_asking() {
    let world = World::new("gate-already");
    let rt = gated(&world, &Clock::system());
    let first = admit(&rt, &world, "return 7;");
    let r = run(&rt, &first);
    let mut spec = world.spec(&rt, "return 7;").expect("spec");
    spec.trigger_id = "trigger-2".into();
    let second = rt
        .admit(&spec)
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();
    world
        .mock
        .set_install_status(&r.flow_id, 1, Some(InstallStatus::Unknown));
    let end = rt.resume(&first).expect("activation");
    assert!(matches!(end, ActivationEnd::Revoked { .. }), "{end:?}");
    let end = rt.resume(&second).expect("activation");
    assert!(
        matches!(
            end,
            ActivationEnd::Revoked {
                cause: RevokeCause::AlreadyRevoked,
                ..
            }
        ),
        "{end:?}"
    );
    assert_eq!(world.mock.install_queries().len(), 1);
    assert_eq!(run(&rt, &second).state, RunState::Cancelled);
}

/// A new version approved after core revoked the previous one runs again:
/// the disable core's revoke caused is lifted by the new approval.
#[test]
fn approving_a_new_version_lifts_cores_disable() {
    let world = World::new("gate-reapprove");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    world.mock.set_install_status(
        &r.flow_id,
        1,
        Some(InstallStatus::Revoked {
            code_hash: hex(&r.code_hash),
        }),
    );
    let end = rt.resume(&run_id).expect("activation");
    assert!(matches!(end, ActivationEnd::Revoked { .. }), "{end:?}");

    let mut manifest = test_manifest();
    manifest["version"] = serde_json::json!(2);
    let manifest = manifest.to_string();
    let installed = rt
        .install(&InstallRequest {
            script: "return 8;".into(),
            manifest: manifest.clone(),
            author: "ALF".into(),
            loop_override: false,
        })
        .expect("install");
    rt.approve(&r.flow_id, 2, &installed.code_hash, "card-2")
        .expect("approve");
    let next = rt
        .admit_trigger(&r.flow_id, "trigger-2", JsonText::null())
        .expect("admit")
        .run_id()
        .expect("admitted")
        .to_owned();
    let n = run(&rt, &next);
    assert_eq!(n.flow_version, Some(2));
    let flow = rt.flow(&n.flow_id).expect("flow").expect("record");
    assert!(flow.enabled, "{flow:#?}");
    assert_eq!(flow.approved_version, Some(2));
    world.mock.set_install_status(&n.flow_id, 2, active(&n));
    let end = rt.resume(&next).expect("activation");
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
}

/// A version basal recorded as revoked by core cannot be approved again.
#[test]
fn a_revoked_version_cannot_be_approved_again() {
    let world = World::new("gate-no-reapproval");
    let rt = gated(&world, &Clock::system());
    let run_id = admit(&rt, &world, "return 7;");
    let r = run(&rt, &run_id);
    world
        .mock
        .set_install_status(&r.flow_id, 1, Some(InstallStatus::Unknown));
    rt.resume(&run_id).expect("activation");
    let again = rt.approve(&r.flow_id, 1, &r.code_hash, "again");
    assert!(
        matches!(again, Err(basal_core::InstallError::Revoked { .. })),
        "{again:?}"
    );
}

/// With the gate off (the dry run's scratch runtime), core is never asked.
#[test]
fn the_gate_off_never_asks_core() {
    let world = World::new("gate-off");
    let rt = common::runtime(&world);
    let run_id = admit(&rt, &world, "return 7;");
    let end = rt.resume(&run_id).expect("activation");
    assert!(matches!(end, ActivationEnd::Succeeded { .. }), "{end:?}");
    assert!(world.mock.install_queries().is_empty());
}
