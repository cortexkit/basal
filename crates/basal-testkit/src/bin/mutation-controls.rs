//! Mutation controls: proof that each safety test can fail.
//!
//! Each control disables one mechanism with a temporary source edit, runs
//! exactly the test named for that mechanism, records whether it went red,
//! and restores the source. A test that stays green with its mechanism
//! disabled is not testing that mechanism.
//!
//! Run from anywhere in the repository, with the changes under test either
//! committed or staged:
//!
//! ```text
//! cargo run -p basal-testkit --bin mutation-controls [-- [--worker | --journal | --dispatch | --schedule | --module | --broca | --hosts] [--check] [<label filter>]]
//! ```
//!
//! Without a suite flag it runs the worker engine's controls; with
//! `--journal`, the journal, run state machine and driver controls in
//! basal-core; with `--dispatch`, the manifest, authorization, audit, token,
//! `kv`, slot, deadline, rate-limit, disable and per-run limit controls in
//! basal-core; with `--schedule`, the scheduler's controls in basal-core;
//! with `--module`, the module shell's (pool, engine, ops, dry run, consent,
//! manifest), whose tests live in basal-module; with `--hosts`, the consumer
//! adapters and their journal integration, also driven by basal-module tests.
//! `--broca` selects model host contracts, recovery, token metadata and restart controls.
//! basal-core's tests live in basal-testkit. `--check` only verifies that every edit's
//! text occurs exactly once in the current source.
//!
//! Safety of the working tree: the runner stages every file it will touch so
//! the index holds the current source, refuses to start if anything is
//! unstaged, restores each edit from the index with `git checkout --`, and
//! checks the tree is clean again before the next control. It never stashes
//! and never restores from HEAD. Every temporary edit carries the marker
//! `NON-VACUITY BREAK` so a break left behind by a crash is easy to find.
//! Evidence is written to `docs/findings/slice-1-mutations.json` (worker),
//! `docs/findings/slice-2-mutations.json` (journal),
//! `docs/findings/slice-3-mutations.json` (dispatch),
//! `docs/findings/slice-4-mutations.json` (scheduler) or
//! `docs/findings/slice-5-mutations.json` (module),
//! `docs/findings/i1a-broca-mutations.json` (model host), or
//! `docs/findings/i1a-host-mutations.json` (consumer adapters). Each evidence
//! row names the changed mechanism, expected failing test and restore checks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PRELUDE: &str = "crates/basal-worker/src/prelude.js";
const ENGINE: &str = "crates/basal-worker/src/engine.rs";
const CLOCK: &str = "crates/basal-worker/src/clock.rs";
const LINK: &str = "crates/basal-worker/src/link.rs";
const CONFINEMENT: &str = "crates/basal-worker/src/confinement.rs";
const FRAME: &str = "crates/basal-proto/src/frame.rs";
const WORKER_MANIFEST: &str = "crates/basal-worker/Cargo.toml";
const LOCKFILE: &str = "Cargo.lock";

