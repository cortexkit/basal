# Live codemode run-scope rig

The ckdev-flows suite calls basal's real `codemode` tool on the registered
OpenCode agent's core-owned scope. It obtains the selector from
`session.scope_identity`; it never fabricates a scope or daemon stamp.
The prefrontal repository supplies the real core scope owner; its default pin
is `defe4a72a3ee4bac8ce81645df801c6907a8ce4c`, which serves the run-scope API.
The consent provider's default is `c650e9db6e87dbb28fe20dd5fc2815db5deb41e9`.
Core refuses codemode with `codemode_unavailable: initiation` until global
and project onboarding are complete. The rig uses `init.save_global` and
`init.hire_head_with_handoff`, settles the handoff, and checks the `head` phase.
The selected model name is handoff metadata only; no model is called. A before/
after read of Broca's real run index proves onboarding and codemode started no
model run.

The probe is signed with a development identifier and registered only in the
isolated rig daemon's config, never production packaging. Advertising
`agent-run-scopes/v1` makes its tool eligible for core's run-scoped catalog.
Its `probe_echo` returns text and the daemon-stamped agent/run IDs. The live
case checks the return value, captured console output, core's actual open
catalog (probe included, codemode excluded), the persisted run ID, core's
settled scope end, and a later daemon-refused call. A second program loops
forever and must end as `budget_exhausted:js_cpu`, also with its scope closed.
An agent-scoped probe call without a run ID must be `run_scope_required`.
The daemon owns expiry; providers receive no stamped deadline. The negative
post-close call proves that ended scopes cannot dispatch to the probe.

## Live result

The final run tested the basal module and worker executables built at
`db9406d92a483b67a24265f62e3306ee8738cc20`, prefrontal-core and cingulate at the pins above,
and subc daemon 0.33.7: running programs, not unit-test mocks. Build, signed placement,
configuration and cleanup ran after the deployment rehearsal released the rig.
All **23 codemode checks passed** (6 onboarding, 11 successful-program, 6 CPU):

- Read Broca's real index, resolve onboarding, complete the project through
  the real hire API, settle its handoff, and confirm Head. Global onboarding
  was already complete from the preceding verified API setup.
- Confirm zero model runs: Broca's count stayed **46 → 46** across onboarding
  and both programs.
- Obtain the registered host's real core-owned selector; refuse the probe
  on the agent scope without a run ID.
- Complete the program with `answer: 42`, capture `rig console`, and match
  the echoed text plus stamped agent/run IDs to the actual persisted run.
- Read core's open catalog: `probe_echo` present, `codemode` absent.
- For each program, read the actual keyed run and agent from basal, require
  core's terminal scope with its end settled and matching agent, then require
  the daemon's **`scope_not_live`** before another probe call can dispatch.
- Exhaust the infinite loop's JavaScript CPU budget, requiring both terminal
  status and error code **`budget_exhausted:js_cpu`**.

The package approval/card/pin/flush case passed **12/12** at both core
`16cd2fa48` and `defe4a72a`, with cingulate `c650e9db`.
The old `bd609a5` consent provider did not serve the package card; `c650e9db` matches production and the same case passes.

The earlier production-pin reruns ran **254 checks: 253 passed, 1 failed**.
`packages: seeded stale ensure verified by core's exact observable floor (no wire capture)`
failed the old assertion **`core has no retry loop: the floor and cleared marker stay stable`**.
Core returned `generation: 1003`, `marker: null`, `last_error: null`,
`confirmed_version: 1`, `pin_version: 1`, `removed: false`; the package was
`approval_state: approved`, `last_card_outcome: grant`. The deployment rehearsal
passed that assertion at live prefrontal `066a84981`; the longer reruns failed
at `16cd2fa48` and `defe4a72a` because they reached a recovery pass. Core's owner
confirmed this is deliberate bounded recovery, not changed behavior.

The seeded case now asserts the refused ensure's exact learned floor and null
marker, then waits on stored state until one recovery confirms generation
**seeded + 1**. The wait has a hang bound, not an elapsed-time assertion.
Basal's own row must independently record that corrective ensure while
preserving enablement. The old two-second stability assertion was removed
because it forbade this intended correction.

