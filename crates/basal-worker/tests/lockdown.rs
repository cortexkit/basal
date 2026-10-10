//! The lockdown: what a script can and cannot reach. A flow is an
//! operator-approved script that basal runs unattended; the lockdown keeps
//! it to the globals listed below and the host calls basal journals.

// Windows: gated until basal-testkit builds there.
#![cfg(not(windows))]

mod common;

use basal_proto::{CallKind, JsonText, Primitive};
use serde_json::json;

/// The complete global object after lockdown. A new global (from an engine
/// upgrade, say) fails this test until someone decides whether a flow may
/// see it.
const ALLOWED_GLOBALS: &[&str] = &[
    "AggregateError",
    "Array",
    "ArrayBuffer",
    "AsyncDisposableStack",
    "BigInt",
    "BigInt64Array",
    "BigUint64Array",
    "Boolean",
    "DataView",
    "Date",
    "DisposableStack",
    "Error",
    "EvalError",
    "Float16Array",
    "Float32Array",
    "Float64Array",
    "Infinity",
    "Int16Array",
    "Int32Array",
    "Int8Array",
    "InternalError",
    "Iterator",
    "JSON",
    "Map",
    "Math",
    "NaN",
    "Number",
    "Object",
    "Promise",
    "Proxy",
    "RangeError",
    "ReferenceError",
    "Reflect",
    "RegExp",
    "Set",
    "String",
    "SuppressedError",
    "Symbol",
    "SyntaxError",
    "TypeError",
    "URIError",
    "Uint16Array",
    "Uint32Array",
    "Uint8Array",
    "Uint8ClampedArray",
    "WeakMap",
    "WeakSet",
    "classify",
    "decodeURI",
    "decodeURIComponent",
    "encodeURI",
    "encodeURIComponent",
    "escape",
    "facts",
    "fs",
    "git",
    "globalThis",
    "isFinite",
    "isNaN",
    "kv",
    "llm",
    "net",
    "now",
    "ops",
    "parseFloat",
    "parseInt",
    "random",
    "self",
    "sink",
    "step",
    "trigger",
    "undefined",
    "unescape",
];

#[test]
fn global_inventory_is_exactly_the_allowlist() {
    let mut parent = common::parent();
    let report = parent.run("t", "return Object.getOwnPropertyNames(globalThis).sort()");
    let mut allowed: Vec<&str> = ALLOWED_GLOBALS.to_vec();
    allowed.sort();
    assert_eq!(report.value(), json!(allowed));
}

#[test]
fn removed_globals_are_absent() {
    let mut parent = common::parent();
    let script = r#"
        const names = ['eval', 'Function', 'WeakRef', 'FinalizationRegistry', 'Atomics',
            'SharedArrayBuffer', 'Intl', 'performance', 'setTimeout', 'setInterval',
            'clearTimeout', 'clearInterval', 'setImmediate', 'queueMicrotask', 'gc', 'os', 'std',
            'navigator', 'WebAssembly', 'console', 'print', 'structuredClone',
            // Names that would give a script the host-call bridge's
            // internals; none may be reachable.
            'pending', 'issueOp', 'issuePrimitive', 'issueSync', 'deliver', 'native', 'start',
            'status', '__enqueue', '__sync', '__rawBridge', 'call', 'settle', 'lockdown'];
        return names.filter((n) => n in globalThis || typeof globalThis[n] !== 'undefined');
    "#;
    assert_eq!(parent.run("t", script).value(), json!([]));
}

