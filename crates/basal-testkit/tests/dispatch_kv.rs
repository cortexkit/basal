//! A flow's `kv`: eager writes that outlive a failed run, shared by the
//! flow's versions, size caps refused as journaled rejections, and each
//! write applied exactly once across every cut.

mod common;

use std::sync::Arc;

use basal_core::{Config, KvLimits, NoHooks, RunState};
use basal_proto::Settlement;
use basal_testkit::harness::{World, run_with_cuts};
use common::{admit_trigger, config, finish, result};
use serde_json::json;

#[test]
fn kv_writes_survive_a_later_failure_of_their_run() {
    let world = World::new("kv-eager");
    let rt = common::runtime(&world);
    let failed = admit_trigger(
        &rt,
        &world,
        r#"
        await kv.set('posted', 'x');
        await kv.set('n', 1);
        throw new Error('a later step fails');
        "#,
        "t1",
    );
    let run = finish(&rt, &world, &failed);
    assert_eq!(run.state, RunState::Failed, "{run:#?}");
    // The next run is a new version of the flow: kv belongs to the flow id.
    let next = admit_trigger(
        &rt,
        &world,
        "return [await kv.get('posted'), await kv.get('n'), await kv.get('never')];",
        "t2",
    );
    let run = finish(&rt, &world, &next);
    assert_eq!(run.flow_version, Some(2));
    assert_eq!(result(&run), json!(["x", 1, null]), "{run:#?}");
}

#[test]
fn kv_caps_refuse_with_journaled_rejections() {
    let world = World::new("kv-caps");
    let rt = world
        .runtime(
            Arc::new(NoHooks),
            Config {
                kv: KvLimits {
                    max_key_bytes: 8,
                    max_value_bytes: 16,
                    max_flow_bytes: 40,
                },
                ..config()
            },
        )
        .expect("runtime");
    // Values are stored as JSON text: 'x'.repeat(10) is 12 bytes, and an
    // entry counts its key's bytes too.
    let run_id = admit_trigger(
        &rt,
        &world,
        r#"
        const out = {};
        const attempt = async (name, f) => {
            try { await f(); out[name] = 'ok'; } catch (e) { out[name] = e.data.code; }
        };
        await attempt('long_key', () => kv.set('k'.repeat(9), 1));
        await attempt('big_value', () => kv.set('a', 'x'.repeat(20)));
        await attempt('first', () => kv.set('a', 'x'.repeat(10)));
        await attempt('second', () => kv.set('b', 'y'.repeat(10)));
        await attempt('over_total', () => kv.set('c', 'z'.repeat(14)));
        await attempt('replace', () => kv.set('a', 'x'.repeat(14)));
        out.c = await kv.get('c');
        return out;
        "#,
        "t1",
    );
    let run = finish(&rt, &world, &run_id);
    assert_eq!(
        result(&run),
        json!({
            "long_key": "kv_limit",
            "big_value": "kv_limit",
            "first": "ok",
            "second": "ok",
            "over_total": "kv_limit",
            "replace": "ok",
            "c": null,
        }),
        "{run:#?}"
    );
    let calls = rt.calls(&run_id).expect("calls");
    let rejected: Vec<u64> = calls
        .iter()
        .filter(|c| {
            c.outcome
                .as_ref()
                .is_some_and(|o| o.settlement == Settlement::Rejected)
        })
        .map(|c| c.position)
        .collect();
    assert_eq!(rejected, vec![0, 1, 4]);
    let audit = rt
        .store()
        .read(|c| basal_core::audit::rows(c, &run_id))
        .expect("audit");
    let limited: Vec<u64> = audit
        .iter()
        .filter(|r| r.outcome == "kv_limit")
        .map(|r| r.position)
        .collect();
    assert_eq!(limited, vec![0, 1, 4]);
}

#[test]
fn kv_write_is_applied_once_across_every_cut() {
    let script = r#"
        await kv.set('n', 1);
        const l = await ops.call('mock', 'long', {});
        await kv.set('m', 2);
        await kv.delete('gone');
        return [await kv.get('n'), await kv.get('m'), l.done];
    "#;
    let world = World::new("kv-once-uncut");
    let base = run_with_cuts(&world, script, &[], &config()).expect("uncut");
    assert_eq!(base.summary.result, Some(json!([1, 2, true])));
    let expected = json!({"m": "2@1", "n": "1@1"});
    assert_eq!(json!(base.summary.kv), expected);
    let points = base.phases[0].clone();
    assert!(points.len() > 10, "{points:?}");
    let mut failures = Vec::new();
    for point in points {
        let world = World::new("kv-once");
        match run_with_cuts(&world, script, std::slice::from_ref(&point), &config()) {
            Ok(run) => {
                if json!(run.summary.kv) != expected
                    || run.summary.result != base.summary.result
                    || !run.summary.audit_complete()
                {
                    failures.push(format!("{}: {:#?}", point.render(), run.summary));
                }
            }
            Err(e) => failures.push(format!("{}: {e}", point.render())),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
