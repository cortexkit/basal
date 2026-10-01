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
//! cargo run -p basal-testkit --bin mutation-controls [-- <label filter>]
//! ```
//!
//! Safety of the working tree: the runner stages every file it will touch so
//! the index holds the current source, refuses to start if anything is
//! unstaged, restores each edit from the index with `git checkout --`, and
//! checks the tree is clean again before the next control. It never stashes
//! and never restores from HEAD. Every temporary edit carries the marker
//! `NON-VACUITY BREAK` so a break left behind by a crash is easy to find.
//! Evidence is written to `docs/findings/slice-1-mutations.json`, beside the
//! worker engine's findings note `docs/findings/slice-1-worker.md`.

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
        .args(["test", "-p", "basal-worker"]);
    match control.target {
        Target::Integration(file) => command.args(["--test", file]),
        Target::Lib => command.arg("--lib"),
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
    let filter = std::env::args().nth(1);
    let root = match git(Path::new("."), &["rev-parse", "--show-toplevel"]) {
        Ok(r) => PathBuf::from(r),
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    // Stage every file a control may touch, so restoring from the index
    // brings back the current source rather than an older commit.
    let mut files: Vec<&str> = CONTROLS
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
    for control in CONTROLS {
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
        let path = root.join("docs/findings/slice-1-mutations.json");
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