#[test]
fn code_from_strings_is_unreachable() {
    let mut parent = common::parent();
    let script = r#"
        const routes = {
            eval: typeof eval,
            Function: typeof Function,
            ordinary: typeof (function () {}).constructor,
            arrow: typeof (() => {}).constructor,
            klass: typeof (class {}).constructor,
            native: typeof Math.max.constructor,
            bound: typeof (function () {}).bind(null).constructor,
            async: typeof (async function () {}).constructor,
            asyncArrow: typeof (async () => {}).constructor,
            generator: typeof (function* () {}).constructor,
            asyncGenerator: typeof (async function* () {}).constructor,
            viaProto: typeof Object.getPrototypeOf(function* () {}).constructor,
            viaDescriptor: typeof Object.getOwnPropertyDescriptor(
                Object.getPrototypeOf(async function () {}), 'constructor').value,
            viaGlobalObject: typeof globalThis.constructor.constructor,
        };
        let constructed = 'refused';
        try {
            const C = Object.getPrototypeOf(function* () {}).constructor;
            new C('yield 1');
            constructed = 'compiled';
        } catch (e) {}
        const imported = await import('data:text/javascript,export default 1').then(
            () => 'loaded', () => 'refused');
        return { routes, constructed, imported };
    "#;
    let value = parent.run("t", script).value();
    for (route, kind) in value["routes"].as_object().expect("routes object") {
        assert_eq!(
            kind, "undefined",
            "code-from-string route {route} is reachable"
        );
    }
    assert_eq!(value["constructed"], "refused");
    assert_eq!(value["imported"], "refused");
}

#[test]
fn date_constructor_cannot_be_recovered() {
    let mut parent = common::parent();
    let start = parent.host.clock_ms;
    let script = r#"
        const d = new Date(0);
        const viaInstance = Object.getPrototypeOf(d).constructor;
        return {
            instance: d.constructor === Date,
            proto: Date.prototype.constructor === Date,
            viaInstance: viaInstance === Date,
            descriptor: Object.getOwnPropertyDescriptor(Date.prototype, 'constructor').value === Date,
            name: Date.name,
            length: Date.length,
            // Constructing through the constructor reached from an instance
            // (Date.prototype.constructor, via Object.getPrototypeOf) still
            // reads the host clock, not the machine's.
            recovered: new viaInstance().getTime(),
        };
    "#;
    let report = parent.run("t", script);
    let value = report.value();
    for key in ["instance", "proto", "viaInstance", "descriptor"] {
        assert_eq!(
            value[key], true,
            "{key}: the native Date constructor is reachable"
        );
    }
    assert_eq!(value["name"], "Date");
    assert_eq!(value["length"], 7);
    assert_eq!(value["recovered"], json!(start));
    assert_eq!(
        report
            .host_calls
            .iter()
            .map(|c| c.kind.clone())
            .collect::<Vec<_>>(),
        vec![CallKind::Primitive(Primitive::Now)]
    );
}

#[test]
fn local_time_and_locale_are_removed() {
    let mut parent = common::parent();
    let script = r#"
        const local = ['getDate', 'getDay', 'getFullYear', 'getHours', 'getMilliseconds',
            'getMinutes', 'getMonth', 'getSeconds', 'getYear', 'getTimezoneOffset', 'setDate',
            'setFullYear', 'setHours', 'setMilliseconds', 'setMinutes', 'setMonth', 'setSeconds',
            'setYear', 'toDateString', 'toTimeString', 'toString', 'toLocaleString',
            'toLocaleDateString', 'toLocaleTimeString'];
        const d = new Date(0);
        const present = local.filter((k) => typeof d[k] !== 'undefined');
        const locale = [
            typeof (1).toLocaleString, typeof 'a'.localeCompare, typeof 'a'.toLocaleUpperCase,
            typeof 'a'.toLocaleLowerCase, typeof [].toLocaleString, typeof (1n).toLocaleString,
            typeof ({}).toLocaleString, typeof new Uint8Array(1).toLocaleString, typeof Intl,
        ];
        let localString = 'refused';
        try { new Date('2020-01-02T03:04:05'); localString = 'accepted'; } catch (e) {
            if (!(e instanceof RangeError)) { localString = String(e); }
        }
        return {
            present,
            locale,
            utcComponents: new Date(2020, 0, 2, 3, 4, 5).toISOString(),
            utcCall: Date.UTC(2020, 0, 2),
            offsetString: new Date('2020-01-02T03:04:05+02:00').toISOString(),
            dateOnly: Date.parse('2020-01-02'),
            localParse: Date.parse('2020-01-02T03:04:05'),
            localString,
            stringified: `${new Date(0)}`,
            json: JSON.stringify(new Date(0)),
        };
    "#;
    let value = parent.run("t", script).value();
    assert_eq!(value["present"], json!([]), "local-time methods remain");
    assert!(
        value["locale"]
            .as_array()
            .expect("array")
            .iter()
            .all(|t| t == "undefined"),
        "locale methods remain: {}",
        value["locale"]
    );
    assert_eq!(value["utcComponents"], "2020-01-02T03:04:05.000Z");
    assert_eq!(value["utcCall"], json!(1_577_923_200_000u64));
    assert_eq!(value["offsetString"], "2020-01-02T01:04:05.000Z");
    assert_eq!(value["dateOnly"], json!(1_577_923_200_000u64));
    assert!(value["localParse"].is_null(), "NaN serialises as null");
    assert_eq!(value["localString"], "refused");
    assert_eq!(value["stringified"], "0");
    assert_eq!(value["json"], "\"1970-01-01T00:00:00.000Z\"");
}

