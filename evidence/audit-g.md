# Tests, rig, scripts and mutation review

## Merge coordination and boundaries

The MSRV increase activates Clippy lints that were dormant at the declared
Rust 1.85 minimum. The parent approved these mechanical, behavior-preserving
edits outside this slice's original source ownership:

| Owner | File / original site | Mechanical change |
|---|---|---|
| C | `crates/basal-core/src/decisions.rs`, `window` | Use `is_multiple_of(1000)`. |
| B | `crates/basal-core/src/driver.rs`, SinkDigest default action | Collapse the nested condition into a let-chain. |
| E | `crates/basal-host/src/broca/subc.rs`, reconnect callback | Collapse the nested condition into a let-chain. |
| D | `crates/basal-host/src/routing.rs`, `classify` | Collapse the nested condition into a let-chain. |
| F | `crates/basal-module/src/harness.rs`, two Broca polls | Collapse the nested conditions into let-chains. |
| F | `crates/basal-module/src/ops.rs`, list visibility | Collapse the nested condition into a let-chain. |

Two existing nested conditions in the owned `journal_kill.rs` test also needed
the same mechanical fix. The list visibility change mechanically rebases
`flow-list-invisible-requested-id-appears`; the parent explicitly approved
keeping that overlap. **Slice F's SQL visibility implementation and mutation
anchor win at merge.** None of F's other protected rows were changed.

Two other small, authorized scope extensions are `.gitignore` (Python bytecode
for R10) and `docs/deploy.md` (the requested runner pin update). No lockfile was
changed; `cargo fetch --locked` was run after manifest changes.

The full external report was initially absent from the worktree. The parent
copied it into ignored task context; no parent-checkout files were read or
edited. There are no delivered changes to `https.rs`, `builtins_net.rs`,
`decisions.rs` prompt assertions, `packages.rs`, `unknown_control.rs`, or
`journal_admin.rs`. Proposed K18 changes were reverted when ownership of the
HTTPS fixtures was assigned to E.

## Finding dispositions

K5 belongs to B and is deliberately absent from this table.

