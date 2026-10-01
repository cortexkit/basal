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
//! cargo run -p basal-testkit --bin mutation-controls [-- [--journal|--dispatch] [--check] [<label filter>]]
//! ```
//!
//! Without a flag it runs the worker engine's controls; with `--journal`,
//! the journal, run state machine and driver controls in basal-core; with
//! `--dispatch`, the manifest, authorization, audit, token, `kv`, slot,
//! deadline, rate-limit, disable and per-run limit controls in basal-core.
//! basal-core's tests live in basal-testkit. `--check` only verifies that every edit's
//! text occurs exactly once in the current source.
//!
//! Safety of the working tree: the runner stages every file it will touch so
//! the index holds the current source, refuses to start if anything is
//! unstaged, restores each edit from the index with `git checkout --`, and
//! checks the tree is clean again before the next control. It never stashes
//! and never restores from HEAD. Every temporary edit carries the marker
//! `NON-VACUITY BREAK` so a break left behind by a crash is easy to find.
//! Evidence is written to `docs/findings/slice-1-mutations.json` (worker)
//! `docs/findings/slice-2-mutations.json` (journal) or
//! `docs/findings/slice-3-mutations.json` (dispatch), beside the findings
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
    // `--journal` selects the journal and runtime controls (basal-core,
    // driven through basal-testkit's tests); the default is the worker's.
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `--dispatch` selects manifests, authorization and the dispatch
    // ledgers (basal-core too).
    let journal = args.first().is_some_and(|a| a == "--journal");
    let dispatch = args.first().is_some_and(|a| a == "--dispatch");
    if journal || dispatch {
        args.remove(0);
    }
    let (controls, evidence_file) = if journal {
        (JOURNAL_CONTROLS, "docs/findings/slice-2-mutations.json")
    } else if dispatch {
        (DISPATCH_CONTROLS, "docs/findings/slice-3-mutations.json")
    } else {
        (CONTROLS, "docs/findings/slice-1-mutations.json")
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