`packages: further recovery passes make no more ensure calls` is explicitly
**not run live**: there is no completed-pass observable or basal received-ensure
counter to prove two more passes are call-free. Generation is not used as a
proxy for received calls. Core tests bounded stale-generation behavior in
`flow_reconcile::tests::stale_generation_above_marker_drops_it_without_immediate_corrective_call`.

Other exercised legacy cases passed: agent setup;
local/agent/declined installs; digest/status sinks; facts; revoked/declined
nonexecution; kill recovery; relayed actions/refusals; seeded cross-agent sink
refusals; immutable package registration; package approval, scheduled activation,
unpin and revocation; cleanup. The daemon stopped afterward.

The following model cases were **not run**, as requested (no `--models`):

- `models: first minimal Luna call`
- `models: routing selection journaled before dispatch`
- `models: run.result text and run.status usage settlement`
- `models: classify returns a label`
- `models: token cap refuses before send`
- `models: crash recovers Broca result without a second run`
- `models: flow calls carry no tools`
- `models: a direct non-carrier cannot open the registered flow scope`
- `models: Broca refuses an unscoped basal send without starting a run`

Initial live attempts caught incorrect selector (`epoch` versus daemon
`scope_epoch`) and response-envelope assumptions. Protocol-decoding regression
tests now cover them; tool replies remain bare JSON, and CPU exhaustion is a
structured terminal result, not an operation Error frame. Native builds also
reported an unrelated missing LLVM library for optional debug stripping; builds
and signature/confinement gates still completed.

## Offline verification

- Rust/Cargo 1.99.0: formatting, package clippy with `-D warnings`, and all
  49 basal-rig tests passed. Linux verification used `rusqlite/bundled` because
  the remote host has no system SQLite linker library; no dependency pin changed.
- Basal's `refused_open_preserves_core_code_and_detail_without_worker` passed,
  including unchanged propagation of core's `not_granted` refusal.
- Python 3.14.4: all 56 script tests passed; development naming and path
  dependency fences passed. Mutation partitioning checked 952 rows.
- ShellCheck 0.11.0: the rig script and its sourced signing policy passed.
  ShellCheck was unavailable on the remote host, so this check ran locally.
- ckdev-mutate 0.9.8 checked that each mutation's replacement text matches the
  current source and that the named failing tests exist under their exact names.
  The final catalogue check ran locally: listing the new SQLite store test on
  the remote host hit its missing system SQLite library.
- Each of six stamp-check mutations failed exactly its named probe test,
  with the other six tests green: `requires_run_id`, `requires_agent_id`,
  `requires_core_owner`, `requires_authorized_owner`, `refuses_delegation`,
  `refuses_flow_scope`. Restored probe tests passed. Rows select Linux only.
- Neutralizing the no-model counter failed only
  `broca_run_count_observes_real_insertions_and_store_errors`; the other 14
  store tests stayed green, and all 15 passed after restoration.
- Widening the recovered-generation check failed only
  `seeded::tests::bounded_recovery_requires_exact_next_generation_and_confirmation`;
  the other 11 contract tests stayed green, and all 12 passed after restoration.
- Sidekick reviewed comments in all ten implementation files; flagged wording
  was clarified. No raw rig logs are committed.
- Native release builds passed for the probe, contract runner, basal module
  and worker. On macOS 27.0.1 the worker signing/confinement gate passed before
  any shared-rig operations.

## Coverage boundaries

The live `not_granted` case is explicitly **not run** at this core pin. Core
grants codemode to every registered head and denies it to workers; there is no
per-agent grant toggle. Plugin worker registration requires an existing live
task, which this rig does not launch. Core covers the worker refusal in
`codemode_scope::tests::workers_do_not_hold_the_codemode_grant`; basal's unit
contract above covers propagation. No worker scope is fabricated.

This probe does **not** cover AFT's allow/ask/deny decisions on real tools.
That requires the Agent File Tools provider to declare `agent-run-scopes/v1`
and handle real codemode tool calls, including permission questions and replies.
