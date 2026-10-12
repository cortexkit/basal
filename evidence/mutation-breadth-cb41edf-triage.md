# Nightly breadth triage for cb41edf

The scheduled run [38130550966](https://github.com/cortexkit/basal/actions/runs/38130550966)
failed nine controls; push CI did not exercise their package-wide collateral
catches. The seven job logs were retrieved with
`gh api --allow-escape-sequences repos/cortexkit/basal/actions/jobs/<id>/logs`
for jobs 114443788309, 114443788263, 114443788343, 114443788410,
114443788357, 114443788418, and coverage job 114505219243.

All nine failures reproduced with `--broad` on main at af76f8d before repairs.
Main's upgrade to subc protocol 0.31 and explicit tool call modes at c93c558
was subsequently merged into the repair branch, followed by current main at
0621b31 (content/v1 results and verified git
repository descriptors). The final proofs use the CI-pinned ckdev-mutate 0.9.8,
not the initially installed 0.10.0. The initial repair changed the nine failing
catalogue rows. The review below also settles the 33 warning-only rows.

## Repairs

`only = true` rejects any extra red test before HUB grading, even if a row
already has a reviewed `hub`. The six WRONG_TEST controls retain their named
guards and now permit reviewed shared-property catches with `only = false`.
HUB means the mutant is expected to fail tests in other targets because those
tests assert the same property; an exclusive catch permits only the named test
to fail. Catalogue `hub` is the runner's field for the shared-property reason, and
`hub_targets` names other libtest targets, not filesystem paths.

| Control | Root cause and repair | Reviewed collateral files |
|---|---|---|
| `codemode-host-route-register` | Both tests need a live codemode scope to register a usable run-scoped route. HUB, not an exclusive catch. | `crates/basal-host/src/transport.rs` (library target `basal_host`) |
| `consent-apply-once` | Both endpoints apply settled answers exactly once, including crash redelivery. Existing HUB had incompatible exclusivity. | `crates/basal-module/tests/decisions.rs` |
| `consent-requester-vectors` | Both requester corpora pin the same v2 request bytes. Existing HUB had incompatible exclusivity. | `crates/basal-module/tests/decisions.rs` |
| `consent-v2-body-validation` | Both endpoints enforce the same requester-owned v2 body validator. Existing HUB had incompatible exclusivity. | `crates/basal-module/tests/decisions.rs` |
| `grant-card-engine-retracts-core-card` | Withdrawal reaches the path that raised the card before the local obligation is acknowledged. HUB covers both paths. | `crates/basal-module/tests/consent_paths.rs` |
| `management-fixture-default-starts-a-spare` | The default management fixture starts no flow workers; codemode's dedicated pool must leave that pool unchanged. | `crates/basal-module/tests/codemode.rs` |
| `s7-codemode-handler-blocking-thread` | The SDK test daemon could wait forever, and a held mock core open could block a Tokio worker forever. Bound both artificial waits to 60 s and bound SDK setup to 60 s. Synchronous admission must remain off Tokio workers for both SDK description and held-call withdrawal. | `crates/basal-module/tests/tool_provider_conformance.rs` |
| `s7-codemode-macos-no-address-space-limit` | Worker startup fails under RLIMIT_AS on macOS. `Core::wait_sent` then panics holding its mutex; `Rig::drop` called `Core::release`, whose poisoned-lock unwrap panicked again. Release logs and recovers only during unwinding, preserving strict failure otherwise. | `crates/basal-module/tests/{codemode,codemode_scope,tool_provider_conformance}.rs` |
| `the-scope-stamp-on-a-route-s-bind-is-dropped` | Missing agent authority prevents a held core open. `RunScopes::await_held` panics holding its mutex; `Live::drop` called `RunScopes::release`, whose poisoned-lock unwrap panicked again. Release logs and recovers only during unwinding. SDK and provider tests also require the same attested bind stamp. | `crates/basal-module/tests/{e2e,codemode_scope,tool_provider_conformance}.rs` |

The two double-panic chains were reproduced individually with `--exact
--nocapture`, rather than inferred from SIGABRT alone. No abort is caught. No
assertion or existing deadline is weakened. The artificial core-open hold fails
loudly on timeout; it does not silently release and return a successful reply.
The original 180-second provider waits remain unchanged. The macOS startup
control allows 1800 s for their cumulative duration under broad replay; the
blocking-thread control also has an explicit 1800-second replay budget.

## Warning settlement at 36babd5

All **33 CAUGHT_BROADLY rows** reproduced with `--broad` at main's tip,
36babd59938ea3601325e2654971b0b32003656a, using the CI-pinned ckdev-mutate
0.9.8. The JSON-number parsing control,
`codemode-canonical-parses-nearest-double`, and the catalog-entry sorting control,
`codemode-digest-sorts-entries-by-name`, ran on Linux; the other 31 ran on macOS.
Linux replays used eight vCPUs. Each outcome below was reviewed against the
actual failing tests in current code, not inferred from older nightly target
lists.

HUB approval is specific to the guarded property. A prerequisite catch qualifies
when that prerequisite is the guarded behavior: applying an approved install card
as rejection leaves no approved flow to admit, for example. It does not imply
that a fixture explicitly asserts approval. An unrelated setup error, or breaking
the presence of a capability while testing its grammar, is not a shared-property
catch. No assertion was weakened and no test was removed.

The catalogue's `hub` field records the concrete property shared by the named
and collateral tests; `hub_targets` records the reviewed libtest target names.
The files below identify exactly the reviewed targets, including
other tests in the named guard's own target when those tests also fail. Library
target `basal_module` includes both `codemode.rs` and `caller.rs` unit tests;
`basal_worker` includes the worker's engine unit tests.

Path abbreviations in the table resolve to actual test files:

- `Core/x`: `crates/basal-core/tests/x.rs`.
- `Module/x`: `crates/basal-module/tests/x.rs`; `Module/lib(codemode)` means
  `crates/basal-module/src/codemode.rs` and `Module/lib(caller)` means
  `crates/basal-module/src/caller.rs`.
- `Kit/x`: `crates/basal-testkit/tests/x.rs`, except `journal_cut` and
  `journal_kill`, which are test directories containing `main.rs` and helpers.
- `Worker/x`: `crates/basal-worker/tests/x.rs`; `Worker/lib` means
  `crates/basal-worker/src/engine.rs` and its library tests.

| Warning row | Reviewed collateral test files | Outcome and concrete shared property |
|---|---|---|
| `codemode-canonical-parses-nearest-double` | Core/codemode_digest_vectors | HUB: JSON text rounds to the nearest binary64 value before RFC 8785 canonicalization; the independent RFC-example oracle catches the same loss of serde_json's correctly rounded parsing. |
| `codemode-digest-sorts-entries-by-name` | Core/codemode_digest_vectors | HUB: catalog entries sort by UTF-16 name before hashing; reordered-entry vectors require the same canonical catalog digest. |
| `a-re-issued-model-call-sends-the-script-s-bytes-not-the-journaled-request` | Kit/{builtins_dispatch,flow_scopes,fs_write_temps,journal_cut,journal_kill} | HUB: reissued calls use frozen journaled request bytes, including model clamps, target-scope envelopes and temporary-write envelopes. |
| `classes-fs-write-does-not-honour-idempotency-keys` | Kit/{builtins_dispatch,fs_write_temps} | HUB: fs.write is a keyed idempotent mutation, so recovery resends an unknown write rather than requiring reconciliation, including temporary-file recovery. |
| `codemode-tool-keyed-attach` | Module/{codemode,tool_provider_conformance} | HUB: repeating a committed call key attaches to the existing run without second admission; API, worker-pool and provider retries depend on that attachment to retain the run and its original result. |
| `decision-cards-every-raise-goes-out-under-a-fresh-dedup-key-so-a-changed-card-is-a-second-card` | Module/{consent_paths,decisions} | HUB: a changed decision intent keeps its durable dedup identity on both endpoints, updating one card and settling the same journaled intent. |
| `decision-cards-the-default-is-an-action-rather-than-the-declining-option` | Module/{consent_paths,decisions} | HUB: request defaults name the declining option, not an executable action; both pinned requester corpora and core-validating settlement fixtures enforce that contract. |
| `fs-a-path-is-not-required-to-lie-under-a-root-missing-path` | Kit/{builtins_dispatch,builtins_fs,fs_write_temps} | HUB: missing-path ancestors and resolved paths remain under granted roots, including a temporary-write directory moved outside its root. |
| `integration-omitted-digest-sends-silent` | Module/{audience,hosts} | HUB: an omitted digest action uses the recipient's approved cap, including piggyback, rather than unconditional silent. |
| `worker-top-level-evaluation-skips-js-time-metering` | Worker/{budgets,codemode,fuzz} | HUB: every engine entry, including codemode's top-level wrapper, is metered by the JS CPU budget. |
| `a-resend-mints-a-new-idempotency-key` | Kit/{flow_scope_recovery,fs_write_temps,journal_cut,journal_kill,journal_unknown} | HUB: recovery and reconciliation retain the original committed call key, so effects and temporary-file cleanup are not duplicated. |
| `codemode-tool-late-retention-boundary` | Module/lib(codemode) | HUB: unacknowledged late results expire with their identity at 24 hours, both per session and on the provider route. |
| `decision-cards-a-re-enable-answer-does-not-enable-the-flow` | Module/consent_paths | HUB: an affirmative re-enable answer enables its flow on either settlement endpoint before acknowledgement or recovery of the answer. |
| `decision-cards-every-raise-goes-out-under-a-fresh-dedup-key-so-a-re-raise-after-a-crash-is-a-second-card` | Module/{consent_paths,decisions} | HUB: crash recovery or a lost reply re-raises the durable intent under its original dedup identity, retaining one card on either endpoint. |
| `fs-a-path-is-not-required-to-lie-under-a-root-symlink-out` | Kit/{builtins_dispatch,builtins_fs,fs_write_temps} | HUB: symlink resolution cannot escape granted filesystem roots, including temporary-write directories and compromised envelopes. |
| `hosts-provider-refusal-fulfilled` | Module/{audience,hosts} | HUB: typed core refusals stay rejected with their original code and message, including host admission/retry checks and audience script refusals. |
| `intrinsics-are-not-frozen` | Worker/{lib,codemode,sandbox_integrity} | HUB: intrinsic and global objects are frozen before script code, including codemode and wrapper declarations. |
| `the-manifest-declares-a-capability-subc-protocol-s-grammar-refuses` | Module/manifest_gate | Narrowed: add a malformed capability without removing tool-provider/v1, so the unrelated codemode capability-presence check remains green. HUB only for the revision test's typed decoding of the same grammar-valid manifest; both grammar witnesses remain red. |
| `agents-may-read-flow-health` | None | Narrowed: permit only agents in addition to the existing operator/core allowlist, not unrelated unscoped or unverified routes. Only authorization_matrix fails; all caller-binding tests remain green. |
| `an-unscoped-route-that-is-neither-direct-nor-core-s-is-taken-as-the-operator` | Module/{audience,lib(caller)} | HUB: a route without the operator's attested identity cannot acquire operator authority; identity classification and audience authorization reject unrelated or unverified principals fabricated as the operator. Verified module audience access is not being denied. |
| `calls-of-one-run-dispatched-serially-on-the-activation-s-thread` | Kit/{dispatch_slots,flow_scopes,fs_write_temps,journal_ownership} | HUB: dispatch runs concurrently off the activation thread, so a held or panicking host cannot take over that thread and block other calls, deadline/ownership progress or temporary-write crash recovery. |
| `decision-cards-a-card-whose-raise-failed-is-recorded-as-raised-so-the-intent-is-lost` | Module/consent_paths | HUB: a failed initial or replacement raise preserves its durable, unacknowledged intent for retry on either endpoint. |
| `decision-cards-an-expired-card-applies-an-action-instead-of-its-default` | Module/{consent_paths,decisions} | HUB: expiry applies the declining default, never an affirmative reconcile/re-enable action, including owned-card retention. |
| `hosts-registry-admission-bypassed` | Module/{audience,hosts} | HUB: catalog and audience admission fail closed when an agent registry is absent, unreadable or malformed; an unknown agent gains no authority. |
| `outcomes-arriving-during-an-activation-are-never-released-by-it` | Kit/{builtins_dispatch,dispatch_authorization,dispatch_kv,dispatch_limits,dispatch_tokens,flow_scope_recovery,flow_scopes,fs_write_temps,global_model_scope,journal_admin,journal_cut,journal_identity,journal_kill,journal_ownership,journal_races,journal_unknown,package_instances,template_instances} | HUB: available mailbox outcomes are delivered in the current activation after prefix replay; live calls, refusals, recovery, temporary writes and instance operations depend on that delivery rather than exhausting their activation deadlines. |
| `recovery-does-not-resend-keyed-mutations-parent-kill-9` | Kit/{builtins_dispatch,dispatch_kv,dispatch_tokens,fs_write_temps,journal_cut,journal_unknown} | HUB: recovery resends unresolved keyed mutations with their original request and key, including built-in writes, KV, models and temporary-file completion. |
| `a-card-s-approval-is-applied-as-a-rejection` | Module/{audience,caller_binding,dry_run,e2e,engine,health_contract,idle_wake,list_contract,module_lifecycle,ops,pool,templates} | HUB: an approved install card approves its version and admits its flow; applying approval as rejection leaves flow-running fixtures with no approved audience, owner or admissible run. This does not claim fixtures explicitly assert approved_version. |
| `decision-cards-an-answer-delivered-twice-is-applied-twice` | Module/{consent_paths,decisions} | HUB: duplicate or crash-redelivered settled answers apply the journaled action exactly once on either endpoint. |
| `decision-cards-the-body-leaves-out-the-unknown-reason` | Module/{consent_paths,decisions} | HUB: reconcile request bodies include their journaled unknown reason; requester corpora and core-validating decision fixtures enforce the same closed v2 body contract. |
| `decision-cards-the-request-carries-a-field-core-s-vectors-do-not` | Module/{consent_paths,decisions} | HUB: request envelopes remain byte-compatible with core's closed vectors, with no extra scope field. |
| `flow-deferred-resend-changes-call-key` | Kit/{fs_write_temps,journal_cut,journal_kill,journal_unknown} | HUB: scope-deferred and crash-recovered retries retain the committed call key through reconciliation and temporary-file completion. |
| `interrupt-handler-never-stops-the-engine-cpu-loop` | Worker/{codemode,fuzz,sandbox_integrity} | HUB: the interrupt handler stops unbounded engine entries under the JS CPU budget, including loops, regex payloads and codemode. |
| `recovery-does-not-resend-queries-cuts-in-recovery-suffixes` | Kit/{builtins_dispatch,flow_scopes,journal_cut,journal_kill} | HUB: unanswered queries are resent after cuts and parent kills, including scoped queries that must reach a new readiness check before returning their outcome. |

The initial replay of the unknown-reason control also encountered an unrelated
AlreadyExists error while creating unknown_control's private test directory.
That setup collision is not approved as HUB collateral; unknown_control is not
in the reviewed target list.

## Nine-control repair proof and gate results

These results are from the branch after merging main at 0621b31. Each command
was `ckdev-mutate run --only <control> --broad`, using 0.9.8 and the row's declared
platform. All **nine** controls grade **HUB**, with their named guard red, every
baseline green, and no ERROR, WRONG_TEST, TIMED_OUT, or unreviewed breadth warning.

| Control | Platform | Failing / passing tests under the mutant | Mutant test time |
|---|---|---|---|
| `codemode-host-route-register` | Linux | 2 / 143 | 20.529 s |
| `grant-card-engine-retracts-core-card` | Linux | 2 / 358 | 88.938 s |
| `s7-codemode-handler-blocking-thread` | Linux | 2 / 358 | 186.685 s |
| `consent-apply-once` | macOS | 3 / 352 | 114.203 s |
| `consent-requester-vectors` | macOS | 2 / 353 | 121.732 s |
| `consent-v2-body-validation` | macOS | 2 / 353 | 117.035 s |
| `management-fixture-default-starts-a-spare` | macOS | 2 / 353 | 121.857 s |
| `s7-codemode-macos-no-address-space-limit` | macOS | 17 / 338 | 877.948 s |
| `the-scope-stamp-on-a-route-s-bind-is-dropped` | macOS | 13 / 342 | 186.022 s |

Separate exact-name witnesses exercised each of the nine primary oracles with
all other tests filtered: each produced exactly one named failure. For each
witness the live files were staged, a nonempty `git diff --stat` was captured
while the mutant was applied, and checkout plus touch restored an empty diff.
The scope-stamp witness adds only the safety-marker comment to the catalogue's
source edit, and the management witness omits the empty `edits` metadata when
calling the single-file witness helper. Sources and catalogue were restored.

Final gates:

- Rust/cargo 1.99.0, clippy 0.1.99: `cargo fmt --all --check` and both
  `cargo clippy --workspace --all-targets -- -D warnings` configurations
  (default and `--features basal-module/rig-kill-hook`) pass on Linux, covering
  all eight workspace packages.
- `cargo test --workspace --locked`: Linux **1299 passed**, zero failed or
  ignored; macOS **1211 passed**, zero failed, one ignored. Each host ran 132
  target summaries including doctests and platform-empty targets.
- ckdev-mutate 0.9.8 `check` passes on macOS, verifying catalogue anchors and
  exact test names for the full 1132-row catalogue. A Linux invocation of the
  full check encountered an unrelated standalone basal-rig link failure:
  `unable to find library -lsqlite3`. Workspace tests and all three Linux
  controls pass; no dependency or linkage changes were made for this host gap.
- Python 3.14.4: naming gate checks 148 files with zero violations; path-dependency
  gate passes for the eight workspace packages; mutation coverage reports
  Linux 565, macOS 500, Windows 67, total 1132 with one platform per row;
  `python3.14 -m unittest script.tests` runs **62 tests**, all passing.
- Comment review examined all five changed code/catalogue comment blocks;
  the provider-wait budget comment was clarified after its first review.

## Warning-settlement proof

Every settled row was replayed with `--broad` on its declared platform, using
ckdev-mutate 0.9.8. The final result is **32 HUB and one exclusive CAUGHT**, with
zero CAUGHT_BROADLY warnings, ERROR, WRONG_TEST or TIMED_OUT results. No original
warning disappeared without a repair or reviewed HUB declaration. All baselines
were green, and each final collateral target set exactly matches its reviewed
`hub_targets` set. The unrelated unknown_control directory collision did not
recur in the final replay; that test remained green.

Both narrowed controls preserve the guarding test names in their catalogue
`expect_red` fields:

- `agents-may-read-flow-health` now permits only `Caller::Agent` in addition to
  the existing operator/core health allowlist. Only `authorization_matrix`
  fails, so the result is CAUGHT; every caller-binding test remains green.
- `the-manifest-declares-a-capability-subc-protocol-s-grammar-refuses` now adds a
  malformed capability without replacing tool-provider/v1. The result remains
  HUB for `the_built_binary_prints_a_manifest_subc_protocol_accepts` and
  `the_manifest_and_version_declare_the_embedded_build_revision`, which both
  validate the emitted manifest's grammar. Codemode's capability-presence
  assertion remains green.

| Control | Platform | Grade | Failing / passing tests under the mutant |
|---|---|---|---|
| `codemode-canonical-parses-nearest-double` | Linux | HUB | 3 / 298 |
| `codemode-digest-sorts-entries-by-name` | Linux | HUB | 2 / 299 |
| `a-card-s-approval-is-applied-as-a-rejection` | macOS | HUB | 37 / 318 |
| `a-re-issued-model-call-sends-the-script-s-bytes-not-the-journaled-request` | macOS | HUB | 9 / 228 |
| `a-resend-mints-a-new-idempotency-key` | macOS | HUB | 7 / 230 |
| `agents-may-read-flow-health` | macOS | CAUGHT | 1 / 354 |
| `an-unscoped-route-that-is-neither-direct-nor-core-s-is-taken-as-the-operator` | macOS | HUB | 3 / 352 |
| `calls-of-one-run-dispatched-serially-on-the-activation-s-thread` | macOS | HUB | 7 / 230 |
| `classes-fs-write-does-not-honour-idempotency-keys` | macOS | HUB | 4 / 233 |
| `codemode-tool-keyed-attach` | macOS | HUB | 3 / 352 |
| `codemode-tool-late-retention-boundary` | macOS | HUB | 2 / 353 |
| `decision-cards-a-card-whose-raise-failed-is-recorded-as-raised-so-the-intent-is-lost` | macOS | HUB | 3 / 352 |
| `decision-cards-a-re-enable-answer-does-not-enable-the-flow` | macOS | HUB | 5 / 350 |
| `decision-cards-an-answer-delivered-twice-is-applied-twice` | macOS | HUB | 3 / 352 |
| `decision-cards-an-expired-card-applies-an-action-instead-of-its-default` | macOS | HUB | 4 / 351 |
| `decision-cards-every-raise-goes-out-under-a-fresh-dedup-key-so-a-changed-card-is-a-second-card` | macOS | HUB | 15 / 340 |
| `decision-cards-every-raise-goes-out-under-a-fresh-dedup-key-so-a-re-raise-after-a-crash-is-a-second-card` | macOS | HUB | 15 / 340 |
| `decision-cards-the-body-leaves-out-the-unknown-reason` | macOS | HUB | 13 / 342 |
| `decision-cards-the-default-is-an-action-rather-than-the-declining-option` | macOS | HUB | 15 / 340 |
| `decision-cards-the-request-carries-a-field-core-s-vectors-do-not` | macOS | HUB | 3 / 352 |
| `flow-deferred-resend-changes-call-key` | macOS | HUB | 7 / 230 |
| `fs-a-path-is-not-required-to-lie-under-a-root-missing-path` | macOS | HUB | 8 / 229 |
| `fs-a-path-is-not-required-to-lie-under-a-root-symlink-out` | macOS | HUB | 8 / 229 |
| `hosts-provider-refusal-fulfilled` | macOS | HUB | 5 / 350 |
| `hosts-registry-admission-bypassed` | macOS | HUB | 3 / 352 |
| `integration-omitted-digest-sends-silent` | macOS | HUB | 3 / 352 |
| `interrupt-handler-never-stops-the-engine-cpu-loop` | macOS | HUB | 5 / 94 |
| `intrinsics-are-not-frozen` | macOS | HUB | 4 / 95 |
| `outcomes-arriving-during-an-activation-are-never-released-by-it` | macOS | HUB | 81 / 156 |
| `recovery-does-not-resend-keyed-mutations-parent-kill-9` | macOS | HUB | 9 / 228 |
| `recovery-does-not-resend-queries-cuts-in-recovery-suffixes` | macOS | HUB | 5 / 232 |
| `the-manifest-declares-a-capability-subc-protocol-s-grammar-refuses` | macOS | HUB | 2 / 353 |
| `worker-top-level-evaluation-skips-js-time-metering` | macOS | HUB | 4 / 95 |

Separate exact-name witnesses ran the primary oracle for all 33 rows on the same
platforms. Each baseline passed, and each mutant produced exactly one named
failure with all other tests filtered. Each witness staged the live state before
applying its NON-VACUITY BREAK edit, captured a nonempty source diff, and restored
with checkout plus touch to an empty unstaged diff. No witness left a mutant in
the tree.

### Warning-settlement gates

- Rustfmt 1.10.0-stable: `cargo fmt --all --check` passes for all eight workspace
  packages.
- Rust/cargo 1.99.0 and clippy 0.1.99: both
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings`
  pass on Linux for all eight workspace packages.
- `cargo test --workspace --locked` passes on both platforms: Linux 1299 passed,
  zero failed or ignored; macOS 1211 passed, zero failed, one ignored. Both runs
  have 132 target summaries, including doctests and platform-empty targets. The
  full Linux workspace passed without excluding basal-rig.
- ckdev-mutate 0.9.8 `check` verifies the catalogue's anchors and exact test names
  for all 1132 rows on macOS.
- Python 3.14.4: `script/check-ckdev-names.py` checks 150 files with zero
  violations; `script/check-path-deps.py` passes for eight local workspace
  packages; `script/check-local-paths.py` checks 560 text files with zero
  violations; `.github/workflows/check_mutation_coverage.py` reports Linux 565,
  macOS 500, Windows 67, total 1132, with exactly one platform per row;
  `python3.14 -m unittest script.tests` runs 66 tests, all passing.
