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
not the initially installed 0.10.0. Only the nine failing catalogue rows are changed; the warnings
below are recommendations for a separate follow-up.

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

## Warning-only follow-up

The supplied logs contain **33 CAUGHT_BROADLY rows**. These warnings do not fail
a shard. They are left unchanged so the nine job-failing controls can be repaired
independently of a larger catalogue review. The table records all collateral
targets from those logs, rather than assuming
that a future replay will have the same set. Proposed HUB reasons are specific
to each row's property, not one umbrella approval for unrelated breakage.

Path abbreviations in the table resolve to actual test files:

- `Core/x`: `crates/basal-core/tests/x.rs`.
- `Module/x`: `crates/basal-module/tests/x.rs`; `Module/lib(codemode)` means
  `crates/basal-module/src/codemode.rs` and `Module/lib(caller)` means
  `crates/basal-module/src/caller.rs`.
- `Kit/x`: `crates/basal-testkit/tests/x.rs`, except `journal_cut` and
  `journal_kill`, which are test directories containing `main.rs` and helpers.
- `Worker/x`: `crates/basal-worker/tests/x.rs`; `Worker/lib` means
  `crates/basal-worker/src/lockdown.rs` and its library tests.

**Replay status:** the two Linux core warnings were replayed on merged main with
0.9.8 and still report CAUGHT_BROADLY: the named guard fails, but additional tests
in other targets also fail without a fully reviewed HUB declaration. The other
31 warning rows require a fresh replay in the
follow-up: package-wide worker, recovery, and module replays are not cheap, and
some intentionally exhaust long budgets. Their collateral sets below come from
the original nightly logs, not a claim of current replay confirmation.