| Finding | State | Evidence / disposition |
|---|---|---|
| K1 | FIXED | Both stalled-driver cleanup tests now drop their workers and independently probe `waitpid`/`ECHILD`, rather than asking an already-waited worker whether it exited. |
| K2 | FIXED | The pool observer's shutdown is checked before fallback shutdown, so the fallback no longer makes the reap assertion pass vacuously. |
| K3 | NOT A DEFECT | Current `flow_scopes.rs` already uses a 60-second healthy activation budget; its 3-second budget is confined to the deliberately stalled deadline fixture (`ACTIVATION_WAIT` / `STALLED_ACTIVATION_WAIT`). |
| K4 | FIXED | Healthy pool handshake/acquisition/activation waits use 60 seconds; the intentionally stalled observer retains its separately named 3-second deadline. |
| K6 | FIXED | `drive` passes its remaining budget to the inner wait; `ProcessSource::handshake_timeout` is independently configurable instead of a hard-coded 120 seconds. |
| K7 | FIXED | The e2e harness kills/reaps on Drop, drains reader threads, and bounds response/exit waits; `e2e_fixture_drop_reaps_its_child` failed before the fix and passed afterward. |
| K8 | FIXED | The kill parent uses a bounded, concurrently drained process-group launcher; the deadline test exercises descendants holding output pipes. |
| K9 | FIXED | Discovery follows Cargo's executable artifact, not a guessed directory/profile, and caches build errors outside a panicking initializer. The fake-Cargo regression failed before the fix for a relative target path. |
| K10 | FIXED | The three copied admission helpers use `tests/common::admit_trigger`. |
| K11 | FIXED | e2e/confinement use the counter-bearing scratch helper; creation errors are reported, and e2e, confinement and store-mode directories have unwinding cleanup guards. |
| K12 | FIXED | The shared healthy activation budget is now 60 seconds, agreeing with its headroom comment and production default. |
| K13 | FIXED | Unread `FrameCounts` fields, `spawned_at`, and the worker tests' unused `script_error` helper were removed. |
| K14 | NEEDS A DECISION | See the mock-boundary tradeoff below; the two hosts have different journal/persistence semantics. |
| K15 | FIXED | Worker samples are sorted, malformed arguments are refused before spawning, and journal `--samples` applies to all three measurement families. The two summary schemas remain intentionally distinct. |
| K16 | FIXED | Fuzz seed and JS budget are named constants used by both execution and the report. |
| K17 | FIXED | macOS RSS samples use `proc_pidinfo`, removing a `ps` fork/exec every 2 ms from the measurement loop. |
| K18 | NEEDS A DECISION | Per parent coordination, E owns the HTTPS helper and net tests; the proposed stronger observation contract is recorded below for merge. |
| K19 | FIXED | The duplicate standalone uncut test was removed; both cut suites still call `check_uncut`. Crash suites share `CUT_PARALLEL`. |
| K20 | FIXED | IPC asserts the exact `"number"` result of `typeof Date.now()`; journal race results no longer pass through a needless OnceLock. |
| K21 | NEEDS A DECISION | Replacing the source-order guard needs a runtime-observable startup boundary; see below. |
| K22 | FIXED | Used underscored names were corrected, and failure to admit the representative kill run produces an explicit error instead of run `""`. |
| R1 | FIXED | CI runs the feature suite; local execution names and passes `transport::tests::rig_send_hook_opens_only_the_selected_send_unscoped_and_keeps_other_calls_scoped`. |
| R2 | FIXED | Declared minimum is 1.88; a dedicated CI job checks all targets in both feature configurations. Both configurations were also checked locally on 1.88.0. |
| R3 | FIXED | `strings` and `nm` statuses are checked before searching their output; both failures reproduced in an offline test and have separately proved mutation rows. |
| R4 | FIXED | Required journal/receipt read errors append a failed case check instead of becoming successful absence evidence; the regression failed before the fix. |
| R5 | FIXED | Receipt matching uses the journaled agent and requested revision, including no-live-session outcomes, rather than any status receipt; the wrong-agent/revision regression failed before the fix. |
| R6 | FIXED | Physical-path resolution follows dangling final symlinks and link chains; the dangling-link regression reproduced the escape and its mutation control is caught. |
| R7 | FIXED | Config/test temporary-daemon ownership is scoped in subshells with exit/signal cleanup; the config failure regression reproduced the leaked daemon marker. |
| R8 | FIXED | The native bridge row's `test_file` names `engine/tests.rs`; its replay is CAUGHT. |
| R9 | FIXED | CI downloads the explicit ShellCheck 0.11.0 release, rather than whatever Homebrew currently serves. Local 0.11.0 checks all four shell scripts. |
| R10 | FIXED | `__pycache__/` is ignored, and CI disables bytecode writes. |
| R11 | FIXED | Production card generation consumes prepared validation and precedes `.current` declarations; the second-validation regression failed before the fix. Standalone card callers still validate. |
| R12 | FIXED | Model aborts retain completed evidence and name every unrun case; cleanup/close still run, including declined and model flows, and cleanup storage errors cannot masquerade as absence. |
| R13 | FIXED | `place_staged` verifies the temporary copy before replacing the placed binary. `place_one` already verified its temporary copy. The bad-copy regression reproduced replacement of good bytes. |
| R14 | FIXED | Repository/binary/manifest TSV loops use tab-delimited `read`, preserving spaces without subshell loss of accumulated values; the spaced checkout regression failed before the fix. |
| R15 | FIXED | Cargo cache keys no longer contain commit SHA; scheduled breadth runs skip redundant checks; shard total comes from the matrix strategy rather than a second literal. |
| R16 | NEEDS A DECISION | Parallel contract firing changes observation order and model load; see below. |
| R17 | FIXED | Unused workspace `basal-module` dependency and `KIND_FACTS` were removed. The documented, reachable stub `elicitation.get` API is not dead code; the path-dependency conjunction correctly rejects outside paths and its offline tests pass. |
| R18 | FIXED | WAL, ownership and transcript readers share the separator-checking `session_key`; the ambiguous transcript key test failed before the fix. Local, separately labelled contract assertions retain their explicit receipt/filter/relay loops rather than sharing assertion state. |
| R19 | FIXED | Negative positions are rejected (red-before-fix regression), WAL offsets derive from named frame fields, and the supervisor registration pause is named. |
| R20 | FIXED | Stale catalogue counts were removed, scope/stub/hash docs corrected, and staged-build skips now name all four unavailable hook-dependent cases. |
| R21 | FIXED | Rig stores use atomically allocated scratch directories with Drop cleanup, including sidecars; Python store fixtures/readers explicitly close connections. |

### Decisions and limits

* **K14:** The protocol test parent and durable core host intentionally model
  different boundaries. Renaming their public types, or making both use one
  RNG implementation, is a workspace test-double/API decision; keep separate
  semantics and coordinate any naming/RNG consolidation with H.
* **K18:** An asynchronous accept counter is not proof that no connection was
  attempted, and treating every accept error as WouldBlock hides fixture
  failures. At merge, combine E's port-443 changes with a resolver spy that
  proves unauthorized requests stop before DNS, and distinguish WouldBlock,
  Interrupted and fatal listener errors; preserve counts only as accepted-
  connection diagnostics.
* **K21:** The source guard catches the intended constructor-order mutant but
  is coupled to formatting/naming and does not directly observe execution.
  Keep it until a production startup seam exposes nonce consumption and pool
  construction as ordered events; do not replace it with a proxy that misses
  that boundary.
