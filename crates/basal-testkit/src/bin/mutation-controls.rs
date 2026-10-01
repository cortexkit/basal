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
//! cargo run -p basal-testkit --bin mutation-controls [-- [--journal | --dispatch | --schedule | --module] [--check] [<label filter>]]
//! ```
//!
//! Without a suite flag it runs the worker engine's controls; with
//! `--journal`, the journal, run state machine and driver controls in
//! basal-core; with `--dispatch`, the manifest, authorization, audit, token,
//! `kv`, slot, deadline, rate-limit, disable and per-run limit controls in
//! basal-core; with `--schedule`, the scheduler's controls in basal-core;
//! with `--module`, the module shell's (pool, engine, ops, dry run, consent,
//! manifest), whose tests live in basal-module.
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
//! `docs/findings/slice-5-mutations.json` (module), beside the findings
//! notes in `docs/findings/`.

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
const M_POOL: &str = "crates/basal-module/src/pool.rs";
const M_ENGINE: &str = "crates/basal-module/src/engine.rs";
const M_FATAL: &str = "crates/basal-module/src/fatal.rs";
const M_MODULE: &str = "crates/basal-module/src/module.rs";
const M_DRYRUN: &str = "crates/basal-module/src/dryrun.rs";
const M_OPS: &str = "crates/basal-module/src/ops.rs";
const M_UNCONFIGURED: &str = "crates/basal-module/src/unconfigured.rs";
const CORE_OPS: &str = "crates/basal-core/src/ops.rs";

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
            "(Caller::Agent(agent), Mode::Capture) if self.owns(agent, &p.flow_id)? => {}",
            "(Caller::Agent(agent), _) if self.owns(agent, &p.flow_id)? => {}",
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
            "(Caller::Agent(agent), Mode::Capture) if self.owns(agent, &p.flow_id)? => {}",
            "(Caller::Agent(_), Mode::Capture) => {}",
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
        label: "any agent may enable any flow",
        edits: &[(
            M_OPS,
            "Caller::Agent(agent) if self.owns(agent, &p.flow_id)? => {}",
            "Caller::Agent(_) => {}",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("ops"),
        test: "authorization_matrix",
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
            "proven_unsent: true,",
            "proven_unsent: false,",
        )],
        also_restore: NO_EXTRA,
        target: Target::Module("engine"),
        test: "the_unconfigured_host_refuses_every_dispatch_as_never_sent",
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

fn main() -> ExitCode {
    // `--journal` selects the journal and runtime controls, `--dispatch`
    // the manifest, authorization and dispatch-ledger controls, and
    // `--schedule` the scheduler's (all in basal-core, driven through
    // basal-testkit's tests); the default is the worker's.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let suite = match args.first().map(String::as_str) {
        Some(flag @ ("--journal" | "--dispatch" | "--schedule" | "--module")) => {
            let flag = flag.to_owned();
            args.remove(0);
            flag
        }
        _ => String::new(),
    };
    let (controls, evidence_file) = match suite.as_str() {
        "--journal" => (JOURNAL_CONTROLS, "docs/findings/slice-2-mutations.json"),
        "--dispatch" => (DISPATCH_CONTROLS, "docs/findings/slice-3-mutations.json"),
        "--schedule" => (SCHEDULE_CONTROLS, "docs/findings/slice-4-mutations.json"),
        "--module" => (MODULE_CONTROLS, "docs/findings/slice-5-mutations.json"),
        _ => (CONTROLS, "docs/findings/slice-1-mutations.json"),
    };
    // `--check` only verifies that every edit's text occurs exactly once,
    // without touching anything.
    let check = args.first().is_some_and(|a| a == "--check");
    if check {
        args.remove(0);
    }
    let filter = args.into_iter().next();
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