| Warning row | Collateral test files from nightly | Proposed outcome and shared property |
|---|---|---|
| `codemode-canonical-parses-nearest-double` | Core/codemode_digest_vectors | HUB: parsing JSON text rounds to the nearest binary64 value before RFC 8785 canonicalization; the independent RFC-example oracle catches the same loss of serde_json's `float_roundtrip` feature (correct round-to-nearest number parsing). Current broad replay confirmed. |
| `codemode-digest-sorts-entries-by-name` | Core/codemode_digest_vectors | HUB: catalog entries sort by UTF-16 name before hashing; published reordered-entry vectors check the same property. Current broad replay confirmed. |
| `a-re-issued-model-call-sends-the-script-s-bytes-not-the-journaled-request` | Kit/{builtins_dispatch,flow_scopes,fs_write_temps,journal_cut,journal_kill} | Extend HUB: reissued calls use frozen journaled request bytes, including clamps and built-in envelopes. Review new flow_scopes and fs_write_temps catches. |
| `classes-fs-write-does-not-honour-idempotency-keys` | Kit/{builtins_dispatch,fs_write_temps} | HUB: fs.write is an idempotent keyed mutation, so recovery resends it rather than requiring reconciliation; temporary-file recovery exercises that classification too. |
| `codemode-tool-keyed-attach` | Module/{codemode,tool_provider_conformance} | HUB: repeating a committed call key attaches to the same run without second admission, on the API and real provider route. |
| `decision-cards-every-raise-goes-out-under-a-fresh-dedup-key-so-a-changed-card-is-a-second-card` | Module/{consent_paths,decisions} | HUB: a changed decision intent keeps its stable core dedup identity on both endpoints rather than creating a second card. |
| `decision-cards-the-default-is-an-action-rather-than-the-declining-option` | Module/{consent_paths,decisions} | HUB: request defaults name the declining option, not an executable action; both pinned request corpora enforce it. |
| `fs-a-path-is-not-required-to-lie-under-a-root-missing-path` | Kit/{builtins_dispatch,builtins_fs,fs_write_temps} | Extend HUB: a missing file's existing ancestor must remain under an approved root before creation or dispatch. Review temporary-file escape checks. |
| `integration-omitted-digest-sends-silent` | Module/{audience,hosts} | HUB: an omitted digest action uses the recipient's approved cap (including piggyback), not unconditional silent. |
| `worker-top-level-evaluation-skips-js-time-metering` | Worker/{budgets,codemode,fuzz} | Extend HUB: every engine entry, including codemode's top-level wrapper, is metered by the JS CPU budget. |
| `a-resend-mints-a-new-idempotency-key` | Kit/{flow_scope_recovery,fs_write_temps,journal_cut,journal_kill,journal_unknown} | Extend HUB: recovery and reconciliation resend the original committed call key, including fs.write temporary-file completion. |
| `codemode-tool-late-retention-boundary` | Module/lib(codemode) | HUB: unacknowledged late results expire with their identity at 24 hours, asserted both per session in the unit test and on the provider route. |
| `decision-cards-a-re-enable-answer-does-not-enable-the-flow` | Module/consent_paths | HUB: an affirmative re-enable answer enables its flow on either settlement endpoint. |
| `decision-cards-every-raise-goes-out-under-a-fresh-dedup-key-so-a-re-raise-after-a-crash-is-a-second-card` | Module/{consent_paths,decisions} | HUB: crash recovery re-raises the durable intent under its original dedup identity on either endpoint. |
| `fs-a-path-is-not-required-to-lie-under-a-root-symlink-out` | Kit/{builtins_dispatch,builtins_fs,fs_write_temps} | Extend HUB: symlink resolution cannot escape approved filesystem roots, including temporary-file writes. |
| `hosts-provider-refusal-fulfilled` | Module/{audience,hosts} | HUB: typed core refusals stay rejected with their original code/message, including audience sink refusals observed by script. |
| `intrinsics-are-not-frozen` | Worker/{lib,codemode,sandbox_integrity} | Extend HUB: intrinsic/global objects are frozen before any script runs, including codemode. |
| `the-manifest-declares-a-capability-subc-protocol-s-grammar-refuses` | Module/{codemode,manifest_gate} | Narrow: malformed shared manifest capability syntax is a structural break that can prevent unrelated module tests from reaching their guarded behavior; scope the mutant to the manifest guard instead of approving bootstrap failures as HUB. |
| `agents-may-read-flow-health` | Module/caller_binding | HUB candidate: agents cannot read operator-only flow health, including identities derived from attested binds; confirm the exact caller-binding failure in the next broad report. |
| `an-unscoped-route-that-is-neither-direct-nor-core-s-is-taken-as-the-operator` | Module/{audience,lib(caller)} | Extend HUB: an unrelated unscoped module route is not the operator and cannot read core/operator-only audience data. |
| `calls-of-one-run-dispatched-serially-on-the-activation-s-thread` | Kit/{dispatch_slots,flow_scopes,fs_write_temps,journal_ownership} | Extend HUB: dispatch runs concurrently off the activation thread, so a held host call cannot block other progress, including temporary-file recovery. |
| `decision-cards-a-card-whose-raise-failed-is-recorded-as-raised-so-the-intent-is-lost` | Module/consent_paths | HUB: a failed raise preserves its durable, unacknowledged intent for retry on either endpoint. |
| `decision-cards-an-expired-card-applies-an-action-instead-of-its-default` | Module/{consent_paths,decisions} | HUB: expiry applies the defined default, never an affirmative action, on either endpoint. |
| `hosts-registry-admission-bypassed` | Module/{audience,hosts} | HUB: agent registry admission fails closed; audience author/target checks cannot gain authority when the registry is absent or unreadable. |
| `outcomes-arriving-during-an-activation-are-never-released-by-it` | Kit/{builtins_dispatch,dispatch_authorization,dispatch_kv,dispatch_limits,dispatch_tokens,flow_scope_recovery,flow_scopes,fs_write_temps,global_model_scope,journal_admin,journal_cut,journal_identity,journal_kill,journal_ownership,journal_races,journal_unknown,package_instances} | Extend HUB: available mailbox outcomes are delivered in the current activation after prefix replay, including fs.write completion. |
| `recovery-does-not-resend-keyed-mutations-parent-kill-9` | Kit/{builtins_dispatch,dispatch_kv,dispatch_tokens,fs_write_temps,journal_cut,journal_kill,journal_unknown} | Extend HUB: recovery resends keyed mutations whose outcomes are unknown, with original bytes and keys, including fs.write. |
| `a-card-s-approval-is-applied-as-a-rejection` | Module/{audience,caller_binding,decisions,dry_run,e2e,engine,health_contract,hosts,idle_wake,install_gate,list_contract,module_lifecycle,ops,pool} | Extend HUB: approval actually approves its installed version; audience fixtures use the same approved-install precondition. |
| `decision-cards-an-answer-delivered-twice-is-applied-twice` | Module/{consent_paths,decisions} | HUB: duplicate/redelivered settled answers have no second effect on either endpoint. |
| `decision-cards-the-body-leaves-out-the-unknown-reason` | Module/{consent_paths,decisions} | HUB: reconcile request bodies include the journaled unknown reason and match core's closed v2 body contract on both endpoints. |
| `decision-cards-the-request-carries-a-field-core-s-vectors-do-not` | Module/{consent_paths,decisions} | HUB: request envelopes remain byte-compatible with core's closed vectors, with no extra field on either endpoint. |
| `flow-deferred-resend-changes-call-key` | Kit/{fs_write_temps,journal_cut,journal_kill,journal_unknown} | Extend HUB: a scope-deferred retry keeps the committed call key across recovery and built-in temporary-file completion. |
| `interrupt-handler-never-stops-the-engine-cpu-loop` | Worker/{codemode,fuzz,sandbox_integrity} | Extend HUB: the interrupt handler stops CPU-bound engine entries under the JS time budget, including codemode. |
| `recovery-does-not-resend-queries-cuts-in-recovery-suffixes` | Kit/{builtins_dispatch,flow_scopes,journal_cut,journal_kill} | Extend HUB: unanswered queries are resent across cuts in recovery suffixes, including scoped calls. |

Representative assertions checked for the recommendations include
`the_production_digest_matches_every_published_vector` and
`the_oracle_canonical_json_matches_the_rfc_8785_example` in core's digest vectors,
`audience_recipient_and_core_refusal_reach_script_unchanged` (both a rejected
sink and its piggyback default), `omitted_digest_action_uses_each_recipient_approved_cap`,
and `denylist_and_paged_agent_registry_fail_closed`. Existing reviewed HUB reasons
in the catalogue support the recovery, dispatch, mailbox, sandbox, and approval
extensions. The warning follow-up must inspect the actual newly red tests in
its fresh reports before adopting these proposals; this delivery does not claim
an exhaustive assertion-by-assertion review of those 31 expensive warning rows.

## Final proof and gate results

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

The first Linux attempt to use `script/witness-mutations.py` could not stage
sources because the remote git index is read-only; Linux witnesses instead used
local staging/mutation/restoration around tests on the Linux build server, using
libtest `--exact` to run only the fully named guard. The helper's
`--nocapture` output interleaved a panic between the test name and FAILED on the
first macOS witness and misclassified an actual single red as undefended. The
macOS witnesses were rerun with libtest output capture enabled, producing six
unambiguous named failures without changing the helper or any assertion.

Raw runner JSON, captured panic output, exact-guard witness logs, and workspace
test output were kept outside build directories under `/tmp/basal-breadth-pool38`
on the author's Mac. That temporary path is not a portable delivery artifact;
this note records the durable results rather than committing generated logs or
reports.
