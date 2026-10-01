//! The profile fence: no shell in the flow profile, by any route a script
//! can take. (The native bridge itself is covered by the engine's unit test
//! `raw_bridge_cannot_issue_sh_in_flow_profile`, because a script cannot
//! reach it.)

mod common;

use basal_proto::{CallKind, Primitive, Profile};
use serde_json::json;

const ROUTES: &str = r#"
    const found = [];
    // Every object reachable from the global object, searched for anything
    // called sh.
    const seen = new Set();
    const work = [globalThis];
    while (work.length) {
        const o = work.pop();
        if (o === null || (typeof o !== 'object' && typeof o !== 'function') || seen.has(o)) continue;
        seen.add(o);
        work.push(Object.getPrototypeOf(o));
        for (const key of Reflect.ownKeys(o)) {
            const d = Object.getOwnPropertyDescriptor(o, key);
            for (const v of [d.value, d.get, d.set]) {
                if (key === 'sh' || (typeof v === 'function' && v.name === 'sh')) {
                    found.push(String(key));
                }
                work.push(v);
            }
        }
    }
    const routes = {
        global: typeof globalThis.sh,
        computed: typeof globalThis['s' + 'h'],
        joined: typeof globalThis[['s', 'h'].join('')],
        has: 'sh' in globalThis,
        reflect: typeof Reflect.get(globalThis, 'sh'),
        prototype: typeof Object.getPrototypeOf(globalThis).sh,
        ops: typeof ops.sh,
        opsIndexed: typeof ops['sh'],
        kv: typeof kv.sh,
        sink: typeof sink.sh,
    };
    let bare;
    try { bare = typeof sh === 'undefined' ? 'undefined' : 'reachable'; sh('ls'); } catch (e) { bare = e.name; }
    // A module op that happens to be called sh is an op, not the shell
    // primitive; the parent's manifest check decides about it.
    const op = await ops.call('sh', 'sh', 'ls');
    return { found, routes, bare, op };
"#;

#[test]
fn sh_is_unreachable_in_the_flow_profile() {
    let mut parent = common::parent();
    parent.profile = Profile::Flow;
    let report = parent.run("t", ROUTES);
    let value = report.value();
    assert_eq!(value["found"], json!([]));
    for (route, kind) in value["routes"].as_object().expect("routes") {
        assert!(
            kind == "undefined" || kind == false,
            "sh reachable through {route}: {kind}"
        );
    }
    assert_eq!(value["bare"], "ReferenceError");
    assert!(
        report
            .host_calls
            .iter()
            .all(|c| c.kind != CallKind::Primitive(Primitive::Sh)),
        "{:?}",
        report.host_calls
    );

    // The control: the same call in the codemode profile reaches the host.
    let mut codemode = common::parent();
    codemode.profile = Profile::Codemode;
    let report = codemode.run("c", "return await sh('ls');");
    assert_eq!(report.value(), json!({"stdout": "mock shell", "exit": 0}));
    assert_eq!(
        report
            .host_calls
            .iter()
            .map(|c| c.kind.clone())
            .collect::<Vec<_>>(),
        vec![CallKind::Primitive(Primitive::Sh)]
    );
}