* **R16:** Independent flow observations can be awaited concurrently, but
  several model and crash cases depend on earlier durable checkpoints.
  Recommend parallelizing only independent waits with separate evidence
  buffers; retaining sequential dependent model cases avoids changing load,
  token accounting or the chronology the suite proves.

The per-run `drive` timeout does not redefine basal-core's WorkerSource
interface: callers must separately configure the source handshake budget.
The two benchmark summary schemas and the individually labelled relay
assertions remain separate on purpose. The portable non-macOS RSS fallback
still uses `ps`; deployed workers and these measurements are macOS-only.

## Nightly breadth review

All three changed rows were replayed with runner 0.9.1 using
`ckdev-mutate run --only <id> --broad`; **all three finished HUB**, with no
unreviewed target remaining:

| Row | Reviewed additional targets | Evidence |
|---|---|---|
| `production-worker-inherits-parent-privacy-identity` | `privacy_launch` | The runtime responsible-process probe and `production_pool_constructor_requires_its_own_trampoline` both require production disclaiming. `only = false` permits that deliberate second catcher. |
| `a-disabled-schedule-ticks` | `schedule_flows`, `package_instances` | `removed_instance_schedule_pauses_and_resumes_without_catch_up` failed because a paused schedule planned fires, exactly the inactive-schedule property the mutant removes. |
| `install-gate-an-activation-does-not-ask-core-first` | `flow_scope_recovery`, `flow_scopes`, `package_instances` | The new package catchers are `self_replays_from_admission_and_upgrade_keeps_kv_and_old_run_version` (missing frozen-version refresh query) and `removed_instance_admits_nothing_and_cancels_at_next_activation_without_disabling` (resumed membership not revalidated). |

All six new rows were proved with `ckdev-mutate prove --catalogue mutations.toml`
and then individually replayed: CAUGHT. The metadata-only native-bridge row
and mechanical list-visibility row were also individually replayed: CAUGHT.
Mutation instrumentation captured a nonempty unstaged source diff during
each new proof, followed by an empty diff after source checkout/touch restore.
The three Rust proof targets kept their two sibling tests green.

The report/helper proofs do not substitute for running the live contract
against an external daemon. The runner explicitly warns that body mutations
alone do not prove callers reach `check_receipts` / `report_model_abort`; the
offline tests directly exercise those bodies, while the live call sites are
reviewed in the diff. No live fleet or credentialed model suite was run here.

## Gates

| Check | Result |
|---|---|
| `cargo fmt --all --check` | Passed; rustfmt 1.10.0-stable, all seven crates. |
| `cargo clippy --workspace --all-targets -- -D warnings` | Passed; Clippy 0.1.99, all seven crates/targets, no allow lists. |
| `cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings` | Passed; Clippy 0.1.99, all seven crates/targets, no allow lists. |
| `env -u BASAL_WORKER_BIN -u BASAL_CUT_EXHAUSTIVE cargo test --workspace --locked` | Passed; Cargo 1.99.0, 451 tests in 82 result targets, zero failures. |
| Same suite with `--features basal-module/rig-kill-hook` | Passed; Cargo 1.99.0, 452 tests in 82 result targets, including the named send-hook test. |
| `CARGO_TARGET_DIR=target/msrv cargo +1.88.0 check --workspace --all-targets --locked` | Passed on final code; Cargo 1.88.0, all seven crates/targets. |
| Same minimum-Rust check with `--features basal-module/rig-kill-hook` | Passed on final code; Cargo 1.88.0, all seven crates/targets. |
| Four explicit `python3 -m unittest script.tests.* -v` modules | Passed; Python 3.9.6, 38 tests. |
| `python3 script/check-path-deps.py` | Passed; Python 3.9.6; Cargo metadata inspected the workspace boundary. Its five offline tests also passed. |
| `python3 script/check-ckdev-names.py` | Passed; Python 3.9.6, 88 files, zero violations. |
| `shellcheck -x script/*.sh` | Passed; ShellCheck 0.11.0, four scripts. |
| `/bin/sh -n script/stage.sh` and `script/flows-rig.sh` | Passed. |
| Mutation proofs/replays | Runner 0.9.1: six new proofs CAUGHT; eight narrow row replays CAUGHT; three broad replays HUB. |
| `ckdev-mutate check` | **Did not complete.** Two compile-queue timeouts: first after four minutes while listing the worker descriptor target; second after 60 minutes while listing `broca-absent-usage-becomes-zero`. Parent instructed no third hour-long retry, individual replay of every added/touched row, and full catalogue validation on the merged tree. |
| AFT diagnostics | Partial (Rust analyzer still indexing; no authoritative snapshot). The Cargo checks and exact Clippy gates above are the completed type/diagnostic verification. |

Local logs and machine-readable proofs are ignored artifacts in
`target/workspace-tests.log`, `target/workspace-feature-tests.log`, and
`target/mutations/*-proof.json` / `target/mutations/replay-*.json`.