#[test]
fn payloads_arrive_as_plain_data() {
    let mut parent = common::parent();
    let payload = "{\"__proto__\":{\"polluted\":true},\"text\":\"a\u{2028}b\u{2029}c\"}";
    parent.trigger = JsonText::new(payload).expect("small");
    let script = r#"
        const raw = '{"__proto__":{"polluted":true},"text":"a\u2028b\u2029c"}';
        const fromHost = await ops.call('mock', 'raw', { raw });
        const check = (v) => ({
            plainProto: Object.getPrototypeOf(v) === Object.prototype,
            notPolluted: v.polluted === undefined && ({}).polluted === undefined,
            ownKeys: Object.keys(v),
            ownProto: JSON.stringify(Object.getOwnPropertyDescriptor(v, '__proto__').value),
            text: v.text === 'a\u2028b\u2029c',
        });
        return { host: check(fromHost), trigger: check(trigger) };
    "#;
    let expected = json!({
        "plainProto": true,
        "notPolluted": true,
        "ownKeys": ["__proto__", "text"],
        "ownProto": "{\"polluted\":true}",
        "text": true,
    });
    let first = parent.run("t", script);
    assert_eq!(
        first.value(),
        json!({"host": expected, "trigger": expected})
    );
    // Replayed from the journal, the value is parsed the same way and the
    // result is identical, with no call to the host.
    let replay = parent.run("t", script);
    assert!(replay.host_calls.is_empty());
    assert_eq!(replay.value(), first.value());
}

#[test]
fn intrinsics_are_frozen() {
    let mut parent = common::parent();
    let script = r#"
        const frozen = [Object.prototype, Array.prototype, Object.getPrototypeOf(function () {}),
            Promise.prototype,
            Promise, JSON, Math, Reflect, globalThis, ops, kv, sink, trigger, Date, Date.prototype,
            Object.getPrototypeOf(async function () {}),
            Object.getPrototypeOf([][Symbol.iterator]()),
            Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]())),
            Object.getPrototypeOf(Int8Array.prototype)].every((o) => Object.isFrozen(o));
        const attempts = [
            () => { Array.prototype.push = null; },
            () => { Promise.prototype.then = null; },
            () => { JSON.parse = null; },
            () => { Object.prototype.polluted = 1; },
            () => { globalThis.injected = 1; },
            () => { ops.call = null; },
            () => { Object.defineProperty(Map.prototype, 'get', { value: null }); },
        ].map((f) => { try { f(); return 'allowed'; } catch (e) { return e.name; } });
        // Ordinary code that assigns an inherited name still works.
        class Custom extends Error {
            constructor() { super('m'); this.name = 'Custom'; }
        }
        const o = {};
        o.toString = () => 'own';
        return { frozen, attempts, errorName: new Custom().name, own: String(o) };
    "#;
    let value = parent.run("t", script).value();
    assert_eq!(value["frozen"], true);
    assert_eq!(value["attempts"], json!(vec!["TypeError"; 7]));
    assert_eq!(value["errorName"], "Custom");
    assert_eq!(value["own"], "own");
}