/// Where the named test lives.
#[derive(Clone, Copy)]
enum Target {
    /// An integration test file under `crates/basal-worker/tests/`.
    Integration(&'static str),
    /// A unit test inside the worker library.
    Lib,
    /// An integration test file under `crates/basal-testkit/tests/`.
    Testkit(&'static str),
    /// An integration test file under `crates/basal-module/tests/`.
    Module(&'static str),
    /// A unit test inside the module library.
    ModuleLib,
    /// An integration test of the Broca host with a fake instead of a live service.
    Host(&'static str),
    HostLib,
}

struct Control {
    label: &'static str,
    /// (file, exact text that must occur once, replacement)
    edits: &'static [(&'static str, &'static str, &'static str)],
    /// Files cargo may rewrite while the control runs, restored with the
    /// edited ones.
    also_restore: &'static [&'static str],
    target: Target,
    test: &'static str,
}

const NO_EXTRA: &[&str] = &[];

const CONTROLS: &[Control] = &[
    Control {
        label: "lockdown: forbidden globals are not removed",
        edits: &[(
            PRELUDE,
            "for (const key of REMOVED_GLOBALS) {",
            "for (const key of []) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "removed_globals_are_absent",
    },
    Control {
        label: "lockdown: forbidden globals are not removed (inventory)",
        edits: &[(
            PRELUDE,
            "for (const key of REMOVED_GLOBALS) {",
            "for (const key of []) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "global_inventory_is_exactly_the_allowlist",
    },
    Control {
        label: "lockdown: function-kind constructors are left in place",
        edits: &[(
            PRELUDE,
            "for (const fn of [function () {}, async function () {}, function* () {}, async function* () {}]) {",
            "for (const fn of []) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "code_from_strings_is_unreachable",
    },
    Control {
        label: "lockdown: Date.prototype.constructor still names the native Date",
        edits: &[(
            PRELUDE,
            "ObjectDefineProperty(dateProto, 'constructor', { value: RunDate, writable: true, enumerable: false, configurable: true });",
            "// constructor left native",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "date_constructor_cannot_be_recovered",
    },
    Control {
        label: "lockdown: local-time methods are kept",
        edits: &[(
            PRELUDE,
            "for (const key of LOCAL_TIME) {",
            "for (const key of []) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "local_time_and_locale_are_removed",
    },
    Control {
        label: "values evaluated as source instead of parsed as data",
        edits: &[(
            PRELUDE,
            "const JSONParse = JSON.parse;",
            "const JSONParse = ((evaluate) => (text) => evaluate('(' + text + ')'))(eval);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "payloads_arrive_as_plain_data",
    },
    Control {
        label: "intrinsics are not frozen",
        edits: &[(ENGINE, "harden(&ctx, roots)?;", "drop(roots);")],
        also_restore: NO_EXTRA,
        target: Target::Integration("lockdown"),
        test: "intrinsics_are_frozen",
    },
    Control {
        label: "recorded clock and random outcomes are not served from the prefix",
        edits: &[(
            ENGINE,
            "match recorded.outcome.clone() {",
            "match recorded.outcome.clone().filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("clock"),
        test: "clock_and_random_are_journaled_and_replayed",
    },
    Control {
        label: "Math.random is not a host call",
        edits: &[(
            PRELUDE,
            "value: function random() { return callSync(RANDOM); },",
            "value: function random() { return 0.5; },",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("clock"),
        test: "clock_and_random_are_journaled_and_replayed",
    },
    Control {
        label: "one clock value reused for the whole activation",
        edits: &[(
            PRELUDE,
            "  function readClock() {\n    return callSync(NOW);\n  }",
            "  let cachedClock;\n  function readClock() {\n    if (cachedClock === undefined) { cachedClock = callSync(NOW); }\n    return cachedClock;\n  }",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("clock"),
        test: "fresh_clock_read_after_a_gap_returns_the_new_time",
    },
    Control {
        label: "recorded outcomes released in issue order, not delivery order (race/any)",
        edits: &[(
            ENGINE,
            "release.sort_by_key(|r| r.order);",
            "release.sort_by_key(|r| r.position);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "race_and_any_keep_their_winner_on_replay",
    },
    Control {
        label: "recorded outcomes released in issue order, not delivery order",
        edits: &[(
            ENGINE,
            "release.sort_by_key(|r| r.order);",
            "release.sort_by_key(|r| r.position);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "recorded_outcomes_are_released_in_delivery_order",
    },
    Control {
        label: "recorded rejections replayed as fulfilments (caught rejection)",
        edits: &[(
            ENGINE,
            "self.deliver(position, outcome.settlement, &outcome.value)?;",
            "self.deliver(position, Settlement::Fulfilled, &outcome.value)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "caught_rejection_replays_as_a_rejection",
    },
    Control {
        label: "recorded rejections replayed as fulfilments (all-rejected any)",
        edits: &[(
            ENGINE,
            "self.deliver(position, outcome.settlement, &outcome.value)?;",
            "self.deliver(position, Settlement::Fulfilled, &outcome.value)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "all_rejected_any_replays_in_order",
    },
    Control {
        label: "recorded rejections replayed as fulfilments (early all rejection)",
        edits: &[(
            ENGINE,
            "self.deliver(position, outcome.settlement, &outcome.value)?;",
            "self.deliver(position, Settlement::Fulfilled, &outcome.value)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "early_all_rejection_replays_with_calls_in_flight",
    },
    Control {
        label: "divergence check disabled",
        edits: &[(
            ENGINE,
            "if recorded.kind != kind || recorded.args_digest != digest {",
            "if false && (recorded.kind != kind || recorded.args_digest != digest) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "divergent_call_fails_with_a_typed_error",
    },
    Control {
        label: "suspension on long-running calls disabled",
        edits: &[(ENGINE, "if all_long {", "if false && all_long {")],
        also_restore: NO_EXTRA,
        target: Target::Integration("replay"),
        test: "blocked_only_on_long_calls_suspends_and_resumes",
    },
    Control {
        label: "interrupt handler never stops the engine (CPU loop)",
        edits: &[(
            ENGINE,
            "interrupt.halted.get() || interrupt.clock.over_budget()",
            "false && (interrupt.halted.get() || interrupt.clock.over_budget())",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("budgets"),
        test: "cpu_bound_loop_is_stopped_by_the_js_time_budget",
    },
    Control {
        label: "interrupt handler never stops the engine (catastrophic regex)",
        edits: &[(
            ENGINE,
            "interrupt.halted.get() || interrupt.clock.over_budget()",
            "false && (interrupt.halted.get() || interrupt.clock.over_budget())",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("fuzz"),
        test: "catastrophic_regex_is_stopped_by_the_js_time_budget",
    },
    Control {
        label: "JS time measured as wall time from the first engine entry",
        edits: &[
            (
                CLOCK,
                "libc::CLOCK_THREAD_CPUTIME_ID",
                "libc::CLOCK_MONOTONIC",
            ),
            (
                CLOCK,
                "    pub fn leave(&self) {\n",
                "    pub fn leave(&self) {\n        #[allow(unreachable_code)]\n        return;\n",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Integration("budgets"),
        test: "long_host_waits_do_not_consume_the_js_time_budget",
    },
    Control {
        label: "memory limit not applied",
        edits: &[(
            ENGINE,
            "rt.set_memory_limit(request.budgets.memory_bytes as usize);",
            "// memory limit skipped",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("budgets"),
        test: "memory_limit_holds",
    },
    Control {
        label: "stack limit not applied",
        edits: &[(
            ENGINE,
            "rt.set_max_stack_size(request.budgets.stack_bytes as usize);",
            "// stack limit skipped",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("budgets"),
        test: "stack_limit_holds",
    },
    Control {
        label: "stall detection disabled",
        edits: &[(
            ENGINE,
            "ScriptState::Pending if outstanding.is_empty() => {",
            "ScriptState::Pending if false && outstanding.is_empty() => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("budgets"),
        test: "stalled_promise_is_reported_as_stalled",
    },
    Control {
        label: "sh installed in every profile",
        edits: &[(
            PRELUDE,
            "  if (codemode) {\n    api.sh",
            "  if (true || codemode) {\n    api.sh",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("profile"),
        test: "sh_is_unreachable_in_the_flow_profile",
    },
    Control {
        label: "native bridge accepts sh in the flow profile",
        edits: &[(
            ENGINE,
            "if bridge.profile == Profile::Flow && kind.is_shell() {",
            "if false && bridge.profile == Profile::Flow && kind.is_shell() {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Lib,
        test: "engine::tests::raw_bridge_cannot_issue_sh_in_flow_profile",
    },
    Control {
        label: "frame size not checked before reading the payload",
        edits: &[(
            FRAME,
            "let len = u32::from_be_bytes(header) as usize;\n    if len > MAX_FRAME_BYTES {",
            "let len = u32::from_be_bytes(header) as usize;\n    if false && len > MAX_FRAME_BYTES {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("ipc"),
        test: "oversized_frame_is_refused_and_the_worker_exits_cleanly",
    },
    Control {
        label: "malformed frames break the channel instead of being refused",
        edits: &[(
            LINK,
            "Err(FrameError::Decode(e)) => {",
            "Err(FrameError::Decode(e)) if false => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("ipc"),
        test: "malformed_frames_are_refused_and_the_worker_survives",
    },
    Control {
        label: "deliveries for positions not awaited are accepted",
        edits: &[(
            LINK,
            "if !awaiting.contains(&outcome.position) {",
            "if false && !awaiting.contains(&outcome.position) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("ipc"),
        test: "out_of_order_frames_are_refused",
    },
    Control {
        label: "delivery order regressions are accepted",
        edits: &[(
            LINK,
            "Some(last) if received <= last =>",
            "Some(last) if false && received <= last =>",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("ipc"),
        test: "out_of_order_frames_are_refused",
    },
    Control {
        label: "worker depends on a subc crate",
        edits: &[(
            WORKER_MANIFEST,
            "rquickjs.workspace = true\n",
            "rquickjs.workspace = true\nsubc-protocol = \"0.24.1\"\n",
        )],
        also_restore: &[LOCKFILE],
        target: Target::Integration("dependency_fence"),
        test: "worker_dependency_tree_has_no_store_no_subc_and_no_core",
    },
    Control {
        label: "Seatbelt profile not applied",
        edits: &[(
            CONFINEMENT,
            "    apply_seatbelt()?;\n",
            "    // sandbox skipped\n",
        )],
        also_restore: NO_EXTRA,
        target: Target::Integration("confinement"),
        test: "sandbox_denies_files_sockets_and_exec",
    },
];

const JOURNAL: &str = "crates/basal-core/src/journal.rs";
const DRIVER: &str = "crates/basal-core/src/driver.rs";
const RUNS: &str = "crates/basal-core/src/runs.rs";
const RUNTIME: &str = "crates/basal-core/src/runtime.rs";
const ADMISSION: &str = "crates/basal-core/src/admission.rs";
const RETENTION: &str = "crates/basal-core/src/retention.rs";
const STORE: &str = "crates/basal-core/src/store.rs";

/// The journal, run state machine and driver: each control disables one
/// mechanism in basal-core and runs the basal-testkit test named for it.
const JOURNAL_CONTROLS: &[Control] = &[
    Control {
        label: "late outcome shipped in the prefix with a higher order (replay barrier removed)",
        edits: &[(
            JOURNAL,
            "let prefix: Vec<RecordedCall> = rows.iter().map(CallRow::recorded).collect();",
            "let mut prefix: Vec<RecordedCall> = rows.iter().map(CallRow::recorded).collect();\n    let mut next = prefix.iter().filter_map(|c| c.outcome.as_ref().map(|o| o.delivery_order + 1)).max().unwrap_or(0);\n    let mut stmt = conn.prepare(\"SELECT position, settlement, value FROM mailbox WHERE run_id = ?1 ORDER BY seq\")?;\n    let late = stmt.query_map([run_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;\n    for (p, s, v) in late {\n        if let Some(call) = prefix.get_mut(to_u64(p, \"position\")? as usize) {\n            call.outcome = Some(basal_proto::RecordedOutcome { settlement: parse_settlement(&s)?, value: json(v, \"late value\")?, delivery_order: next });\n            next += 1;\n        }\n    }",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "race_cut_late_outcome_waits_for_the_barrier",
    },
    Control {
        label: "suspend transition does not re-read arrivals (lost wakeup)",
        edits: &[(
            RUNS,
            "let arrived = settled_awaited\n        || in_mailbox > 0\n        || crate::model::to_u64(readiness, \"readiness\")? != seen_readiness;",
            "let arrived = false;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "completion_after_suspended_report_is_not_lost",
    },
    Control {
        label: "claim is not a compare-and-set from pending",
        edits: &[(
            RUNS,
            "awaited = NULL WHERE run_id = ?1 AND state = 'pending'\",",
            "awaited = NULL WHERE run_id = ?1 AND state IN ('pending', 'running')\",",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "concurrent_resumes_start_one_activation",
    },
    Control {
        label: "outcomes arriving during an activation are never released by it",
        edits: &[(
            JOURNAL,
            "payload_hash FROM mailbox \\\n         WHERE run_id = ?1 ORDER BY seq\",",
            "payload_hash FROM mailbox \\\n         WHERE run_id = ?1 AND arrived_generation < (SELECT generation FROM runs WHERE run_id = ?1) ORDER BY seq\",",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "completion_during_replay_is_released_after_the_prefix",
    },
    Control {
        label: "rejections journaled as fulfilments (caught rejection)",
        edits: &[(
            JOURNAL,
            "            position,\n            settlement,\n            value,\n            hash\n        ],",
            "            position,\n            { let _ = &settlement; \"fulfilled\" },\n            value,\n            hash\n        ],",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "caught_rejection_through_the_parent",
    },
    Control {
        label: "rejections journaled as fulfilments (all-rejected any)",
        edits: &[(
            JOURNAL,
            "            position,\n            settlement,\n            value,\n            hash\n        ],",
            "            position,\n            { let _ = &settlement; \"fulfilled\" },\n            value,\n            hash\n        ],",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "all_rejected_any_through_the_parent",
    },
    Control {
        label: "rejections journaled as fulfilments (early all rejection)",
        edits: &[(
            JOURNAL,
            "            position,\n            settlement,\n            value,\n            hash\n        ],",
            "            position,\n            { let _ = &settlement; \"fulfilled\" },\n            value,\n            hash\n        ],",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "early_all_rejection_through_the_parent",
    },
    Control {
        label: "calls of one run dispatched serially on the activation's thread",
        edits: &[(
            RUNTIME,
            "let handle = thread::spawn(f);",
            "f();\n        let handle = thread::spawn(|| {});",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "calls_of_one_run_are_dispatched_concurrently",
    },
    Control {
        label: "recovery applies a committed local effect again",
        edits: &[(
            DRIVER,
            "if waiting.contains(&row.position) || self.rt.is_inflight(&run_id, row.position) {",
            "if self.rt.is_inflight(&run_id, row.position) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "local_effect_behind_unresolved_call_applies_once",
    },
    Control {
        label: "a synchronous call journaled without its value is not answered again",
        edits: &[(
            DRIVER,
            "if row.class != StoredClass::Sync || row.outcome.is_some() {",
            "if true || row.class != StoredClass::Sync || row.outcome.is_some() {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "sync_call_journaled_without_value_is_answered_again",
    },
    Control {
        label: "clock reads not kept monotonic within a run",
        edits: &[
            (
                DRIVER,
                "let value = last.map_or(now, |last| now.max(last));",
                "let value = last.map_or(now, |_| now);",
            ),
            (
                DRIVER,
                "(Some(c), Some(last)) => Some(c.max(last)),",
                "(Some(c), Some(_)) => Some(c),",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_races"),
        test: "clock_stays_monotonic_across_activations",
    },
    Control {
        label: "the activation's call insert is not fenced",
        edits: &[(
            JOURNAL,
            "WHERE {FENCE} AND ?4 = (SELECT COUNT(*) FROM journal WHERE run_id = ?1)",
            "WHERE (1 OR {FENCE}) AND ?4 = (SELECT COUNT(*) FROM journal WHERE run_id = ?1)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_ownership"),
        test: "owner_loss_before_call_commit_dispatches_nothing",
    },
    Control {
        label: "recovery resends a call this process is still dispatching",
        edits: &[(
            DRIVER,
            "if waiting.contains(&row.position) || self.rt.is_inflight(&run_id, row.position) {",
            "if waiting.contains(&row.position) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_ownership"),
        test: "owner_loss_after_call_commit_dispatches_once",
    },
    Control {
        label: "delivery-order allocation is not fenced",
        edits: &[
            (
                JOURNAL,
                "    if !lease.holds(tx)? {\n        return Err(lease.lost());\n    }\n    let readiness: i64",
                "    let readiness: i64",
            ),
            (
                JOURNAL,
                "payload_hash = ?7, \\\n             delivery_order = {NEXT_ORDER} \\\n             WHERE run_id = ?1 AND position = ?4 AND settlement IS NULL AND {FENCE}\"",
                "payload_hash = ?7, \\\n             delivery_order = {NEXT_ORDER} \\\n             WHERE run_id = ?1 AND position = ?4 AND settlement IS NULL AND (1 OR {FENCE})\"",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_ownership"),
        test: "owner_loss_around_order_commit_delivers_nothing_more",
    },
    Control {
        label: "a call is dispatched before its row commits",
        edits: &[(
            DRIVER,
            "                let run_id = self.lease.run_id.clone();\n                self.rt.register(&run_id, call.position);",
            "                let run_id = self.lease.run_id.clone();\n                self.rt.spawn_dispatch(basal_host::CallRequest { flow_id: flow_id.clone(), run_id: run_id.clone(), position: call.position, kind: call.kind.clone(), args: call.args.clone(), idempotency_key: crate::ids::idempotency_key(&flow_id, &run_id, call.position), attempt: 1 }, class);\n                self.rt.register(&run_id, call.position);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_ownership"),
        test: "storage_failure_before_commit_dispatches_nothing",
    },
    Control {
        label: "recovery resends a mutation that ignores idempotency keys",
        edits: &[(
            DRIVER,
            "(class, _) if class.safe_to_resend() => resend.push(row.clone()),",
            "(_, _) => resend.push(row.clone()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "unknown_unkeyed_mutation_needs_reconcile",
    },
    Control {
        label: "a resend mints a new idempotency key",
        edits: &[(
            RUNTIME,
            "idempotency_key: row.idempotency_key.clone(),",
            "idempotency_key: format!(\"{}-resent\", row.idempotency_key),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "unknown_keyed_mutation_is_reissued_with_the_same_key",
    },
    Control {
        label: "Unavailable retried for every mutation",
        edits: &[(
            RUNTIME,
            "let may_retry = class.safe_to_resend() || proven_unsent;",
            "let may_retry = true || class.safe_to_resend() || proven_unsent;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "unavailable_is_retried_only_when_safe",
    },
    Control {
        label: "a call reconciled as not applied is not sent again",
        edits: &[(
            DRIVER,
            "(_, DispatchState::NotApplied) => resend.push(row.clone()),",
            "(_, DispatchState::NotApplied) => unknown.push(row.position),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "reconcile_not_applied_reissues_with_the_same_key",
    },
    Control {
        label: "completions for a cancelled run are accepted",
        edits: &[(
            JOURNAL,
            "if state == \"cancelled\" {",
            "if false && state == \"cancelled\" {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "reconcile_cancel_ends_the_run_and_refuses_later_completions",
    },
    Control {
        label: "a contradictory completion is taken for a redelivery",
        edits: &[(
            JOURNAL,
            "if waiting == hash {",
            "if true || waiting == hash {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "redelivered_completion_is_a_noop_and_contradictory_one_is_quarantined",
    },
    Control {
        label: "divergence reported as a script error",
        edits: &[(
            DRIVER,
            "self.fail(\"nondeterminism\", format!(\"{n:?}\"), true)",
            "self.fail(\"script\", format!(\"{n:?}\"), true)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_identity"),
        test: "divergence_fails_the_run_as_nondeterminism",
    },
    Control {
        label: "code hash not checked before replay",
        edits: &[(
            DRIVER,
            "if code_hash(&self.run.script, &self.run.manifest) != self.run.code_hash {",
            "if false && code_hash(&self.run.script, &self.run.manifest) != self.run.code_hash {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_identity"),
        test: "code_hash_mismatch_is_engine_mismatch_without_replay",
    },
    Control {
        label: "runtime fingerprint not checked before replay",
        edits: &[(
            DRIVER,
            "Some(recorded) if *recorded != current => {",
            "Some(recorded) if false && *recorded != current => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_identity"),
        test: "fingerprint_mismatch_is_engine_mismatch_without_replay",
    },
    Control {
        label: "admission does not consult tombstones",
        edits: &[(
            ADMISSION,
            "if let Some(run_id) = tomb {",
            "if let Some(run_id) = tomb.filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_admin"),
        test: "admission_deduplicates_and_tombstones_refuse_after_pruning",
    },
    Control {
        label: "admission does not deduplicate",
        edits: &[(
            ADMISSION,
            "if let Some(run_id) = existing {",
            "if let Some(run_id) = existing.filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_admin"),
        test: "admission_deduplicates_and_tombstones_refuse_after_pruning",
    },
    Control {
        label: "pruning ignores open obligations",
        edits: &[(
            RETENTION,
            "if !journal::unsettled_positions(tx, &run_id)?.is_empty() {",
            "if false && !journal::unsettled_positions(tx, &run_id)?.is_empty() {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_admin"),
        test: "pruning_spares_unfinished_runs_and_open_obligations",
    },
    Control {
        label: "pruning touches runs that have not ended",
        edits: &[(
            RETENTION,
            "WHERE state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled') \\\n         AND ended_at IS NOT NULL AND ended_at <= ?1",
            "WHERE state IN ('succeeded', 'failed', 'engine_mismatch', 'cancelled', 'pending', 'suspended') \\\n         AND (ended_at IS NULL OR ended_at <= ?1)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_admin"),
        test: "pruning_spares_unfinished_runs_and_open_obligations",
    },
    Control {
        label: "store runs synchronous = NORMAL",
        edits: &[(
            STORE,
            "c.pragma_update(None, \"synchronous\", \"FULL\")?;",
            "c.pragma_update(None, \"synchronous\", \"NORMAL\")?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_admin"),
        test: "store_runs_synchronous_full_and_the_chosen_fullfsync",
    },
    Control {
        label: "fullfsync choice not applied",
        edits: &[(
            STORE,
            "c.pragma_update(None, \"fullfsync\", durability.fullfsync)?;",
            "c.pragma_update(None, \"fullfsync\", false)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_admin"),
        test: "store_runs_synchronous_full_and_the_chosen_fullfsync",
    },
    Control {
        label: "recovery does not resend queries (cut harness)",
        edits: &[(
            DRIVER,
            "(class, _) if class.safe_to_resend() => resend.push(row.clone()),",
            "(StoredClass::KeyedMutation, _) => resend.push(row.clone()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_cut"),
        test: "every_cut_recovers_to_the_uncut_state",
    },
    Control {
        label: "recovery does not resend queries (cuts in recovery suffixes)",
        edits: &[(
            DRIVER,
            "(class, _) if class.safe_to_resend() => resend.push(row.clone()),",
            "(StoredClass::KeyedMutation, _) => resend.push(row.clone()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_cut"),
        test: "cuts_in_recovery_generated_suffixes_recover_too",
    },
    Control {
        label: "recovery does not resend keyed mutations (parent kill -9)",
        edits: &[(
            DRIVER,
            "(class, _) if class.safe_to_resend() => resend.push(row.clone()),",
            "(StoredClass::Query, _) => resend.push(row.clone()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_kill"),
        test: "killing_the_parent_at_every_boundary_recovers_to_the_uncut_state",
    },
    Control {
        label: "a broken worker fails the run instead of replaying it on another (worker kill -9)",
        edits: &[(
            DRIVER,
            "if self.run.broken + 1 >= self.rt.config.max_broken_activations {",
            "if true || self.run.broken + 1 >= self.rt.config.max_broken_activations {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_kill"),
        test: "killing_the_worker_at_every_boundary_recovers_to_the_uncut_state",
    },
    Control {
        label: "unknown reasons: a keyed call out of retries is recorded with its last send's reason",
        edits: &[(
            RUNTIME,
            "                    UnknownReason::RetriesExhausted\n                } else {\n                    last\n",
            "                    last\n                } else {\n                    last\n",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "a_keyed_call_ambiguous_through_its_retries_is_unknown_as_retries_exhausted",
    },
    Control {
        label: "unknown reasons: a call found sent after a restart is recorded as a lost connection",
        edits: &[(
            DRIVER,
            "journal::record_unknown(tx, &run_id, *p, UnknownReason::BasalRestarted)?;",
            "journal::record_unknown(tx, &run_id, *p, UnknownReason::ConnectionLost)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_unknown"),
        test: "unknown_unkeyed_mutation_needs_reconcile",
    },
    Control {
        label: "unknown reasons: the migration accepts a store holding unknown calls without a reason",
        edits: &[(
            SCHEMA,
            "    SELECT COUNT(*) FROM journal WHERE dispatch = 'unknown';",
            "    SELECT 0;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("unknown_reason"),
        test: "the_migration_refuses_a_store_that_already_holds_an_unknown_call",
    },
    Control {
        label: "unknown reasons: the schema lets a call be unknown without a reason",
        edits: &[(
            SCHEMA,
            "    CHECK (dispatch <> 'unknown' OR unknown_reason IS NOT NULL);",
            ";",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("unknown_reason"),
        test: "a_call_marked_unknown_needs_a_reason_from_the_closed_set",
    },
];

const MANIFEST: &str = "crates/basal-core/src/manifest.rs";
const INSTALL: &str = "crates/basal-core/src/install.rs";
const AUTHORIZE: &str = "crates/basal-core/src/authorize.rs";
const TOKENS: &str = "crates/basal-core/src/tokens.rs";
const KV: &str = "crates/basal-core/src/kv.rs";
const RATE: &str = "crates/basal-core/src/rate.rs";
const SCHEMA: &str = "crates/basal-core/src/schema.rs";

/// Manifests, install validation, authorization at dispatch, the call
/// audit, token reservations, `kv`, the concurrency slot and deadline, the
/// rate limits, disable and the per-run limits: each control disables one
/// mechanism in basal-core and runs the basal-testkit test named for it.
const DISPATCH_CONTROLS: &[Control] = &[
    Control {
        label: "manifest accepts unknown fields",
        edits: &[(
            MANIFEST,
            "#[serde(deny_unknown_fields)]\npub struct Manifest {",
            "pub struct Manifest {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "manifest_refuses_unknown_fields",
    },
    Control {
        label: "manifest accepts both trigger kinds",
        edits: &[(
            MANIFEST,
            "(Some(events), None) => {",
            "(Some(events), _) => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "trigger_needs_exactly_one_kind",
    },
    Control {
        label: "install accepts undeclared events and versions",
        edits: &[(
            INSTALL,
            "let Some(decl) = catalog.event(&e.module, &e.name, e.version) else {",
            "let Some(decl) = catalog.event(&e.module, &e.name, e.version).or(Some(basal_host::EventDecl { origin: basal_host::EventOrigin::Internal, body: EventBody::Inline })) else {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "install_refuses_unknown_events_and_versions",
    },
    Control {
        label: "install accepts ops missing from the catalog",
        edits: &[(
            INSTALL,
            "let Some(decl) = catalog.op(module, op) else {",
            "let Some(decl) = catalog.op(module, op).or(Some(basal_host::OpDecl { kind: Some(OpKind::Query), cause_echo: false, shell_capable: false })) else {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "install_refuses_unknown_and_unmarked_ops",
    },
    Control {
        label: "install accepts ops with no query or mutate marker",
        edits: &[(
            INSTALL,
            "let Some(kind) = decl.kind else {",
            "let Some(kind) = decl.kind.or(Some(OpKind::Query)) else {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "install_refuses_unknown_and_unmarked_ops",
    },
    Control {
        label: "install ignores the catalog's shell-capable marker",
        edits: &[(
            INSTALL,
            "if decl.shell_capable {",
            "if false && decl.shell_capable {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "install_refuses_shell_capable_ops_listed_by_marker_or_denylist",
    },
    Control {
        label: "install ignores the shell denylist",
        edits: &[(
            INSTALL,
            "if denylist.contains(module, op) {",
            "if false && denylist.contains(module, op) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "install_refuses_shell_capable_ops_listed_by_marker_or_denylist",
    },
    Control {
        label: "install accepts unknown agents",
        edits: &[(
            INSTALL,
            "if !catalog.agent_known(agent) {",
            "if false && !catalog.agent_known(agent) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "install_refuses_unknown_agents",
    },
    Control {
        label: "loop install rule not applied",
        edits: &[(
            INSTALL,
            "if trigger_modules.contains(&module.as_str()) && !decl.cause_echo {",
            "if false && trigger_modules.contains(&module.as_str()) && !decl.cause_echo {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "loop_install_rule_needs_the_operator_override",
    },
    Control {
        label: "admission does not require the approved version's exact code",
        edits: &[(
            ADMISSION,
            "if approved.code_hash != code_hash(&spec.script, &spec.manifest) {",
            "if false && approved.code_hash != code_hash(&spec.script, &spec.manifest) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "new_version_applies_to_newly_admitted_triggers_only",
    },
    Control {
        label: "ops outside the manifest are dispatched",
        edits: &[(
            AUTHORIZE,
            "if !manifest.lists_op(module, op) {",
            "if false && !manifest.lists_op(module, op) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_authorization"),
        test: "unlisted_op_is_denied_journaled_and_catchable",
    },
    Control {
        label: "shell denylist not checked at dispatch",
        edits: &[(
            AUTHORIZE,
            "if denylist.contains(module, op) {",
            "if false && denylist.contains(module, op) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_authorization"),
        test: "listed_op_on_the_denylist_is_refused_at_dispatch",
    },
    Control {
        label: "catalog shell marker not checked at dispatch",
        edits: &[(
            AUTHORIZE,
            "Some(decl) if decl.shell_capable => Some(Refusal::denied(format!(",
            "Some(decl) if false && decl.shell_capable => Some(Refusal::denied(format!(",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_authorization"),
        test: "listed_op_marked_shell_capable_is_refused_at_dispatch",
    },
    Control {
        label: "the dispatcher does not refuse sh from the worker",
        edits: &[(
            DRIVER,
            "if call.kind.is_shell() {",
            "if false && call.kind.is_shell() {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_authorization"),
        test: "a_compromised_worker_cannot_issue_sh",
    },
    Control {
        label: "facts of agents outside the manifest's targets are allowed",
        edits: &[(
            AUTHORIZE,
            "if !grant.targets.iter().any(|t| t == agent) {",
            "if false && !grant.targets.iter().any(|t| t == agent) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_authorization"),
        test: "facts_sinks_and_model_calls_are_checked_against_the_manifest",
    },
    Control {
        label: "digest actions above the manifest's cap are allowed",
        edits: &[(
            AUTHORIZE,
            "Some(action) if action <= cap => Ok(()),",
            "Some(_) => Ok(()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_authorization"),
        test: "facts_sinks_and_model_calls_are_checked_against_the_manifest",
    },
    Control {
        label: "audit row written outside the transaction that journals the call (cut harness)",
        edits: &[
            (
                DRIVER,
                "        audit::record(\n            tx,\n            flow_id,\n            &self.lease.run_id,\n            call.position,\n            &call.kind,\n            &ArgsDigest::of(&call.args),\n            audit::ALLOWED,\n            now_ms,\n        )?;\n        Ok(Some(inserted))",
                "        Ok(Some(inserted))",
            ),
            (
                DRIVER,
                "                self.at(Boundary::CallCommitted {\n                    position: call.position,\n                })?;\n",
                "                self.at(Boundary::CallCommitted {\n                    position: call.position,\n                })?;\n                self.rt.store().write(|tx| audit::record(tx, &flow_id, &run_id, call.position, &call.kind, &ArgsDigest::of(&call.args), audit::ALLOWED, audited_at))?;\n",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("journal_cut"),
        test: "every_cut_recovers_to_the_uncut_state",
    },
    Control {
        label: "token reservation ignores outstanding reservations",
        edits: &[(
            TOKENS,
            "\"SELECT reserved + input_tokens + cache_write_tokens + output_tokens + unreported_tokens \\",
            "\"SELECT 0 + input_tokens + cache_write_tokens + output_tokens + unreported_tokens \\",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_tokens"),
        test: "concurrent_llm_calls_cannot_spend_the_same_remainder",
    },
    Control {
        label: "a model call over the cap is dispatched",
        edits: &[(
            TOKENS,
            "if used.saturating_add(r.amount) > r.cap {",
            "if false && used.saturating_add(r.amount) > r.cap {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_tokens"),
        test: "llm_call_over_the_cap_is_a_journaled_rejection",
    },
    Control {
        label: "a re-issued model call sends the script's bytes, not the journaled request",
        edits: &[(
            RUNTIME,
            "args: row.dispatch_args(),",
            "args: row.args.clone(),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_tokens"),
        test: "reissue_after_a_cut_sends_identical_bytes_under_the_same_send_id",
    },
    Control {
        label: "requested output not clamped to the manifest's ceiling",
        edits: &[(TOKENS, "requested.min(grant.max_output)", "requested")],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_tokens"),
        test: "reissue_after_a_cut_sends_identical_bytes_under_the_same_send_id",
    },
    Control {
        label: "usage applied again for a settled send id",
        edits: &[
            (
                TOKENS,
                "FROM token_ledger \\\n                 WHERE send_id = ?1 AND state = 'reserved'\"",
                "FROM token_ledger \\\n                 WHERE send_id = ?1\"",
            ),
            (
                TOKENS,
                "settled_at = ?7 \\\n         WHERE send_id = ?1 AND state = 'reserved'\"",
                "settled_at = ?7 \\\n         WHERE send_id = ?1\"",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_tokens"),
        test: "duplicate_usage_report_is_applied_once",
    },
    Control {
        label: "usage settled in the current window instead of the reservation's",
        edits: &[(
            TOKENS,
            "            window_ms,\n            start,\n            reserved,",
            "            window_ms,\n            crate::tokens::window_start(now_ms, window_ms),\n            reserved,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_tokens"),
        test: "usage_lands_in_the_reservation_window_across_suspension_and_rollover",
    },
    Control {
        label: "kv writes roll back when their run fails",
        edits: &[(
            SCHEMA,
            "    revision INTEGER NOT NULL CHECK (revision >= 1),\n    PRIMARY KEY (flow_id, key)\n);",
            "    revision INTEGER NOT NULL CHECK (revision >= 1),\n    PRIMARY KEY (flow_id, key)\n);\nCREATE TRIGGER kv_rolls_back AFTER UPDATE OF state ON runs WHEN NEW.state = 'failed' BEGIN DELETE FROM kv WHERE flow_id = NEW.flow_id; END;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_kv"),
        test: "kv_writes_survive_a_later_failure_of_their_run",
    },
    Control {
        label: "kv key size not capped",
        edits: &[(
            KV,
            "if key.len() > limits.max_key_bytes {",
            "if false && key.len() > limits.max_key_bytes {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_kv"),
        test: "kv_caps_refuse_with_journaled_rejections",
    },
    Control {
        label: "kv value size not capped",
        edits: &[(
            KV,
            "if value.len() > limits.max_value_bytes {",
            "if false && value.len() > limits.max_value_bytes {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_kv"),
        test: "kv_caps_refuse_with_journaled_rejections",
    },
    Control {
        label: "kv total bytes per flow not capped",
        edits: &[(
            KV,
            "if total - previous + entry > bytes_i64(limits.max_flow_bytes)? {",
            "if false && total - previous + entry > bytes_i64(limits.max_flow_bytes)? {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_kv"),
        test: "kv_caps_refuse_with_journaled_rejections",
    },
    Control {
        label: "recovery applies a committed kv write again",
        edits: &[(
            DRIVER,
            "if waiting.contains(&row.position) || self.rt.is_inflight(&run_id, row.position) {",
            "if self.rt.is_inflight(&run_id, row.position) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_kv"),
        test: "kv_write_is_applied_once_across_every_cut",
    },
    Control {
        label: "a run starts while an earlier run of its flow holds the slot",
        edits: &[(
            RUNS,
            "if let Some(holder) = slot_holder(tx, run_id)? {",
            "if let Some(holder) = slot_holder(tx, run_id)?.filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_slots"),
        test: "second_run_waits_for_the_first_even_while_suspended",
    },
    Control {
        label: "run deadline never enforced (activation and expiry)",
        edits: &[
            (
                DRIVER,
                ".is_some_and(|d| self.rt.config.clock.now_ms() >= d)",
                ".is_some_and(|_| false)",
            ),
            (
                RUNS,
                "deadline_at IS NOT NULL AND deadline_at <= ?1 \\",
                "deadline_at IS NOT NULL AND deadline_at <= ?1 AND 0 \\",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_slots"),
        test: "deadline_fails_a_stuck_run_and_frees_the_slot",
    },
    Control {
        label: "suspended runs never expire",
        edits: &[(
            RUNS,
            "deadline_at IS NOT NULL AND deadline_at <= ?1 \\",
            "deadline_at IS NOT NULL AND deadline_at <= ?1 AND 0 \\",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_slots"),
        test: "deadline_fails_a_suspended_run_and_frees_the_slot",
    },
    Control {
        label: "admission ignores the run rate limit",
        edits: &[(
            ADMISSION,
            "        return Ok(Err(Admission::RateLimited));",
            "        let _ = Admission::RateLimited;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "run_rate_limit_and_auto_disable_after_k_saturated_windows",
    },
    Control {
        label: "sustained saturation never disables the flow",
        edits: &[(RATE, "if streak < k {", "if true || streak < k {")],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "run_rate_limit_and_auto_disable_after_k_saturated_windows",
    },
    Control {
        label: "saturated windows counted without being consecutive",
        edits: &[(
            RATE,
            "AND saturated = 1 AND window_start BETWEEN ?3 AND ?4\",",
            "AND saturated = 1 AND window_start <= ?4\",",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "saturated_windows_that_are_not_consecutive_do_not_disable",
    },
    Control {
        label: "admission ignores a disabled flow",
        edits: &[(
            ADMISSION,
            "Some(flow) if !flow.enabled => return Ok(Err(Admission::Disabled)),",
            "Some(flow) if false && !flow.enabled => return Ok(Err(Admission::Disabled)),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "run_rate_limit_and_auto_disable_after_k_saturated_windows",
    },
    Control {
        label: "dispatch budget not checked",
        edits: &[(
            DRIVER,
            "if let Take::Refused { .. } = rate::check(tx, flow_id, rate::Kind::Dispatch, now_ms, &rate)?",
            "if let Take::Refused { .. } = Take::Allowed",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "dispatch_budget_refuses_beyond_the_window",
    },
    Control {
        label: "a disabled flow's resumed run still dispatches",
        edits: &[(
            DRIVER,
            "        let rate = self.rt.config.rate;\n        if install::is_disabled(tx, flow_id)? {",
            "        let rate = self.rt.config.rate;\n        if false && install::is_disabled(tx, flow_id)? {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "disable_fences_a_resumed_run_while_in_flight_calls_settle",
    },
    Control {
        label: "any agent can disable any flow",
        edits: &[(
            INSTALL,
            "&& record.owner.as_deref() != Some(agent.as_str())",
            "&& false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "only_the_operator_or_the_owning_agent_disables",
    },
    Control {
        label: "per-run host-call limit not checked",
        edits: &[(
            DRIVER,
            "if calls >= limits.max_calls {",
            "if false && calls >= limits.max_calls {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "per_run_call_limit_fails_the_run",
    },
    Control {
        label: "per-run journal byte limit not checked",
        edits: &[(
            DRIVER,
            "if bytes.saturating_add(incoming) > limits.max_journal_bytes {",
            "if false && bytes.saturating_add(incoming) > limits.max_journal_bytes {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "per_run_journal_bytes_limit_fails_the_run",
    },
    Control {
        label: "argument byte cap not checked before parsing",
        edits: &[(
            DRIVER,
            "if call.args.len() > cap {",
            "if false && call.args.len() > cap {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "argument_over_the_cap_is_refused_before_it_is_parsed",
    },
    Control {
        label: "result byte cap not checked",
        edits: &[(
            RUNTIME,
            "(bytes > cap).then(|| {",
            "(false && bytes > cap).then(|| {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_limits"),
        test: "result_over_the_cap_is_recorded_as_a_rejection",
    },
    Control {
        label: "manifest schedule not validated by the scheduler",
        edits: &[(
            MANIFEST,
            "crate::schedule::validate(schedule)\n                    .map_err(|e| ManifestError::Schedule(e.to_string()))?;",
            "let _ = schedule;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "schedule_triggers_are_typed_and_validated_by_the_scheduler",
    },
];

const M_MANIFEST: &str = "crates/basal-module/src/manifest.rs";
const M_CALLER: &str = "crates/basal-module/src/caller.rs";
const M_SERVE: &str = "crates/basal-module/src/serve.rs";
const M_CORE_CONSENT: &str = "crates/basal-host/src/core_consent.rs";
const M_POOL: &str = "crates/basal-module/src/pool.rs";
const M_ENGINE: &str = "crates/basal-module/src/engine.rs";
const M_FATAL: &str = "crates/basal-module/src/fatal.rs";
const M_MODULE: &str = "crates/basal-module/src/module.rs";
const M_DRYRUN: &str = "crates/basal-module/src/dryrun.rs";
const M_OPS: &str = "crates/basal-module/src/ops.rs";
const M_UNCONFIGURED: &str = "crates/basal-module/src/unconfigured.rs";
const CORE_OPS: &str = "crates/basal-core/src/ops.rs";
const M_DECISIONS: &str = "crates/basal-core/src/decisions.rs";
const M_RECONCILE: &str = "crates/basal-core/src/reconcile.rs";

/// The module shell: each control disables one mechanism in basal-module
/// (or in a basal-core function only the module calls, such as the health
/// figures) and runs the basal-module test named for it.
const MODULE_CONTROLS: &[Control] = &[
    Control {
        label: "the manifest declares a capability subc-protocol's grammar refuses",
        edits: &[(
            M_MANIFEST,
            ".capabilities(None)",
            ".capabilities(Some(subc_protocol::manifest::CapabilityDeclarations { provides: vec![\"Not A Capability\".into()], requires: Vec::new(), must_never_reach: Vec::new() }))",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("manifest_gate"),
        test: "the_built_binary_prints_a_manifest_subc_protocol_accepts",
    },
    Control {
        label: "the manifest leaves out an op the module serves",
        edits: &[(
            M_MANIFEST,
            "operations: OPERATIONS\n                .iter()",
            "operations: OPERATIONS\n                .iter()\n                .skip(1)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("manifest_gate"),
        test: "the_built_binary_prints_a_manifest_subc_protocol_accepts",
    },
    Control {
        label: "an agent id in a scope nobody vouched for is taken as the caller",
        edits: &[(
            M_CALLER,
            "core_owned && scope.owner_authorized",
            "core_owned",
        )],
        also_restore: NO_EXTRA,
        target: Target::ModuleLib,
        test: "caller::tests::identity_comes_only_from_the_stamp",
    },
    Control {
        label: "the scope stamp on a route's bind is dropped",
        edits: &[(M_SERVE, "scope: request.scope.clone(),", "scope: None,")],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "an_owner_authorized_core_scope_names_the_agent_and_its_scope",
    },
    Control {
        label: "a scope owned by any module names its agent",
        edits: &[(
            M_CALLER,
            "Principal::Reserved { module_id } if module_id == CORE_MODULE\n        );",
            "Principal::Reserved { .. }\n        );",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "a_scope_owned_by_another_module_names_no_agent",
    },
    Control {
        label: "an agent id in a core scope the daemon does not vouch for names the agent",
        edits: &[(
            M_CALLER,
            "core_owned && scope.owner_authorized",
            "core_owned",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "a_scope_without_owner_authorization_names_no_agent",
    },
    Control {
        label: "an unscoped route that is neither direct nor core's is taken as the operator",
        edits: &[(
            M_CALLER,
            "other => Caller::Other(principal_label(other)),",
            "_ => Caller::Operator,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "a_route_without_a_scope_is_never_an_agent",
    },
    Control {
        label: "a route that has gone keeps the caller it was bound with",
        edits: &[(
            M_SERVE,
            "lock(&self.stamps).remove(&(handle.channel, handle.epoch));",
            "let _ = handle;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "a_route_without_a_scope_is_never_an_agent",
    },
    Control {
        label: "a vouched agent may run a capture dry run of a flow it does not own",
        edits: &[(
            M_OPS,
            "(\n                Caller::Agent {\n                    agent_id: agent, ..\n                },\n                Mode::Capture,\n            ) if self.owns(agent, &p.flow_id)? => {}",
            "(Caller::Agent { .. }, Mode::Capture) => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "an_agent_cannot_act_on_a_flow_it_does_not_own",
    },
    Control {
        label: "a vouched agent may enable a flow it does not own (the module's ownership check and the core's both removed)",
        edits: &[
            (
                M_OPS,
                "Caller::Agent {\n                agent_id: agent, ..\n            } if self.owns(agent, &p.flow_id)? => Actor::Agent(agent.to_owned()),",
                "Caller::Agent { agent_id: agent, .. } => Actor::Agent(agent.to_owned()),",
            ),
            (
                INSTALL,
                "            if record.owner.as_deref() != Some(agent.as_str()) {",
                "            if false && record.owner.as_deref() != Some(agent.as_str()) {",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "an_agent_cannot_act_on_a_flow_it_does_not_own",
    },
    Control {
        label: "an unscoped direct key-holder is the operator",
        edits: &[(
            M_CALLER,
            "Some(Principal::Direct) => Caller::Local,",
            "Some(Principal::Direct) => Caller::Operator,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "any reserved module is the operator, not only the attested callosum",
        edits: &[(
            M_CALLER,
            "Some(Principal::Reserved { module_id }) if module_id == OPERATOR_MODULE => Caller::Operator,",
            "Some(Principal::Reserved { .. }) => Caller::Operator,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "a local caller refused an operator action is told not_permitted, not operator_attestation_required",
        edits: &[(M_OPS, "if *caller == Caller::Local {", "if false {")],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "a local caller may replace a flow someone else wrote",
        edits: &[(
            M_OPS,
            "if owners.iter().any(|o| o != LOCAL_AUTHOR) {",
            "if false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "a_local_caller_installs_only_its_own_flows_with_a_digest_sink",
    },
    Control {
        label: "a local caller may install in another author's name",
        edits: &[(
            M_OPS,
            "if p.author.as_ref().is_some_and(|a| a != LOCAL_AUTHOR) {",
            "if false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "a_local_caller_installs_only_its_own_flows_with_a_digest_sink",
    },
    Control {
        label: "a local caller may override the loop install rule",
        edits: &[(
            M_OPS,
            "if p.loop_override {\n                    return Err(OpError::operator_attestation_required(",
            "if false {\n                    return Err(OpError::operator_attestation_required(",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "a_local_caller_installs_only_its_own_flows_with_a_digest_sink",
    },
    Control {
        label: "a local caller's flow without a digest sink is installed and its card raised",
        edits: &[(M_OPS, "if manifest.sinks.is_empty() {", "if false {")],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "a_local_caller_installs_only_its_own_flows_with_a_digest_sink",
    },
    Control {
        label: "a local caller's card names the operator as its author",
        edits: &[(
            M_OPS,
            "Caller::Local => Ok(json!({ \"local\": true })),",
            "Caller::Local => Ok(json!({ \"operator\": true })),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "a_local_caller_installs_only_its_own_flows_with_a_digest_sink",
    },
    Control {
        label: "the consent host accepts the retired agent author core no longer takes",
        edits: &[(
            M_CORE_CONSENT,
            "if key == \"scope\" && !scope_ref.is_empty() => {",
            "if (key == \"scope\" || key == \"agent\") && !scope_ref.is_empty() => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "card_author_is_scope_operator_or_local_and_nothing_else",
    },
    Control {
        label: "the consent host sends an author that is not one of core's three forms",
        edits: &[(
            M_CORE_CONSENT,
            "        _ => Err(refused()),",
            "        _ => Ok(AuthorKind::Operator),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "card_author_is_scope_operator_or_local_and_nothing_else",
    },
    Control {
        label: "an agent's scope_ref is taken from its agent id instead of the route's stamp",
        edits: &[(
            M_CALLER,
            "scope_ref: scope.scope_ref.clone(),",
            "scope_ref: agent.clone(),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("caller_binding"),
        test: "an_owner_authorized_core_scope_names_the_agent_and_its_scope",
    },
    Control {
        label: "an agent's card author names its agent id instead of the scope_ref it was stamped with",
        edits: &[(
            M_OPS,
            "Caller::Agent { scope_ref, .. } => Ok(json!({ \"scope\": scope_ref })),",
            "Caller::Agent { agent_id, .. } => Ok(json!({ \"scope\": agent_id })),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "install_card_is_byte_exact_and_scope_comes_from_the_route_stamp",
    },
    Control {
        label: "core's refusal of the author scope is treated as an ordinary consent refusal",
        edits: &[(
            M_CORE_CONSENT,
            "if code == SCOPE_UNKNOWN || code == SCOPE_ENDED =>",
            "if false && (code == SCOPE_UNKNOWN || code == SCOPE_ENDED) =>",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "core_refusing_the_author_scope_is_a_clear_install_error",
    },
    Control {
        label: "a pending card is raised again in the name of whoever first installed it",
        edits: &[(
            M_OPS,
            "raised_fields[\"wire_author\"] = card_author(caller)?;",
            "let _ = card_author(caller)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "core_refusing_the_author_scope_is_a_clear_install_error",
    },
    Control {
        label: "an idle worker bound to any flow is reused for another",
        edits: &[(
            M_POOL,
            "let reuse = state.bound.get_mut(flow).and_then(Vec::pop);",
            "let reuse = state.bound.values_mut().find_map(Vec::pop);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "a_worker_is_never_reused_across_flows",
    },
    Control {
        label: "a killed worker goes back to its flow's idle list and is handed out again",
        edits: &[
            (
                M_POOL,
                "let keep = match (&lease.binding, lease.killed || crashed) {",
                "let keep = match (&lease.binding, false) {",
            ),
            (
                M_POOL,
                "if let Some(mut idle) = reuse {\n                    if idle.process.has_exited() {",
                "if let Some(mut idle) = reuse {\n                    if false && idle.process.has_exited() {",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "a_killed_worker_is_replaced_and_the_run_continues",
    },
    Control {
        label: "a killed worker is not owed a replacement",
        edits: &[(
            M_POOL,
            "Metrics::bump(&self.shared.metrics.workers_killed);\n                    self.shared.lost(&mut state, true);",
            "Metrics::bump(&self.shared.metrics.workers_killed);\n                    self.shared.lost(&mut state, false);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "a_killed_worker_is_replaced_and_the_run_continues",
    },
    Control {
        label: "warm spares are never handed out",
        edits: &[(
            M_POOL,
            "if let Some((id, mut process)) = state.spares.pop_front() {",
            "if let Some((id, mut process)) = None::<(u64, WorkerProcess)> {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "an_activation_uses_a_warm_spare",
    },
    Control {
        label: "a worker is never retired for its activation count",
        edits: &[(
            M_POOL,
            "if activations >= self.shared.config.max_activations => None,",
            "if activations >= u32::MAX => None,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "a_bound_worker_is_retired_after_its_activation_count_and_its_idle_period",
    },
    Control {
        label: "a bound worker is never retired for idling",
        edits: &[(
            M_POOL,
            "} else if now.saturating_sub(idle.idle_since_ms) >= idle_ms {",
            "} else if idle_ms < 0 && now.saturating_sub(idle.idle_since_ms) >= idle_ms {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "a_bound_worker_is_retired_after_its_activation_count_and_its_idle_period",
    },
    Control {
        label: "the run is claimed before its worker is spawned and greeted",
        edits: &[(
            M_ENGINE,
            "        let mut lease = match self.inner.pool.acquire(Binding::Flow(flow_id.to_owned())) {\n            Ok(lease) => lease,\n            Err(e) => {\n                tracing::warn!(target: \"engine\", run = %run_id, \"no worker for the run: {e}\");\n                return None;\n            }\n        };\n        lease.serve_run(run_id);\n        Some(self.inner.rt.activate(run_id, &mut lease))",
            "        let _ = (flow_id, Binding::DryRun(String::new()));\n        Some(self.inner.rt.resume(run_id))",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("pool"),
        test: "the_run_deadline_starts_after_the_worker_answers_its_handshake",
    },
    Control {
        label: "activations are not bounded",
        edits: &[(
            M_ENGINE,
            "if active.runs.len() >= inner.config.max_concurrent_activations {",
            "if active.runs.len() >= usize::MAX {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("engine"),
        test: "activations_are_bounded_across_flows",
    },
    Control {
        label: "a storage error does not raise the fatal latch (in process)",
        edits: &[(
            M_FATAL,
            "        if slot.is_none() {\n            *slot = Some(why);",
            "        if false {\n            *slot = Some(why);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("engine"),
        test: "a_storage_error_stops_the_engine_and_raises_the_fatal_latch",
    },
    Control {
        label: "a storage error does not end the process",
        edits: &[(
            M_FATAL,
            "        if slot.is_none() {\n            *slot = Some(why);",
            "        if false {\n            *slot = Some(why);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("e2e"),
        test: "a_store_error_ends_the_process_non_zero_and_the_restart_recovers",
    },
    Control {
        label: "the module starts without recovering runs left running",
        edits: &[(
            M_MODULE,
            "let recovered = rt.recover().map_err(|e| format!(\"recovering runs: {e}\"))?;",
            "let recovered: Vec<String> = Vec::new();",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("e2e"),
        test: "a_schedule_flow_survives_a_catch_up_a_worker_kill_and_a_module_kill_with_each_write_once",
    },
    Control {
        label: "a card's approval is applied as a rejection",
        edits: &[(
            M_MODULE,
            "CardDecision::Approve => Decision::Approve,",
            "CardDecision::Approve => Decision::Reject,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "install_raises_one_card_per_version_and_its_decision_approves_or_rejects",
    },
    Control {
        label: "capture mode sends calls to the host",
        edits: &[(
            M_DRYRUN,
            "if !self.runs_live(&request.kind) {",
            "if false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("dry_run"),
        test: "capture_mode_executes_no_host_call_and_replays_the_schedule_window",
    },
    Control {
        label: "a delivered captured rejection does not taint what follows",
        edits: &[(
            M_DRYRUN,
            "Event::Delivered(p) if captured.contains(p) => tainted = true,",
            "Event::Delivered(p) if captured.contains(p) && false => tainted = true,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("dry_run"),
        test: "a_captured_call_the_script_uses_marks_the_trace_partial",
    },
    Control {
        label: "live mode runs ops that are not marked query",
        edits: &[(
            M_DRYRUN,
            ".is_some_and(|d| d.kind == Some(OpKind::Query) && !d.shell_capable),",
            ".is_some(),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("dry_run"),
        test: "live_mode_runs_only_query_ops_and_only_for_the_operator",
    },
    Control {
        label: "an owning agent may ask for a live dry run",
        edits: &[(
            M_OPS,
            "(\n                Caller::Agent {\n                    agent_id: agent, ..\n                },\n                Mode::Capture,\n            ) if self.owns(agent, &p.flow_id)? => {}",
            "(Caller::Agent { agent_id: agent, .. }, _) if self.owns(agent, &p.flow_id)? => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("dry_run"),
        test: "live_mode_runs_only_query_ops_and_only_for_the_operator",
    },
    Control {
        label: "dry runs share one scratch store that is never removed",
        edits: &[
            (
                M_DRYRUN,
                "\"dry-{}-{}-{n}\",\n            std::process::id(),\n            request.now_ms\n        ));",
                "\"dry-shared{}\",\n            n * 0\n        ));",
            ),
            (
                M_DRYRUN,
                "let _ = std::fs::remove_dir_all(&dir);",
                "let _ = &dir;",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Module("dry_run"),
        test: "each_dry_run_has_its_own_scratch_store_and_the_real_kv_and_runs_are_untouched",
    },
    Control {
        label: "any agent may run a capture dry run of another's flow",
        edits: &[(
            M_OPS,
            "(\n                Caller::Agent {\n                    agent_id: agent, ..\n                },\n                Mode::Capture,\n            ) if self.owns(agent, &p.flow_id)? => {}",
            "(Caller::Agent { .. }, Mode::Capture) => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "agents may read flow.health",
        edits: &[(
            M_OPS,
            "if !matches!(caller, Caller::Operator | Caller::Core) {",
            "if matches!(caller, Caller::Other(_)) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "anyone may reconcile",
        edits: &[(
            M_OPS,
            "        if *caller != Caller::Operator {\n            return Err(OpError::not_permitted(\"flow.reconcile\", caller));",
            "        if false {\n            return Err(OpError::not_permitted(\"flow.reconcile\", caller));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "anyone may drain",
        edits: &[(
            M_OPS,
            "        if *caller != Caller::Operator {\n            return Err(OpError::not_permitted(\"flow.drain\", caller));",
            "        if false {\n            return Err(OpError::not_permitted(\"flow.drain\", caller));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "any agent may enable any flow (the module's ownership check and the core's both removed)",
        edits: &[
            (
                M_OPS,
                "Caller::Agent {\n                agent_id: agent, ..\n            } if self.owns(agent, &p.flow_id)? => Actor::Agent(agent.to_owned()),",
                "Caller::Agent { agent_id: agent, .. } => Actor::Agent(agent.to_owned()),",
            ),
            (
                INSTALL,
                "            if record.owner.as_deref() != Some(agent.as_str()) {",
                "            if false && record.owner.as_deref() != Some(agent.as_str()) {",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "the owner may not re-enable even a disable it made itself",
        edits: &[(
            INSTALL,
            "Some(by) if by == own => {}",
            "Some(by) if by == own && false => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "the owner may undo the operator's disable",
        edits: &[(
            INSTALL,
            "                _ => return Err(refused()),",
            "                _ => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "the owner may undo an auto-disable",
        edits: &[(
            INSTALL,
            "Some(RUNTIME_ACTOR) => return Err(refused()),",
            "Some(RUNTIME_ACTOR) => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
    },
    Control {
        label: "an operator disable of a flow the owner disabled changes nothing",
        edits: &[(
            INSTALL,
            "let takes_over = matches!(actor, Actor::Operator(_))",
            "let takes_over = false && matches!(actor, Actor::Operator(_))",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "an_operator_disable_takes_over_the_owners_disable",
    },
    Control {
        label: "health has no as_of",
        edits: &[(M_OPS, "\"as_of\": as_of,", "\"as_of_ms\": now,")],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health ignores flow_ids",
        edits: &[(
            M_OPS,
            ".is_none_or(|ids| ids.iter().any(|id| id == flow_id))",
            ".is_none_or(|ids| ids.is_empty() || flow_id.is_empty())",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's last run has no outcome",
        edits: &[(M_OPS, "\"outcome\": r.state,", "\"state\": r.state,")],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's last run time is epoch milliseconds, not RFC 3339",
        edits: &[(
            M_OPS,
            "\"at\": rfc3339(r.ended_at)?,",
            "\"at\": r.ended_at,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's overdue age is null when nothing is overdue",
        edits: &[(
            M_OPS,
            "\"oldest_overdue_age_ms\": f.oldest_overdue_ms.unwrap_or(0),",
            "\"oldest_overdue_age_ms\": f.oldest_overdue_ms,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's needs_reconcile is a list of runs, not a boolean",
        edits: &[(
            M_OPS,
            "\"needs_reconcile\": !f.needs_reconcile.is_empty(),",
            "\"needs_reconcile\": f.needs_reconcile.clone(),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's overflowed is left out",
        edits: &[(M_OPS, "\"overflowed\": false,", "\"overflow\": false,")],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's auto_disabled is null when false",
        edits: &[(
            M_OPS,
            "\"auto_disabled\": f.auto_disabled,",
            "\"auto_disabled\": f.auto_disabled.then_some(true),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health's state is outside the contract's enum",
        edits: &[(
            M_OPS,
            "(Some(_), true) => \"enabled\",",
            "(Some(_), true) => \"active\",",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("health_contract"),
        test: "flow_health_decodes_as_core_decodes_it",
    },
    Control {
        label: "health reports a flow nobody approved by its enabled switch",
        edits: &[(
            M_OPS,
            "(None, _) => \"unapproved\",",
            "(None, _) => if f.enabled { \"enabled\" } else { \"disabled\" },",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "health_lists_a_flow_with_no_approved_version_as_unapproved",
    },
    Control {
        label: "health does not say who disabled a flow",
        edits: &[(M_OPS, "\"by\": disabled_kind(by),", "\"by\": by,")],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "health_reports_flows_runs_and_the_module",
    },
    Control {
        label: "an agent may install a flow in another author's name",
        edits: &[(
            M_OPS,
            "if p.author.as_ref().is_some_and(|a| a != agent) {",
            "if p.author.as_ref().is_some_and(|a| a == \"nobody\" && a != agent) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "install_refuses_what_an_agent_may_not_do",
    },
    Control {
        label: "health reports no oldest pending run",
        edits: &[(
            CORE_OPS,
            "oldest_pending_ms: oldest(\"pending\")?,",
            "oldest_pending_ms: oldest(\"pending\")?.filter(|_| false),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "health_reports_flows_runs_and_the_module",
    },
    Control {
        label: "health does not count failed runs as consecutive failures",
        edits: &[(
            CORE_OPS,
            "\"failed\" | \"engine_mismatch\" => consecutive_failures += 1,",
            "\"engine_mismatch\" => consecutive_failures += 1,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "health_reports_flows_runs_and_the_module",
    },
    Control {
        label: "the unconfigured host's refusal is not proven unsent",
        edits: &[(
            M_UNCONFIGURED,
            "sent: Sent::Never,",
            "sent: Sent::Maybe(basal_host::UnknownReason::ConnectionLost),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("engine"),
        test: "the_unconfigured_host_refuses_every_dispatch_as_never_sent",
    },
    // Operator decision cards (`docs/findings/decision-cards.md`).
    Control {
        label: "decision cards: every raise goes out under a fresh dedup key, so a changed card is a second card",
        edits: &[(
            M_CORE_CONSENT,
            "request[\"dedup_key\"] = json!(key);",
            "request[\"dedup_key\"] = json!(format!(\"{key}:{}\", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_changed_decision_updates_its_card_and_another_unknown_call_gets_its_own",
    },
    Control {
        label: "decision cards: every raise goes out under a fresh dedup key, so a re-raise after a crash is a second card",
        edits: &[(
            M_CORE_CONSENT,
            "request[\"dedup_key\"] = json!(key);",
            "request[\"dedup_key\"] = json!(format!(\"{key}:{}\", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_crash_between_the_intent_and_core_accepting_still_shows_exactly_one_card",
    },
    Control {
        label: "decision cards: a decision already answered for this send of the call is written again",
        edits: &[(
            M_DECISIONS,
            "        if decided {",
            "        if decided && false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_reconcile_option_and_the_default_on_expiry_apply_through_the_journaled_path",
    },
    Control {
        label: "decision cards: a changed decision is not raised again under its key",
        edits: &[(
            M_DECISIONS,
            "card = ?3, revision = revision + 1 \\",
            "card = ?3, revision = revision \\",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_changed_decision_updates_its_card_and_another_unknown_call_gets_its_own",
    },
    Control {
        label: "decision cards: an answer to a decision settled another way is applied instead of recorded stale",
        edits: &[(
            M_DECISIONS,
            "Some(_) if !stands(tx, &card)? => {",
            "Some(_) if false && !stands(tx, &card)? => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "an_answer_after_the_decision_was_settled_another_way_is_stale",
    },
    Control {
        label: "decision cards: an answer delivered twice is applied twice",
        edits: &[(
            M_DECISIONS,
            "if card.state != CardState::Open {",
            "if false && card.state != CardState::Open {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_duplicate_answer_changes_nothing",
    },
    Control {
        label: "decision cards: the audit row of a resolution answered on a card omits the elicitation id",
        edits: &[(
            M_RECONCILE,
            "            Some(position),\n            detail,\n            elicitation_id,\n        )",
            "            Some(position),\n            detail,\n            elicitation_id.filter(|_| false),\n        )",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_reconcile_option_and_the_default_on_expiry_apply_through_the_journaled_path",
    },
    Control {
        label: "decision cards: a card whose raise failed is recorded as raised, so the intent is lost",
        edits: &[(
            M_ENGINE,
            "                Err(e) => {\n                    tracing::warn!(target: \"consent\", key = %record.dedup_key",
            "                Err(e) => {\n                    inner.rt.decision_raised(record.seq, record.revision, \"lost\")?;\n                    tracing::warn!(target: \"consent\", key = %record.dedup_key",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_crash_between_the_intent_and_core_accepting_still_shows_exactly_one_card",
    },
    Control {
        label: "decision cards: a page is acknowledged although applying its answer failed",
        edits: &[(
            M_CORE_CONSENT,
            "if let Err(e) = sink.answer(&answer) {",
            "if let Some(e) = sink.answer(&answer).err().filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_crash_between_receiving_an_answer_and_applying_it_applies_it_exactly_once",
    },
    Control {
        label: "decision cards: 'It ran' releases an invented null result instead of reconciled_as_applied",
        edits: &[(
            M_DECISIONS,
            "APPLIED => Resolution::ReconciledAsApplied,",
            "APPLIED => Resolution::ObservedResult(basal_host::HostOutcome::fulfilled(basal_proto::JsonText::null())),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_reconcile_option_and_the_default_on_expiry_apply_through_the_journaled_path",
    },
    Control {
        label: "decision cards: the reconciled_as_applied rejection depends on when it was produced",
        edits: &[(
            M_RECONCILE,
            "\"the operator reconciled this call as applied; its result was not observed\",",
            "&format!(\"the operator reconciled this call as applied at {}\", crate::store::now_ms()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "reconciled_as_applied_is_journaled_and_replays_the_same_rejection",
    },
    Control {
        label: "decision cards: an expired card applies an action instead of its default",
        edits: &[(
            M_DECISIONS,
            "let state = match answer.choice.as_deref() {",
            "let state = match answer.choice.as_deref().or(Some(APPLIED)) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_reconcile_option_and_the_default_on_expiry_apply_through_the_journaled_path",
    },
    Control {
        label: "decision cards: a re-enable answer does not enable the flow",
        edits: &[(
            M_DECISIONS,
            "install::enable(tx, &card.flow_id, now_ms)",
            "install::enable(tx, \"\", now_ms)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "an_auto_disabled_flow_raises_one_reenable_card_and_each_option_applies",
    },
    Control {
        label: "decision cards: an auto-disable writes no re-enable card intent",
        edits: &[(
            RATE,
            "crate::decisions::record_auto_disable(tx, flow_id, &rule, now_ms)?;",
            "let _ = (&rule, now_ms);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "an_auto_disabled_flow_raises_one_reenable_card_and_each_option_applies",
    },
    Control {
        label: "decision cards: action options carry an effect core does not take",
        edits: &[(
            M_CORE_CONSENT,
            "pub const DECISION_ACTION_EFFECT: &str = \"choose\";",
            "pub const DECISION_ACTION_EFFECT: &str = \"grant\";",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "a_run_in_needs_reconcile_raises_one_card_per_unknown_call_to_the_operator_only",
    },
    Control {
        label: "decision cards: the default is an action rather than the declining option",
        edits: &[(
            M_CORE_CONSENT,
            "\"default\":default,",
            "\"default\":card.options[0].id,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "the_request_builder_reproduces_core_v1_vectors_byte_for_byte",
    },
    Control {
        label: "decision cards: the v1 request carries a field core's vectors do not",
        edits: &[(
            M_CORE_CONSENT,
            "\"scope\":{\"flow_id\":card.flow_id},",
            "\"scope\":{\"flow_id\":card.flow_id,\"version\":card.version},",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "the_request_builder_reproduces_core_v1_vectors_byte_for_byte",
    },
    Control {
        label: "decision cards: the v2 body leaves out the unknown reason",
        edits: &[(
            M_CORE_CONSENT,
            "            body[\"unknown_reason\"] = json!(unknown_reason.as_str());\n",
            "            let _ = unknown_reason;\n",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "the_v2_body_carries_the_typed_context_field_for_field",
    },
    Control {
        label: "decision cards: the v2 body goes on the wire before core accepts it",
        edits: &[(
            M_CORE_CONSENT,
            "pub const FLOW_DECISION_BODY: FlowDecisionBody = FlowDecisionBody::V1;",
            "pub const FLOW_DECISION_BODY: FlowDecisionBody = FlowDecisionBody::V2;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "the_request_builder_reproduces_core_v1_vectors_byte_for_byte",
    },
    Control {
        label: "decision cards: an expiry naming an option is applied as that option",
        edits: &[(
            M_CORE_CONSENT,
            "(Some(\"expired\"), _) => None,",
            "(Some(\"expired\"), choice) => choice.map(str::to_owned),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "an_expiry_is_the_decline_whatever_choice_it_names",
    },
    Control {
        label: "decision cards: a run-limit auto-disable is recorded as a dispatch-limit one",
        edits: &[(
            RATE,
            "Kind::Run => basal_host::DisabledReason::RunLimitSaturated,",
            "Kind::Run => basal_host::DisabledReason::DispatchLimitSaturated,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "an_auto_disabled_flow_raises_one_reenable_card_and_each_option_applies",
    },
    Control {
        label: "decision cards: the re-enable card records the other limit than the one that tripped",
        edits: &[(
            RATE,
            "            limit: max,\n",
            "            limit: limits.max_dispatches,\n",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "an_auto_disabled_flow_raises_one_reenable_card_and_each_option_applies",
    },
    Control {
        label: "decision cards: the prompt misstates why the outcome is unknown",
        edits: &[(
            M_DECISIONS,
            "UnknownReason::BasalRestarted => \"basal restarted before the reply was saved\",",
            "UnknownReason::BasalRestarted => \"the connection closed before a reply came\",",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_unknown_site_records_its_reason_and_the_card_shows_it",
    },
    Control {
        label: "unknown reasons: a timed-out reply is recorded as a lost connection",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "Self::TimedOut(_) => Some(UnknownReason::ReplyTimeout),",
            "Self::TimedOut(_) => Some(UnknownReason::ConnectionLost),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_unknown_site_records_its_reason_and_the_card_shows_it",
    },
    Control {
        label: "unknown reasons: an unreadable reply is recorded as a lost connection",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "Self::Unreadable(_) => Some(UnknownReason::ReplyUnreadable),",
            "Self::Unreadable(_) => Some(UnknownReason::ConnectionLost),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("decisions"),
        test: "each_unknown_site_records_its_reason_and_the_card_shows_it",
    },
];

const SCHED_SPEC: &str = "crates/basal-core/src/schedule/spec.rs";
const SCHED_DUE: &str = "crates/basal-core/src/schedule/due.rs";
const SCHED_TICK: &str = "crates/basal-core/src/schedule/tick.rs";
const SCHED_TABLE: &str = "crates/basal-core/src/schedule/table.rs";

/// The scheduler: each control disables one mechanism in basal-core's
/// schedule module and runs the basal-testkit test named for it.
const SCHEDULE_CONTROLS: &[Control] = &[
    Control {
        label: "a gap time fires at the compatible reading (shifted by the gap's length) instead of at the transition",
        edits: &[(
            SCHED_DUE,
            "            tz.following(before_transition)\n                .next()\n                .map(|t| t.timestamp())",
            "            ambiguous.compatible().ok()",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "spring_forward_gap_fires_once_at_the_first_valid_instant",
    },
    Control {
        label: "an overlap time fires at its second occurrence",
        edits: &[(
            SCHED_DUE,
            "AmbiguousOffset::Fold { .. } => ambiguous\n            .earlier()",
            "AmbiguousOffset::Fold { .. } => ambiguous\n            .later()",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "fall_back_overlap_fires_once_at_the_first_occurrence",
    },
    Control {
        label: "matches landing on an instant already due are not skipped",
        edits: &[(
            SCHED_DUE,
            "        if at > after {\n            return Ok(Some(at));",
            "        if at >= after {\n            return Ok(Some(at));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "half_hourly_cron_collapses_the_gap_and_skips_the_repeated_pass",
    },
    Control {
        label: "an interval's next due time counts from the tick, not the anchor",
        edits: &[(
            SCHED_DUE,
            "let next = timestamp_ms(newest_ms + every);",
            "let next = timestamp_ms(i128::from(now.as_millisecond()) + every);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "interval_due_times_are_the_anchor_plus_whole_periods",
    },
    Control {
        label: "once fires every kept missed due time instead of one catch-up",
        edits: &[(
            SCHED_TICK,
            "MissedPolicy::Once => fires.push(fire(last_missed)),",
            "MissedPolicy::Once => fires.extend(kept_missed.iter().map(|d| fire(*d))),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_sleep_gap_under_once_makes_one_catch_up_fire_with_count_and_window",
    },
    Control {
        label: "skip fires a catch-up",
        edits: &[(
            SCHED_TICK,
            "MissedPolicy::Skip => {}",
            "MissedPolicy::Skip => fires.push(fire(last_missed)),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_sleep_gap_under_skip_makes_no_fire",
    },
    Control {
        label: "each ignores its cap",
        edits: &[(
            SCHED_TICK,
            "let start = kept_missed.len().saturating_sub(cap);",
            "let start = cap.saturating_sub(cap);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_sleep_gap_under_each_fires_the_newest_up_to_the_cap_oldest_first",
    },
    Control {
        label: "each admits its fires newest first",
        edits: &[(
            SCHED_TICK,
            "fires.extend(kept_missed[start..].iter().map(|d| fire(*d)));",
            "fires.extend(kept_missed[start..].iter().rev().map(|d| fire(*d)));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_sleep_gap_under_each_fires_the_newest_up_to_the_cap_oldest_first",
    },
    Control {
        label: "a due time is on time however late it is seen (sleep not counted as missed)",
        edits: &[(
            SCHED_TICK,
            "let on_time = now.duration_since(newest) <= config.grace();",
            "let on_time = now.duration_since(newest) <= config.grace() || true;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_single_due_time_seen_after_the_grace_period_is_missed",
    },
    Control {
        label: "an on-time due time does not fire when missed ones precede it",
        edits: &[(
            SCHED_TICK,
            "    if on_time {\n        fires.push(PlannedFire {",
            "    if on_time && missed_count == 0 {\n        fires.push(PlannedFire {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "an_on_time_due_time_fires_after_the_catch_up",
    },
    Control {
        label: "the missed count walks without its limit",
        edits: &[(
            SCHED_DUE,
            "if count == count_limit {",
            "if count == u64::MAX {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_missed_count_past_the_limit_is_reported_as_a_lower_bound",
    },
    Control {
        label: "the schedule advance is not committed with its planned fires",
        edits: &[(
            SCHED_TICK,
            "params![flow_id, scan.next.map(to_ms), last_fired, at],",
            "params![flow_id, row.next_due.map(to_ms), last_fired, at],",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_restart_after_planning_a_catch_up_mints_no_second_fire",
    },
    Control {
        label: "planned fires are not persisted with the advance",
        edits: &[(
            SCHED_TICK,
            "for fire in &fires {",
            "for fire in fires.iter().filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_restart_after_planning_a_catch_up_mints_no_second_fire",
    },
    Control {
        label: "planned fires are not persisted (restart mid-admission)",
        edits: &[(
            SCHED_TICK,
            "for fire in &fires {",
            "for fire in fires.iter().filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_restart_between_admitting_planned_fires_admits_each_once",
    },
    Control {
        label: "planned fires are admitted newest first",
        edits: &[(
            SCHED_TICK,
            "FROM schedule_fires ORDER BY seq LIMIT 1",
            "FROM schedule_fires ORDER BY seq DESC LIMIT 1",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_restart_between_admitting_planned_fires_admits_each_once",
    },
    Control {
        label: "the schedule is not advanced and fire ids are not derived from the due time",
        edits: &[
            (
                SCHED_TICK,
                "params![flow_id, scan.next.map(to_ms), last_fired, at],",
                "params![flow_id, row.next_due.map(to_ms), last_fired, at],",
            ),
            (
                SCHED_TICK,
                "format!(\"schedule:{due}\")",
                "format!(\"schedule:{due}:{:?}\", std::time::Instant::now())",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_repeated_tick_with_the_same_now_admits_nothing_new",
    },
    Control {
        label: "a newer version counts its due times from the old version's last fire",
        edits: &[(
            SCHED_TABLE,
            "let counted_from = now;",
            "let counted_from = old.last_fired_due.unwrap_or(now);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "approving_a_newer_version_does_not_fire_for_the_gap_the_swap_creates",
    },
    Control {
        label: "approving a newer version drops what the old version owed",
        edits: &[(
            SCHED_TABLE,
            "if old.state == ScheduleState::Active {",
            "if false && old.state == ScheduleState::Active {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "approving_a_newer_version_first_fires_what_the_old_version_owed",
    },
    Control {
        label: "a disabled schedule ticks",
        edits: &[
            (
                SCHED_TICK,
                "if row.state != ScheduleState::Active {",
                "if false && row.state != ScheduleState::Active {",
            ),
            (
                SCHED_TICK,
                "WHERE state = 'active' AND next_due_ms IS NOT NULL",
                "WHERE next_due_ms IS NOT NULL",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_disabled_schedule_does_not_tick_and_does_not_catch_up_when_enabled",
    },
    Control {
        label: "enabling resumes from the stale next due time and catches up the disabled hours",
        edits: &[(
            SCHED_TABLE,
            "            next_due,\n            to_ms(now)\n        ],\n    )?;\n    Ok(true)",
            "            row.next_due.map(to_ms),\n            to_ms(now)\n        ],\n    )?;\n    Ok(true)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "a_disabled_schedule_does_not_tick_and_does_not_catch_up_when_enabled",
    },
    Control {
        label: "removing a schedule keeps its planned fires",
        edits: &[(
            SCHED_TABLE,
            "    tx.execute(\"DELETE FROM schedule_fires WHERE flow_id = ?1\", [flow_id])?;\n    let removed",
            "    let removed",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "removing_a_schedule_stops_it_and_drops_its_planned_fires",
    },
    Control {
        label: "a fire refused by a draining admission is dropped instead of kept",
        edits: &[(
            SCHED_TICK,
            "if admission != Admission::Draining {",
            "if admission != Admission::Draining || true {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "fires_planned_while_admission_drains_wait_and_are_admitted_after",
    },
    Control {
        label: "unknown spec fields accepted",
        edits: &[(
            SCHED_SPEC,
            "#[serde(deny_unknown_fields)]\npub struct ScheduleSpec",
            "pub struct ScheduleSpec",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_unknown_fields_and_values",
    },
    Control {
        label: "each cap not bounded above",
        edits: &[(
            SCHED_SPEC,
            "if cap == 0 || cap > MAX_EACH_CAP {",
            "if cap == 0 {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_a_missed_cap_above_the_limit",
    },
    Control {
        label: "interval range not checked",
        edits: &[(
            SCHED_SPEC,
            "if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&secs) {",
            "if secs == u64::MAX {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_zero_and_out_of_range_intervals",
    },
    Control {
        label: "an unknown zone falls back to UTC",
        edits: &[(
            SCHED_SPEC,
            "tz: zone(zone_name)?,",
            "tz: zone(zone_name).unwrap_or(TimeZone::UTC),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_unknown_zones",
    },
    Control {
        label: "zone names matched case-insensitively",
        edits: &[(
            SCHED_SPEC,
            "if tz.iana_name() != Some(name) {",
            "if tz.iana_name().is_none() {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_unknown_zones",
    },
    Control {
        label: "seconds fields accepted in cron patterns",
        edits: &[(
            SCHED_SPEC,
            ".seconds(Seconds::Disallowed)",
            ".seconds(Seconds::Optional)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_malformed_cron",
    },
    Control {
        label: "a cron pattern that never matches is accepted",
        edits: &[(
            SCHED_SPEC,
            "if parsed.find_next_occurrence(&probe, true).is_err() {",
            "if parsed.find_next_occurrence(&probe, true).is_err() && false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_spec"),
        test: "validation_refuses_a_cron_that_never_fires",
    },
    Control {
        label: "approving a version with a schedule trigger does not create its schedule",
        edits: &[(
            INSTALL,
            "match manifest.trigger.schedule {",
            "match None::<crate::schedule::ScheduleSpec> {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_flows"),
        test: "a_scheduled_flow_runs_from_install_through_tick_admission_and_run",
    },
    Control {
        label: "a version swap does not plan what the old version owed (installed flows)",
        edits: &[(
            SCHED_TABLE,
            "if old.state == ScheduleState::Active {",
            "if false && old.state == ScheduleState::Active {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_flows"),
        test: "owed_fires_after_a_version_swap_run_the_version_approved_at_admission",
    },
    Control {
        label: "disabling a flow leaves its schedule ticking",
        edits: &[(
            INSTALL,
            "    schedule::table::disable(tx, flow_id, timestamp(now_ms)?)?;",
            "    let _ = timestamp(now_ms)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_flows"),
        test: "disabling_and_enabling_a_flow_stops_and_restarts_its_schedule",
    },
    Control {
        label: "enabling a flow leaves its schedule stopped",
        edits: &[(
            INSTALL,
            "        schedule::table::enable(tx, flow_id, timestamp(now_ms)?)?;",
            "        let _ = timestamp(now_ms)?;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_flows"),
        test: "disabling_and_enabling_a_flow_stops_and_restarts_its_schedule",
    },
    Control {
        label: "approving a version without a schedule trigger keeps the old schedule",
        edits: &[(
            INSTALL,
            "            schedule::table::remove(tx, flow_id)?;\n            Ok(None)",
            "            Ok(None)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_tick"),
        test: "removing_a_schedule_stops_it_and_drops_its_planned_fires",
    },
    Control {
        label: "a fire refused by the rate limit is dropped without a record",
        edits: &[(
            SCHED_TICK,
            "if let Some(reason) = dropped_reason(&admission) {",
            "if let Some(reason) = dropped_reason(&admission).filter(|_| false) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("schedule_flows"),
        test: "a_rate_limited_scheduled_fire_is_dropped_and_recorded",
    },
];

const BROCA: &str = "crates/basal-host/src/broca/mod.rs";
const BROCA_CONTROLS: &[Control] = &[
    Control {
        label: "Broca absent usage becomes zero",
        edits: &[(
            BROCA,
            "input_tokens: u.input_tokens,",
            "input_tokens: u.input_tokens.or(Some(0)),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "usage_absent_empty_and_zero_remain_distinct",
    },
    Control {
        label: "Broca fake accepts send id reuse",
        edits: &[(
            "crates/basal-host/src/broca/fake.rs",
            "if old.bytes != params || &old.route != route {",
            "if false && (old.bytes != params || &old.route != route) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "fake_refuses_send_id_reuse_and_restarts_with_same_run",
    },
    Control {
        label: "Broca call sessions lose the position",
        edits: &[(
            TOKENS,
            "basal:flow-{flow_id}:{run_id}:{position}",
            "basal:flow-{flow_id}:{run_id}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "broca_usage_settles_reservation_once_and_classify_returns_a_string",
    },
    Control {
        label: "Broca value can masquerade as usage metadata",
        edits: &[(
            TOKENS,
            "let report = match outcome.usage {",
            "let report = match outcome.usage.or_else(|| value.get(\"usage\").cloned().and_then(|u| serde_json::from_value(u).ok())) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "script_value_cannot_supply_usage_and_absent_ledger_fields_are_nullable",
    },
    Control {
        label: "Broca tools are not explicitly empty",
        edits: &[(
            BROCA,
            "tools: vec![],",
            "tools: vec![json!({\"name\":\"forbidden\"})],",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "exact_send_contract_and_parallel_sessions",
    },
    Control {
        label: "Broca output clamp is ignored",
        edits: &[(
            BROCA,
            "generation.max_output_tokens = Some(e.max_output);",
            "generation.max_output_tokens = Some(999);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "exact_send_contract_and_parallel_sessions",
    },
    Control {
        label: "Broca reissue changes bytes",
        edits: &[(
            BROCA,
            "self.transport.send(&call.route, &call.params)",
            "self.transport.send(&call.route, &[call.params.clone(), if call.handle.is_some() { vec![b' '] } else { vec![] }].concat())",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "reissue_after_cut_is_byte_identical_and_finished_is_immediate",
    },
    Control {
        label: "Broca pending handle is replaced",
        edits: &[(
            BROCA,
            "if call.handle.is_none() {\n            call.handle = Some(handle);\n        }",
            "call.handle = Some(handle); call.handle = call.broca_run_id.clone().or_else(|| Some(\"wrong\".into()));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "pending_submission_keeps_its_handle_and_usage_is_metadata",
    },
    Control {
        label: "Broca completed text is not the run.result final message",
        edits: &[(
            BROCA,
            "let text = &message.text;",
            "let text = &message.mid;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "every_run_result_state_maps_to_its_outcome_and_nonterminals_wait",
    },
    Control {
        label: "Broca completed run with empty text is treated as missing its message",
        edits: &[(
            BROCA,
            "let Some(message) = &result.final_message else {",
            "let Some(message) = result.final_message.as_ref().filter(|m| !m.text.is_empty()) else {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "a_completed_run_without_text_fulfils_with_empty_text",
    },
    Control {
        label: "Broca error rejection drops the error class",
        edits: &[(
            BROCA,
            "json!({\"code\": \"error\", \"class\": error.class, \"message\": error.message})",
            "json!({\"code\": \"error\", \"message\": error.message})",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "every_run_result_state_maps_to_its_outcome_and_nonterminals_wait",
    },
    Control {
        label: "Broca cancelled, interrupted and max_steps lose their state code",
        edits: &[(
            BROCA,
            "other => rejection(other, \"Broca run did not complete\", usage),",
            "_ => rejection(\"error\", \"Broca run did not complete\", usage),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "every_run_result_state_maps_to_its_outcome_and_nonterminals_wait",
    },
    Control {
        label: "Broca active and paused runs are given an outcome",
        edits: &[(
            BROCA,
            "if !TERMINAL_STATES.contains(&state) {",
            "if false && !TERMINAL_STATES.contains(&state) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "every_run_result_state_maps_to_its_outcome_and_nonterminals_wait",
    },
    Control {
        label: "Broca unknown_run is not recognised as an anomaly",
        edits: &[(
            BROCA,
            "Err(BrocaError::Refused { code, detail }) if code == UNKNOWN_RUN => {",
            "Err(BrocaError::Refused { code, detail }) if false && code == UNKNOWN_RUN => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "unknown_run_for_an_accepted_call_is_reported_as_unknown_not_rejected",
    },
    Control {
        label: "Broca unknown_run report omits the Broca run id",
        edits: &[(
            BROCA,
            "\"Broca answered unknown_run for run {run_id}, which",
            "\"Broca answered unknown_run for a run, which",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "unknown_run_for_an_accepted_call_is_reported_as_unknown_not_rejected",
    },
    Control {
        label: "Broca unknown run leaves the basal run suspended",
        edits: &[(
            JOURNAL,
            "\"cancelled\" => return Ok(Some(CompletionAck::Refused)),\n        _ => {}",
            "_ => return Ok(Some(CompletionAck::Accepted)),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "an_unknown_broca_run_moves_the_run_to_needs_reconcile_naming_it",
    },
    Control {
        label: "Broca poll does not read run.result for pending calls",
        edits: &[(
            BROCA,
            "if call.outcome.is_none() {\n                self.resolve(call)?;\n            }",
            "",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "a_missed_run_finished_is_recovered_on_reconnect_even_when_archived",
    },
    Control {
        label: "Broca unwatchable session blocks reading a finished run",
        edits: &[(
            BROCA,
            "let watched = self.transport.watch(&call.route);",
            "self.transport.watch(&call.route)?; let watched: Result<(), BrocaError> = Ok(());",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "a_missed_run_finished_is_recovered_on_reconnect_even_when_archived",
    },
    Control {
        label: "Broca pending call is not watched",
        edits: &[(
            BROCA,
            "let watched = self.transport.watch(&call.route);",
            "let watched: Result<(), BrocaError> = Ok(());",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "a_finished_run_wakes_through_the_watch_opened_at_the_live_head",
    },
    Control {
        label: "Broca outcome settles before run.status reports usage",
        edits: &[(
            BROCA,
            "RunStatusResponse::Active | RunStatusResponse::Paused { .. } => {",
            "RunStatusResponse::Active | RunStatusResponse::Paused { .. } if false => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "usage_comes_from_run_status_once_and_waits_while_status_lags",
    },
    Control {
        label: "Broca outcome waits forever while run.status says active or paused after run.result ended",
        edits: &[(
            BROCA,
            "if lagged < STATUS_LAG_POLLS {",
            "if true || lagged < STATUS_LAG_POLLS {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "a_status_that_never_catches_up_charges_the_reservation_after_a_bounded_wait",
    },
    Control {
        label: "Broca saved outcome is read from Broca again on redelivery",
        edits: &[
            (
                BROCA,
                "if call.outcome.is_none() && call.unknown.is_none() {\n            if call.handle",
                "if call.unknown.is_none() {\n            if call.handle",
            ),
            (
                BROCA,
                "if call.outcome.is_none() {\n                self.resolve(call)?;",
                "{\n                self.resolve(call)?;",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "usage_settles_once_when_an_outcome_is_redelivered_after_a_restart",
    },
    Control {
        label: "Broca acknowledged completion is delivered again",
        edits: &[(
            BROCA,
            "if call.acknowledged {",
            "if false && call.acknowledged {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "a_module_restart_with_calls_in_flight_delivers_each_exactly_once",
    },
    Control {
        label: "Broca usage metadata is ignored by the ledger",
        edits: &[(
            TOKENS,
            "Some(usage) => Report::Metadata(usage),",
            "Some(_usage) => Report::NoEffect,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "broca_usage_settles_reservation_once_and_classify_returns_a_string",
    },
    Control {
        label: "Broca classify accepts outside labels",
        edits: &[(
            BROCA,
            "if labels.contains(text) {",
            "if true || labels.contains(text) {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "classify_contract_is_exact_not_trimmed_or_parsed",
    },
    Control {
        label: "Broca completion is not delivered after module restart",
        edits: &[(
            BROCA,
            "if let Some(outcome) = &call.outcome {\n            sink.complete",
            "if let Some(outcome) = call.outcome.as_ref().filter(|_| false) {\n            sink.complete",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("e2e"),
        test: "a_broca_llm_suspends_survives_module_kill_and_settles_after_restart",
    },
    Control {
        label: "unknown reasons: the host's reported reason is not recorded",
        edits: &[(
            JOURNAL,
            "        params![run_id, p, reason.as_str()],",
            "        params![run_id, p, { let _ = reason; \"connection_lost\" }],",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "an_unknown_broca_run_moves_the_run_to_needs_reconcile_naming_it",
    },
    Control {
        label: "unknown reasons: Broca's unknown_run is reported as a lost connection",
        edits: &[(
            BROCA,
            "reason: UnknownReason::ProviderLostRun,",
            "reason: UnknownReason::ConnectionLost,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("broca_reservations"),
        test: "an_unknown_broca_run_moves_the_run_to_needs_reconcile_naming_it",
    },
];

const TEST_TIMEOUT: Duration = Duration::from_secs(600);

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("git {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn marker(path: &str, label: &str) -> String {
    if path.ends_with(".toml") {
        format!("\n# NON-VACUITY BREAK: {label}\n")
    } else {
        format!("\n// NON-VACUITY BREAK: {label}\n")
    }
}

/// Runs a command with a timeout, returning (exit success, output, timed out).
fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
) -> Result<(bool, String, bool), String> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let out_thread = thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err_thread = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + timeout;
    let (success, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.success(), false),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, true);
            }
            Err(e) => return Err(format!("wait: {e}")),
        }
    };
    let output = format!(
        "{}{}",
        out_thread.join().unwrap_or_default(),
        err_thread.join().unwrap_or_default()
    );
    Ok((success, output, timed_out))
}

fn excerpt(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .filter(|l| {
            l.contains("FAILED")
                || l.contains("test result:")
                || l.contains("panicked at")
                || l.contains("left:")
                || l.contains("right:")
                || l.contains("error[")
        })
        .collect();
    let mut text = lines.join("\n");
    if text.len() > 400 {
        let mut end = 400;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn run_control(root: &Path, control: &Control) -> Result<Value, String> {
    let mut touched: Vec<&str> = Vec::new();
    for (path, _, _) in control.edits {
        if !touched.contains(path) {
            touched.push(path);
        }
    }
    let mut restore = touched.clone();
    restore.extend_from_slice(control.also_restore);

    for path in &touched {
        let full = root.join(path);
        let mut text = std::fs::read_to_string(&full).map_err(|e| format!("{path}: {e}"))?;
        for (edit_path, old, new) in control.edits.iter().filter(|(p, _, _)| p == path) {
            let count = text.matches(old).count();
            if count != 1 {
                return Err(format!(
                    "{}: expected the edit text exactly once in {edit_path}, found {count}",
                    control.label
                ));
            }
            text = text.replacen(old, new, 1);
        }
        text.push_str(&marker(path, control.label));
        std::fs::write(&full, text).map_err(|e| format!("{path}: {e}"))?;
    }
    let during = git(root, &["diff", "--stat"])?;

    let mut command = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    command
        .current_dir(root)
        .env_remove("BASAL_WORKER_BIN")
        .env_remove("BASAL_CUT_EXHAUSTIVE")
        .arg("test");
    match control.target {
        Target::Integration(file) => command.args(["-p", "basal-worker", "--test", file]),
        Target::Lib => command.args(["-p", "basal-worker", "--lib"]),
        Target::Testkit(file) => command.args(["-p", "basal-testkit", "--test", file]),
        Target::Module(file) => command.args(["-p", "basal-module", "--test", file]),
        Target::ModuleLib => command.args(["-p", "basal-module", "--lib"]),
        Target::Host(file) => command.args(["-p", "basal-host", "--test", file]),
        Target::HostLib => command.args(["-p", "basal-host", "--lib"]),
    };
    command.args([control.test, "--", "--exact"]);
    let ran = run_with_timeout(command, TEST_TIMEOUT);

    // Restore the edited files (and any cargo rewrote) from the git index,
    // whether or not the test run succeeded.
    let mut checkout = vec!["checkout", "--"];
    checkout.extend(restore.iter().copied());
    git(root, &checkout)?;
    for path in &restore {
        let full = root.join(path);
        let _ = Command::new("touch").arg(&full).status();
    }
    let after = git(root, &["diff", "--stat"])?;
    if !after.is_empty() {
        return Err(format!(
            "{}: tree not clean after restore:\n{after}",
            control.label
        ));
    }

    let (success, output, timed_out) = ran?;
    let named_red = output.contains(&format!("test {} ... FAILED", control.test))
        && output.contains("1 failed;");
    let outcome = if timed_out {
        "hung"
    } else if output.contains("could not compile") {
        "compile_error"
    } else if named_red && !success {
        "reddened"
    } else if output.contains("0 passed; 0 failed") || output.contains("running 0 tests") {
        "not_reached"
    } else {
        "undefended"
    };
    Ok(json!({
        "control": control.label,
        "expected_red": control.test,
        "captured_output": excerpt(&output),
        "applied_evidence": format!(
            "{}; during: {}; after restore: empty git diff --stat",
            touched.join(", "),
            during.lines().last().unwrap_or("")
        ),
        "outcome": outcome,
    }))
}

const KNOWN_SETS: &[&str] = &[
    "--worker",
    "--journal",
    "--dispatch",
    "--schedule",
    "--module",
    "--broca",
    "--hosts",
];
fn parse_options(args: Vec<String>) -> Result<(String, bool, Option<String>), String> {
    let mut suite = String::new();
    let mut check = false;
    let mut filter = None;
    for arg in args {
        if arg == "--check" {
            check = true;
        } else if KNOWN_SETS.contains(&arg.as_str()) {
            if !suite.is_empty() {
                return Err("choose exactly one control set".into());
            }
            suite = arg;
        } else if arg.starts_with("--") {
            return Err(format!("unknown control flag: {arg}"));
        } else if filter.replace(arg).is_some() {
            return Err("choose at most one label filter".into());
        }
    }
    Ok((suite, check, filter))
}

fn main() -> ExitCode {
    // `--journal` selects the journal and runtime controls, `--dispatch`
    // the manifest, authorization and dispatch-ledger controls, and
    // `--schedule` the scheduler's (all in basal-core, driven through
    // basal-testkit's tests); the default is the worker's.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (suite, check, filter) = match parse_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error}\nKnown sets: {}", KNOWN_SETS.join(", "));
            return ExitCode::from(2);
        }
    };
    let (controls, evidence_file) = match suite.as_str() {
        "--journal" => (JOURNAL_CONTROLS, "docs/findings/slice-2-mutations.json"),
        "--dispatch" => (DISPATCH_CONTROLS, "docs/findings/slice-3-mutations.json"),
        "--schedule" => (SCHEDULE_CONTROLS, "docs/findings/slice-4-mutations.json"),
        "--module" => (MODULE_CONTROLS, "docs/findings/slice-5-mutations.json"),
        "--hosts" => (HOST_CONTROLS, "docs/findings/i1a-host-mutations.json"),
        "--broca" => (BROCA_CONTROLS, "docs/findings/i1a-broca-mutations.json"),
        _ => (CONTROLS, "docs/findings/slice-1-mutations.json"),
    };
    // `--check` only verifies that every edit's text occurs exactly once,
    // without touching anything.
    if check {
        let mut ok = true;
        for control in controls {
            for (path, old, _) in control.edits {
                let count = std::fs::read_to_string(path)
                    .map(|t| t.matches(old).count())
                    .unwrap_or(0);
                if count != 1 {
                    ok = false;
                    eprintln!("{}: {path}: found {count} times", control.label);
                }
            }
        }
        return if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }
    let root = match git(Path::new("."), &["rev-parse", "--show-toplevel"]) {
        Ok(r) => PathBuf::from(r),
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    // Stage every file a control may touch, so restoring from the index
    // brings back the current source rather than an older commit.
    let mut files: Vec<&str> = controls
        .iter()
        .flat_map(|c| {
            c.edits
                .iter()
                .map(|(p, _, _)| *p)
                .chain(c.also_restore.iter().copied())
        })
        .collect();
    files.sort();
    files.dedup();
    let mut add = vec!["add", "--"];
    add.extend(files.iter().copied());
    if let Err(e) = git(&root, &add) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }
    match git(&root, &["diff", "--stat"]) {
        Ok(s) if s.is_empty() => {}
        Ok(s) => {
            eprintln!("unstaged changes must be staged or removed first:\n{s}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }

    let mut evidence = Vec::new();
    let mut all_red = true;
    for control in controls {
        if let Some(f) = &filter
            && !control.label.contains(f.as_str())
            && !control.test.contains(f.as_str())
        {
            continue;
        }
        eprintln!("control: {}", control.label);
        match run_control(&root, control) {
            Ok(entry) => {
                eprintln!("  -> {}", entry["outcome"]);
                all_red &= entry["outcome"] == "reddened";
                evidence.push(entry);
            }
            Err(e) => {
                eprintln!("  -> runner error: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let text = serde_json::to_string_pretty(&evidence).unwrap_or_default() + "\n";
    if filter.is_none() {
        let path = root.join(evidence_file);
        if let Err(e) = std::fs::create_dir_all(root.join("docs/findings"))
            .and_then(|_| std::fs::write(&path, &text))
        {
            eprintln!("writing {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    }
    println!("{text}");
    if all_red {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

const HOST_CONTROLS: &[Control] = &[
    Control {
        label: "selector: unconfigured selector chooses a default",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "fn select(&self, _: &SelectionRequest) -> Result<ModelSelection, SelectionError> {",
            "fn select(&self, request: &SelectionRequest) -> Result<ModelSelection, SelectionError> { return FakeSelector::default().select(request);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "an_unconfigured_selector_has_no_default_model",
    },
    Control {
        label: "selector: no_available_model code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"no_available_model\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: quota_or_cooldown_exhausted code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"quota_or_cooldown_exhausted\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: all_routes_inadequate code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"all_routes_inadequate\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: no_adequate_route code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"no_adequate_route\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: missing_required_capability code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"missing_required_capability\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: context_too_small code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"context_too_small\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: invalid_requirements code lost",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "            code,\n            detail: message,",
            "            code: if code == \"invalid_requirements\" {\"wrong\".into()} else {code},\n            detail: message,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: missing runner falls back to selected ids",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "let raw = reply.get(\"runner\").ok_or_else(|| SelectionError::Refused {\n        code: \"route_runner_missing\".into(),\n        detail: \"routing did not supply a Broca runner\".into(),\n    })?;",
            "let fallback=json!({\"provider\":reply[\"selected\"][\"model\"][\"providerID\"],\"model\":reply[\"selected\"][\"model\"][\"modelID\"]}); let raw=reply.get(\"runner\").unwrap_or(&fallback);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_selection_reply_shapes_require_runner_and_preserve_both_identities",
    },
    Control {
        label: "selector: selection checkpoint moved after intent",
        edits: &[
            (
                "crates/basal-core/src/driver.rs",
                "        if matches!(\n            call.kind,\n            CallKind::Primitive(Primitive::Llm | Primitive::Classify)\n        ) {\n            self.at(Boundary::ModelSelected {\n                position: call.position,\n            })?;\n        }\n        let new = NewCall {",
                "        let new = NewCall {",
            ),
            (
                "crates/basal-core/src/driver.rs",
                "                self.at(Boundary::CallCommitted {\n                    position: call.position,\n                })?;",
                "                self.at(Boundary::CallCommitted {\n                    position: call.position,\n                })?; if matches!(call.kind, CallKind::Primitive(Primitive::Llm | Primitive::Classify)) { self.at(Boundary::ModelSelected {position:call.position})?; }",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selections_freeze_at_intent_commit_and_only_uncommitted_calls_reselect",
    },
    Control {
        label: "integration: unknown flags become filters",
        edits: &[(
            "crates/basal-testkit/src/bin/mutation-controls.rs",
            "arg.starts_with(\"--\")",
            "false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("mutation_cli"),
        test: "unknown_control_flags_refuse_without_running_any_set",
    },
    Control {
        label: "integration: model store initialization omitted",
        edits: &[(
            "crates/basal-module/src/module.rs",
            "initialize(store.clone())?;",
            "let _ = initialize;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_host_model_selection_and_digest_sink_share_the_journal_store",
    },
    Control {
        label: "integration: accepted model subscription wake omitted",
        edits: &[(
            "crates/basal-host/src/routing.rs",
            "self.target(&request.kind).dispatch_committed(request);",
            "let _ = request;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_host_model_selection_and_digest_sink_share_the_journal_store",
    },
    Control {
        label: "integration: stream wake omitted",
        edits: &[(
            "crates/basal-host/src/broca/subc.rs",
            "self.closed.store(true, Ordering::SeqCst);\n        (wake)();",
            "self.closed.store(true, Ordering::SeqCst);\n        let _ = wake;",
        )],
        also_restore: NO_EXTRA,
        target: Target::HostLib,
        test: "broca::subc::tests::only_a_finished_run_wakes_the_host_and_closes_the_stream",
    },
    Control {
        label: "integration: reconnect wake omitted",
        edits: &[(
            "crates/basal-host/src/broca/subc.rs",
            "lock(streams).clear();\n    (wake)();",
            "lock(streams).clear();\n    let _ = wake;",
        )],
        also_restore: NO_EXTRA,
        target: Target::HostLib,
        test: "broca::subc::tests::stream_and_reconnect_wakeups_are_coalesced_without_a_clock",
    },
    Control {
        label: "integration: every control event ends the stream",
        edits: &[(
            "crates/basal-host/src/broca/subc.rs",
            "matches!(*unit, ControlUnit::RunFinished { .. })",
            "{ let _ = unit; true }",
        )],
        also_restore: NO_EXTRA,
        target: Target::HostLib,
        test: "broca::subc::tests::only_a_finished_run_wakes_the_host_and_closes_the_stream",
    },
    Control {
        label: "integration: frozen management parameters reserialized",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "body.extend(params);",
            "body.extend(serde_json::to_vec(&value).map_err(|e|WireError::NeverSent(e.to_string()))?);",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "management_envelopes_preserve_frozen_parameter_bytes",
    },
    Control {
        label: "integration: omitted digest sends silent",
        edits: &[(
            "crates/basal-core/src/driver.rs",
            "serde_json::to_value(cap)",
            "serde_json::to_value(\"silent\")",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "omitted_digest_action_uses_each_recipient_approved_cap",
    },
    Control {
        label: "integration: dry run defaults digest to wake",
        edits: &[(
            "crates/basal-module/src/dryrun.rs",
            "None => manifest.digest_cap(agent),",
            "None => Some(DigestAction::Wake),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("dry_run"),
        test: "omitted_digest_action_in_dry_runs_is_the_recipient_digest_max",
    },
    Control {
        label: "selector: iq becomes optional",
        edits: &[(
            "crates/basal-core/src/manifest.rs",
            "pub iq: u32,",
            "#[serde(default)]\n    pub iq: u32,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "llm_demands_are_required_bounded_and_default_eq_is_zero",
    },
    Control {
        label: "selector: iq upper bound omitted",
        edits: &[(
            "crates/basal-core/src/manifest.rs",
            "if llm.iq > 100 {",
            "if false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "llm_demands_are_required_bounded_and_default_eq_is_zero",
    },
    Control {
        label: "selector: eq upper bound omitted",
        edits: &[(
            "crates/basal-core/src/manifest.rs",
            "if llm.eq > 100 {",
            "if false {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Testkit("dispatch_manifest"),
        test: "llm_demands_are_required_bounded_and_default_eq_is_zero",
    },
    Control {
        label: "selector: script model guard omitted before routing",
        edits: &[(
            "crates/basal-core/src/driver.rs",
            "(args.get(\"model\").is_some() || args.get(\"provider\").is_some())",
            "false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "script_model_or_provider_is_rejected_before_selection_even_if_it_matches",
    },
    Control {
        label: "selector: broker script model guard omitted",
        edits: &[(
            "crates/basal-host/src/broca/mod.rs",
            "e.request.get(\"model\").is_some() || e.request.get(\"provider\").is_some()",
            "false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "broker_rejects_script_model_choices_even_with_a_valid_journaled_selection",
    },
    Control {
        label: "selector: flow target changed",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "\"targetAgent\":\"flow\"",
            "\"targetAgent\":\"operator\"",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_select_request_bytes_are_exact_without_available_models",
    },
    Control {
        label: "selector: demands swapped",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "\"iq\":request.iq,\"eq\":request.eq",
            "\"iq\":request.eq,\"eq\":request.iq",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_select_request_bytes_are_exact_without_available_models",
    },
    Control {
        label: "selector: availableModels added",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "\"substrate\":\"broca\"",
            "\"substrate\":\"broca\",\"availableModels\":[]",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_select_request_bytes_are_exact_without_available_models",
    },
    Control {
        label: "selector: runner mapped from registry identities",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "pub fn decode_selection(reply: Value) -> Result<ModelSelection, SelectionError> {",
            "pub fn decode_selection(mut reply: Value) -> Result<ModelSelection, SelectionError> { reply[\"runner\"] = json!({\"provider\":reply[\"selected\"][\"model\"][\"providerID\"],\"model\":reply[\"selected\"][\"model\"][\"modelID\"]});",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_selection_reply_shapes_require_runner_and_preserve_both_identities",
    },
    Control {
        label: "selector: registry variant not journaled",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "        variant,\n        decision_id:",
            "        variant: None,\n        decision_id:",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_selection_reply_shapes_require_runner_and_preserve_both_identities",
    },
    Control {
        label: "selector: unreachable retry bound omitted",
        edits: &[(
            "crates/basal-core/src/driver.rs",
            "retries >= self.rt.config.unavailable_retries",
            "retries >= u32::MAX",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "unreachable_selector_retries_boundedly_then_journals_route_unavailable",
    },
    Control {
        label: "selector: provider refusal code lost in journal",
        edits: &[(
            "crates/basal-core/src/driver.rs",
            "return Err(Refusal::new(code, detail));",
            "return Err(Refusal::new(\"route_unavailable\", detail));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selector_refusals_are_journaled_with_each_code_and_never_send_broca",
    },
    Control {
        label: "selector: reissue changes selected runner",
        edits: &[(
            "crates/basal-core/src/runtime.rs",
            "args: row.dispatch_args(),",
            "args: { let mut v: serde_json::Value = serde_json::from_str(row.dispatch_args().as_str()).map_err(|_| ()).unwrap_or(serde_json::Value::Null); v[\"selection\"][\"runner\"][\"model\"] = serde_json::json!(\"wrong\"); JsonText::new(v.to_string()).unwrap_or_else(|_| JsonText::null()) },",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "selections_freeze_at_intent_commit_and_only_uncommitted_calls_reselect",
    },
    Control {
        label: "selector: outcome report omitted",
        edits: &[(
            "crates/basal-host/src/broca/mod.rs",
            ".report_outcome(&call.selection.decision_id, outcome)",
            ".report_outcome(\"wrong-decision\", outcome)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "model_outcomes_report_the_selected_decision_best_effort",
    },
    Control {
        label: "selector: outcome failure blocks completion",
        edits: &[(
            "crates/basal-host/src/broca/mod.rs",
            "tracing::warn!(%error,\"model routing outcome report failed\");",
            "return Err(BrocaError::Wire(error.to_string()));",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "model_outcomes_report_the_selected_decision_best_effort",
    },
    Control {
        label: "selector: cancelled outcome marked completed",
        edits: &[(
            "crates/basal-host/src/broca/mod.rs",
            "\"cancelled\" => ModelOutcome::Cancelled",
            "\"cancelled\" => ModelOutcome::Completed",
        )],
        also_restore: NO_EXTRA,
        target: Target::Host("broca"),
        test: "model_outcomes_report_the_selected_decision_best_effort",
    },
    Control {
        label: "selector: decision outcome key changed",
        edits: &[(
            "crates/basal-host/src/selector.rs",
            "\"decisionID\":decision_id,\"outcome\":outcome",
            "\"decisionID\":\"wrong\",\"outcome\":outcome",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_select_request_bytes_are_exact_without_available_models",
    },
    Control {
        label: "hosts: facts request groups discarded",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "\"fields\":args[\"options\"].get(\"fields\").cloned().unwrap_or(json!([\"identity\",\"residence\",\"activity\",\"attention\"]))",
            "\"fields\":[]",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "core_requests_match_c3_c4_field_for_field",
    },
    Control {
        label: "hosts: facts identity validation bypassed",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "Primitive::Facts => {\n            value[\"agent_id\"].is_string()",
            "Primitive::Facts => {\n            true || value[\"agent_id\"].is_string()",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "fact_identity_and_clock_fields_are_required_without_normalising_leaves",
    },
    Control {
        label: "hosts: facts unknown activity normalised to idle",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "if valid {\n        Ok(value)",
            "if valid {\n        let mut value = value; if kind == Primitive::Facts { value[\"activity\"][\"state\"][\"value\"] = json!(\"idle\"); value[\"activity\"][\"state\"][\"status\"] = json!(\"ok\"); }\n        Ok(value)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "fact_identity_and_clock_fields_are_required_without_normalising_leaves",
    },
    Control {
        label: "hosts: journaled class not passed to transport host",
        edits: &[(
            "crates/basal-core/src/runtime.rs",
            ".dispatch_classified(&request, expected_class)",
            ".dispatch(&request)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "catalog_changes_cannot_execute_mutations_under_a_query_retry_policy",
    },
    Control {
        label: "hosts: routing loses journaled class",
        edits: &[(
            "crates/basal-host/src/routing.rs",
            ".dispatch_classified(request, class)",
            ".dispatch(request)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "catalog_changes_cannot_execute_mutations_under_a_query_retry_policy",
    },
    Control {
        label: "hosts: changed operation kind accepted",
        edits: &[(
            "crates/basal-host/src/routing.rs",
            "expected.is_some_and(|class| class != self.declared_class(module, op, &decl))",
            "false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "catalog_changes_cannot_execute_mutations_under_a_query_retry_policy",
    },
    Control {
        label: "hosts: consent provider refusal treated as outage",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            "ConsentError::Refused(format!(\"{code}: {message}\"))",
            "ConsentError::Unavailable(format!(\"{code}: {message}\"))",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "consent_refusal_codes_and_unknown_transport_are_typed",
    },
    Control {
        label: "hosts: expiry tombstone identity is not retrieved",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            "record.get(\"flow_install\").is_none()",
            "false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "expired_answer_tombstones_read_the_owned_record_before_ack",
    },
    Control {
        label: "hosts: keyed tool contract ignored",
        edits: &[(
            "crates/basal-host/src/routing.rs",
            "self.keyed.contains(&(module.to_owned(), op.to_owned()))",
            "false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "operator_keyed_tools_retry_but_unfenceable_tools_do_not",
    },
    Control {
        label: "hosts: status revision substituted",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "\"revision\":created_at",
            "\"revision\":0",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "core_requests_match_c3_c4_field_for_field",
    },
    Control {
        label: "hosts: status replay marker ignored",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "Primitive::SinkStatus => {\n            value[\"replayed\"].is_boolean()",
            "Primitive::SinkStatus => {\n            true",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "sink_reply_missing_fields_and_wrong_types_are_unknown_outcomes",
    },
    Control {
        label: "hosts: missing management op accepted",
        edits: &[(
            "crates/basal-host/src/subc_catalog.rs",
            "find(|o| o.name == op)",
            "find(|_| true)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "catalog_install_refuses_missing_ops_and_events_and_marks_mutations",
    },
    Control {
        label: "hosts: undeclared events fabricated",
        edits: &[(
            "crates/basal-host/src/subc_catalog.rs",
            "fn event(&self, _: &str, _: &str, _: u32) -> Option<EventDecl> {\n        None\n    }",
            "fn event(&self, _: &str, _: &str, _: u32) -> Option<EventDecl> { Some(EventDecl { origin: crate::EventOrigin::Internal, body: crate::EventBody::Inline }) }",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "catalog_install_refuses_missing_ops_and_events_and_marks_mutations",
    },
    Control {
        label: "hosts: registry admission bypassed",
        edits: &[(
            "crates/basal-host/src/subc_catalog.rs",
            "self.known_agent(agent).unwrap_or(false)",
            "true",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "denylist_and_paged_agent_registry_fail_closed",
    },
    Control {
        label: "hosts: shell denylist disabled",
        edits: &[(
            "crates/basal-host/src/subc_catalog.rs",
            ".any(|(m, o)| m.eq_ignore_ascii_case(module) && o.eq_ignore_ascii_case(op))",
            ".any(|_| false)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "denylist_and_paged_agent_registry_fail_closed",
    },
    Control {
        label: "hosts: declining answer approves",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            "(Some(\"answered\"), Some(\"decline\")) | (Some(\"expired\"), _) => CardDecision::Reject",
            "(Some(\"answered\"), Some(\"decline\")) | (Some(\"expired\"), _) => CardDecision::Approve",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "decline_and_expiry_are_rejections_and_unrecognised_answers_are_not_acked",
    },
    Control {
        label: "hosts: proven unsent becomes unknown",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "CallError::NotSent(e) => WireError::NeverSent(e.to_string())",
            "CallError::NotSent(e) => WireError::Unknown(e.to_string())",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "transport_certainty_and_tool_key_are_explicit",
    },
    Control {
        label: "hosts: unknown becomes provably unsent",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "other => WireError::Unknown(other.to_string())",
            "other => WireError::NeverSent(other.to_string())",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "transport_certainty_and_tool_key_are_explicit",
    },
    Control {
        label: "hosts: tool key omitted",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "request.call_key = Some(call_key.to_owned());",
            "request.call_key = None;",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "transport_certainty_and_tool_key_are_explicit",
    },
    Control {
        label: "hosts: provider refusal fulfilled",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "HostOutcome::rejected(v)",
            "HostOutcome::fulfilled(v)",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "every_core_refusal_retains_its_code",
    },
    Control {
        label: "hosts: closed sink decoder bypassed",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "if valid {",
            "if true {",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "closed_sink_dispositions_and_unknown_facts_survive",
    },
    Control {
        label: "hosts: sink idempotency disabled",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "honours_idempotency_keys: true",
            "honours_idempotency_keys: false",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "transport_failures_reach_journal_with_safe_retries_only",
    },
    Control {
        label: "hosts: management mutation marked query",
        edits: &[(
            "crates/basal-host/src/subc_catalog.rs",
            "ManagementOperationKind::Mutate => OpKind::Mutate",
            "ManagementOperationKind::Mutate => OpKind::Query",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "catalog_install_refuses_missing_ops_and_events_and_marks_mutations",
    },
    Control {
        label: "hosts: unfenceable tool becomes repeatable",
        edits: &[("crates/basal-host/src/routing.rs", "!unfenceable", "true")],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "operator_keyed_tools_retry_but_unfenceable_tools_do_not",
    },
    Control {
        label: "hosts: consent hash changed",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            "\"args_digest\":hash",
            "\"args_digest\":\"wrong\"",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "consent_hash_matches_shared_vectors_and_full_c2_envelope",
    },
    Control {
        label: "hosts: exact manifest bytes trimmed",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            "\"manifest_json\":manifest",
            "\"manifest_json\":manifest.trim()",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "install_card_is_byte_exact_and_scope_comes_from_the_route_stamp",
    },
    Control {
        label: "hosts: the consent request sends a session_ref with a scope author",
        edits: &[
            (
                "crates/basal-host/src/core_consent.rs",
                "let result = json!({\"kind\":\"flow_install\"",
                "let mut result = json!({\"kind\":\"flow_install\"",
            ),
            (
                "crates/basal-host/src/core_consent.rs",
                "    Ok(result)\n}",
                "    result[\"session_ref\"] = f.get(\"session_ref\").cloned().unwrap_or(json!(\"ses-from-the-bind\"));\n    Ok(result)\n}",
            ),
        ],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "install_card_is_byte_exact_and_scope_comes_from_the_route_stamp",
    },
    Control {
        label: "hosts: answer acknowledged despite failed decision commit",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            ".map_err(|e| ConsentError::Unavailable(e.to_string()))?;",
            ".ok();",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "decisions_ack_only_after_sink_commit_and_survive_restart",
    },
    Control {
        label: "hosts: approval decoded as rejection",
        edits: &[(
            "crates/basal-host/src/core_consent.rs",
            "(Some(\"answered\"), Some(\"approve\")) => CardDecision::Approve",
            "(Some(\"answered\"), Some(\"approve\")) => CardDecision::Reject",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "decisions_ack_only_after_sink_commit_and_survive_restart",
    },
    Control {
        label: "hosts: reissue ignores journaled envelope",
        edits: &[(
            "crates/basal-core/src/runtime.rs",
            "args: row.dispatch_args(),",
            "args: row.args.clone(),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "journaled_sink_intent_is_byte_identical_after_a_cut",
    },
    Control {
        label: "hosts: core routing bypassed",
        edits: &[(
            "crates/basal-host/src/routing.rs",
            ") => self.core.as_ref(),",
            ") => self.model.as_ref(),",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "routing_host_runs_install_decision_facts_ops_and_sinks_end_to_end",
    },
    Control {
        label: "unknown reasons: a timeout is not told from a closed connection",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "CallError::OutcomeUnknown(e) if e.to_string().contains(SUBC_REPLY_TIMEOUT) => {",
            "CallError::OutcomeUnknown(e) if false && e.to_string().contains(SUBC_REPLY_TIMEOUT) => {",
        )],
        also_restore: NO_EXTRA,
        target: Target::HostLib,
        test: "transport::tests::every_call_error_variant_maps_to_never_sent_or_one_unknown_reason",
    },
    Control {
        label: "unknown reasons: capability resolver errors count as maybe sent",
        edits: &[(
            "crates/basal-host/src/transport.rs",
            "| CallError::InvalidCapabilityIdentifier { .. }) => WireError::NeverSent(e.to_string()),",
            "| CallError::InvalidCapabilityIdentifier { .. }) => WireError::Unknown(e.to_string()),",
        )],
        also_restore: NO_EXTRA,
        target: Target::HostLib,
        test: "transport::tests::every_call_error_variant_maps_to_never_sent_or_one_unknown_reason",
    },
    Control {
        label: "unknown reasons: core's unrecognised reply is recorded as a lost connection",
        edits: &[(
            "crates/basal-host/src/core_host.rs",
            "Err(WireError::Unreadable(\"unrecognised core reply\".into()))",
            "Err(WireError::Unknown(\"unrecognised core reply\".into()))",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("hosts"),
        test: "closed_sink_dispositions_and_unknown_facts_survive",
    },
];
