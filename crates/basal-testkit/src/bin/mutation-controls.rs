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
//! cargo run -p basal-testkit --bin mutation-controls [-- [--journal | --schedule] [--check] [<label filter>]]
//! ```
//!
//! Without a suite flag it runs the worker engine's controls; with
//! `--journal`, the journal, run state machine and driver controls in
//! basal-core; with `--schedule`, the scheduler's controls in basal-core.
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
//! `docs/findings/slice-2-mutations.json` (journal) or
//! `docs/findings/slice-4-mutations.json` (scheduler), beside the findings
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
    // `--journal` selects the journal and runtime controls and `--schedule`
    // the scheduler's (both in basal-core, driven through basal-testkit's
    // tests); the default is the worker's.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let suite = match args.first().map(String::as_str) {
        Some(flag @ ("--journal" | "--schedule")) => {
            let flag = flag.to_owned();
            args.remove(0);
            flag
        }
        _ => String::new(),
    };
    let (controls, evidence_file) = match suite.as_str() {
        "--journal" => (JOURNAL_CONTROLS, "docs/findings/slice-2-mutations.json"),
        "--schedule" => (SCHEDULE_CONTROLS, "docs/findings/slice-4-mutations.json"),
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
