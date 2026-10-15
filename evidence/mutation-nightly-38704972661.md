# Breadth audit 38704972661: catalogue repair

The completed audit ran on main `be57c3530d0ff9b26e5b4c4d14fba89fd3a85045`.
This repair starts at `f763b02ac9b3ec724c6f2b56890d6b887b4f6ba5` and changes
**exactly 43 existing rows: five WRONG_TEST and 38 CAUGHT_BROADLY warnings**.
No product code, test assertion, timeout, platform assignment or row ID changes.
There are no deleted or UNREACHABLE rows.

## Inventory and interpretation

The inventory was collected with
`gh api repos/cortexkit/basal/actions/runs/38704972661/jobs --paginate`, then
`gh api --allow-escape-sequences repos/cortexkit/basal/actions/jobs/<job-id>/logs`
for every job with logs. All **38 jobs** are accounted for: 30 mutation shards,
the matrix and coverage jobs had readable logs; six skipped jobs returned HTTP
404. All 30 mutation `all.json` artifacts were also downloaded through the REST
API to resolve exact red tests and failure sites, including successful shards.
The repeated WRONG_TEST in the coverage job is not a sixth failed row.

The companion [JSON evidence](mutation-nightly-38704972661.json) preserves the
job/shard inventory, every issue's historical red set and guard failure, exact
other test files, disposition, selected witnesses, and observed replay results.
Raw logs and full test-output tails are not committed. Linux results were
captured from runner stdout; remote JSON files were ephemeral and are not
retained. Historical red names in `before` are not presented as newly observed
Linux red names.

All five WRONG_TEST rows have additional witnesses in **other Cargo targets**,
not additional named tests in their primary target. They therefore retain their
original `expect_red`, change to `only = false`, and declare reviewed HUBs.
The schema calls the property sentence `hub`, not `hub_reason`; `hub_targets`
contains Cargo target names, not file paths. Each edited HUB lists only other
targets, excluding its own target. The JSON maps them to exact test files.
In particular, the audited `template-dry-run-at-immediate` red set is its named
template test plus the reminder fixture in `flow_templates`; the event-header
validation witness belongs to `template-value-validation`, not that preview row.

The audit grades mean: WRONG_TEST is an unexpected failing test, CAUGHT_BROADLY
warns that other targets also failed, and CAUGHT means the named tests detected
the mutation. **35 rows are reviewed HUBs; eight structural setup mutants have
narrowed run sets.** A HUB reviews the same guarded property in other targets,
not arbitrary fixture collateral. A selected run set explicitly makes **no package-wide
breadth claim**, even when invoked with `--broad`. Seven narrowed rows pair the
original guard with a historically green witness. The omitted startup callback
has no validated same-setup green witness and uses `select = "expected"`.
The reviewing maintainer approved that fallback. Windows replay is deferred
because this environment has no Windows runner; push CI checks the row targets
and the next nightly audit checks their breadth.

In the tables below, `M/` means `crates/basal-module/tests/`, `W/` means
`crates/basal-worker/tests/`, and `K/` means `crates/basal-testkit/tests/`.
These aliases abbreviate paths only; the JSON stores full repository paths.

## Linux: all 12 changed rows replayed

Each ran on Linux with four vCPUs using repository-pinned **ckdev-mutate
0.12.0**, commons revision `be120e9254c4972191b39b2e6814987918206afc`:

```sh
env -u BASAL_WORKER_BIN -u BASAL_CUT_EXHAUSTIVE ckdev-mutate run --only <row-id> --broad --allow-dirty --report <report.json>
```

All 12 baselines and replays passed: **10 HUB, two CAUGHT, no remaining warning**
in this replayed set. This is not a claim that the still-deferred platforms are
green.

| Row | Before → after | Disposition and reason; other test files |
|---|---|---|
| `production-source-uses-throwaway-durability` | WRONG_TEST → HUB | Production SQLite construction must use `synchronous=FULL` (durable commit flushing), not throwaway/off durability; the source boundary and `crates/basal-module/src/main.rs` SQLite pragma readback independently assert it. |
| `template-dry-run-at-immediate` | WRONG_TEST → HUB | A future one-shot preview fires immediately once; `M/flow_templates.rs` requires that activation in the reminder's committed fixture. |
| `template-placeholder-slot` | WRONG_TEST → HUB | Placeholders occupy whole strings in typed slots; `crates/basal-core/src/manifest.rs` also refuses a path placeholder in an event-header string slot. |
| `template-value-validation` | WRONG_TEST → HUB | Supplied values obey declared scalar types and bounds; `crates/basal-core/src/manifest.rs` and `crates/basal-core/src/events_tests.rs` reject the same invalid event-header parameter values. |
| `template-lifecycle-expired-params` | CAUGHT_BROADLY → HUB | Expired lifecycle bodies preserve canonical params; `M/module_events/lifecycle.rs` pins the same event body as the deadline witness. |
| `audience-workspace-compares-display-name` | CAUGHT_BROADLY → HUB | Membership compares exact workspace IDs, not display names; `crates/basal-host/src/transport.rs` independently requires that identity match. |
| `unknown-install-status-stays-pending` | CAUGHT_BROADLY → HUB | Unknown status is unreadable, never a rejection; `M/install_gate.rs` and the pending-card oracle require the same fail-closed decode. |
| `template-lifecycle-live-ended` | CAUGHT_BROADLY → HUB | Ended instances disappear from live snapshots; `M/flow_templates.rs` and `M/module_events/lifecycle.rs` assert the same terminal boundary. |
| `policy-shared-approval-skipped` | CAUGHT_BROADLY → HUB | Approval establishes the selected version, schedule and disable state; `M/decisions.rs` and `M/hosts.rs` require the same approved version before admission. |
| `template-lifecycle-body-retention` | CAUGHT_BROADLY → HUB | Unpublished bodies survive until acknowledgement and its retention boundary; `crates/basal-module/src/engine_tests.rs` and `M/templates.rs` observe the same pruning rule. |
| `install-request-not-dedicated` | CAUGHT_BROADLY → CAUGHT | Narrow run set: the sole shared install-request route is already a minimal source edit, and changing it prevents unrelated fixture approval; dedicated-route guard plus the unknown-outcome/positive-policy decoder witness; package-wide breadth not claimed. |
| `policy-card-raised-no-longer-decodes` | CAUGHT_BROADLY → CAUGHT | Narrow run set: the minimal shared `card_raised` discriminator breaks unrelated fixture approval; card-raised guard plus the unknown-outcome/positive-policy decoder witness; package-wide breadth not claimed. |

The selected guards are
`install_cards_only_use_core_install_request_and_never_wake_answer_polling` and
`card_raised_without_elicitation_id_is_keyed_on_the_triple`, respectively.
Each runs alongside
`unknown_install_outcome_is_an_error_even_with_a_valid_hash_or_card_id` in
`M/install_policy.rs`, with `only = true`. That witness exercises the same
module/request/decoder setup and positively decodes `ApprovedByPolicy` as well
as checking unknown outcomes. For the wrong-route mutant it does **not** prove
successful transport routing; the dedicated-route guard supplies that claim.

## macOS: 18 catalogue dispositions, replay still required

macOS module tests cannot run in this sandbox, so their mutation replays are
deferred to an unrestricted Mac. The worker and testkit rows were attempted locally with pinned 0.12.0,
but both failed **before mutation**, on their unmodified baselines:

- `interrupt-handler-never-stops-the-engine-catastrophic-regex`: the guard
  `catastrophic_regex_is_stopped_by_the_js_time_budget` reports
  `activation did not finish: Broken(Closed)`; 48 baseline failures overall.
- `recovery-does-not-resend-keyed-mutations-parent-kill-9`: the guard
  `killing_the_parent_at_every_boundary_recovers_to_the_uncut_state` reports
  `no output: basal-test-parent: worker: the worker closed its channel`;
  141 baseline failures overall.

Neither attempt reached the source mutation, so neither establishes that a
test detected the intended defect. Both need replay outside the sandbox too;
no repeated local retry or timeout increase was used.

| Row | Before → observed after; intended grade | Disposition and reason; other test files/setup |
|---|---|---|
| `flow-list-agent-sees-another-agent-s-flow` | CAUGHT_BROADLY → deferred; HUB | Lists remain owner-scoped for ordinary flows and instances; `M/templates.rs` asserts the instance boundary too. |
| `flow-list-state-diverges-from-flow-health` | CAUGHT_BROADLY → deferred; HUB | List and health agree on approved/enabled state; `M/install_gate.rs` and `M/install_policy.rs` use the same reported state for decisions. |
| `a-local-caller-refused-an-operator-action-is-told-not-permitted-not-operator-attestation-required` | CAUGHT_BROADLY → deferred; HUB | Local callers lack operator attestation at package and instance boundaries too; `M/caller_binding.rs`, `M/module_lifecycle.rs`, `M/packages.rs`, `M/templates.rs`. |
| `an-agent-s-card-author-names-its-agent-id-instead-of-the-scope-ref-it-was-stamped-with` | CAUGHT_BROADLY → deferred; HUB | Install-card authors use the attested scope ref, not agent ID; `M/caller_binding.rs` and `M/install_policy.rs` require that same author identity. |
| `a-card-s-approval-is-applied-as-a-rejection` | CAUGHT_BROADLY → deferred; HUB | Approval admits the selected version rather than rejecting it; `M/audience.rs`, `board_hygiene.rs`, `caller_binding.rs`, `dry_run.rs`, `e2e.rs`, `engine.rs`, `health_contract.rs`, `idle_wake.rs`, `install_revoked.rs`, `list_contract.rs`, `module_lifecycle.rs`, `pool.rs`, `state_epoch.rs`, `templates.rs` depend on that same approved/admissible state. All filenames in this cell are under `M/`. |
| `capture-mode-sends-calls-to-the-host` | CAUGHT_BROADLY → deferred; HUB | Capture previews make no live host effects, including board, audience and template sinks; `M/audience.rs`, `M/board_hygiene.rs`, `M/decisions.rs`, `M/e2e.rs`, `M/flow_templates.rs`, `M/hosts.rs`, `M/templates.rs`. |
| `an-owning-agent-may-ask-for-a-live-dry-run` | CAUGHT_BROADLY → deferred; HUB | Live previews require the operator, not merely the owner; `M/audience.rs` and `M/ops.rs` assert that same authorization. |
| `dry-runs-share-one-scratch-store-that-is-never-removed` | WRONG_TEST → deferred; HUB | Every preview uses a fresh scratch store; repeated version previews in `M/state_epoch.rs` must not inherit the preceding preview's approved version. |
| `health-s-auto-disabled-is-null-when-false` | CAUGHT_BROADLY → deferred; HUB | `auto_disabled` is always boolean, including false; `M/install_revoked.rs` and `M/ops.rs` require the same health shape. |
| `integration-model-store-initialization-omitted` | CAUGHT_BROADLY → deferred; CAUGHT | Narrow expected-only run set: omitting the indivisible startup initialization callback prevents common fixtures from receiving their store (`M/common/mod.rs:231`) before unrelated behavior executes; no same-setup green host witness was found, and direct selector/encoding tests bypass startup, so package-wide breadth is not claimed. |
| `package-core-only-callers` | CAUGHT_BROADLY → deferred; HUB | Package/instance operations enforce the same core-only caller boundary in `M/module_lifecycle.rs` and `M/state_epoch.rs` instance previews. |
| `the-owner-may-not-re-enable-even-a-disable-it-made-itself` | CAUGHT_BROADLY → deferred; HUB | Owners may re-enable their own owner-disabled flows, including idempotently re-ensured instances; `M/caller_binding.rs` and `M/templates.rs`. |
| `hosts-answer-acknowledged-despite-failed-decision-commit` | CAUGHT_BROADLY → deferred; HUB | Acknowledgement follows durable decision commit so restart can replay failure; `M/decisions.rs` independently tests that commit/ack ordering. |
| `hosts-undeclared-events-fabricated` | CAUGHT_BROADLY → deferred; HUB | Admission requires published event name/version declarations; `M/catalog_events.rs` and `M/module_events/reader.rs` require the same catalogue evidence. |
| `hosts-the-consent-request-sends-a-session-ref-with-a-scope-author` | CAUGHT_BROADLY → deferred; CAUGHT | Source-narrow injection to scope authors; scope-owned install-policy fixtures still share that malformed request, so select the encoding guard plus green refusal/transport witness; package-wide breadth not claimed. |
| `decision-cards-action-options-carry-an-effect-core-does-not-take` | CAUGHT_BROADLY → deferred; CAUGHT | Narrow run set: changing the shared action-effect constant makes core reject unrelated card setup in `M/decisions.rs` and `M/consent_paths.rs`; select the reconciliation guard plus green grant-loss builder witness, with no source-only way to isolate the shared constant; package-wide breadth not claimed. |
| `interrupt-handler-never-stops-the-engine-catastrophic-regex` | CAUGHT_BROADLY → baseline ERROR; HUB | The same JS CPU interrupt bounds regex, loops and wrapper execution; `W/budgets.rs`, `W/codemode.rs`, `W/sandbox_integrity.rs`. |
| `recovery-does-not-resend-keyed-mutations-parent-kill-9` | CAUGHT_BROADLY → baseline ERROR; HUB | Recovery resends keyed mutations with the original key/request; `K/builtins_dispatch.rs`, `K/dispatch_kv.rs`, `K/dispatch_tokens.rs`, `K/fs_write_temps.rs`, `K/journal_cut.rs`, `K/journal_races.rs`, `K/journal_unknown.rs`. |

The scope-author row now mutates only the scope-author branch, rather than
adding `session_ref` to operator and local requests as well. Its green
`consent_refusal_codes_and_unknown_transport_are_typed` witness uses the same
request builder and typed error path with an operator author. The decision
row's green `grant_lost_phone_vector_and_production_prompt_are_pinned` witness
builds a decision request through the same builder and checks its unaffected
typed body/prompt, not the mutated action-effect field. The expected-only model-store row runs just
`routing_host_model_selection_and_digest_sink_share_the_journal_store` and
requires that test to fail. It preserves `only = false`: a single executed test
does not establish exclusivity across a larger set.

## Windows: all 13 warning rows repaired, own-platform replay deferred

Windows CI will supply the unavailable native verification: push CI replays each row's target;
the next nightly audit checks package breadth. No Windows catch is claimed here.

| Row | Before → observed after; intended grade | Exact other test files or setup step; disposition reason |
|---|---|---|
| `windows-worker-skips-not-lpac` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows/attest.rs`: pure attestation and live startup both require the LPAC claim and `not-lpac` refusal. |
| `windows-worker-skips-capabilities-present` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows/attest.rs`: pure broken-token and check-order cases require the same `capabilities-present` refusal as live startup. |
| `windows-worker-skips-restricting-sid-mismatch` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows/attest.rs`: the single expected restricting SID and its ordered refusal are the same pure/live property. |
| `windows-worker-skips-group-not-deny-only` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows/attest.rs`: enabled logon groups are refused as `group-not-deny-only` in both pure and live startup checks. |
| `windows-worker-skips-privileges-present` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows/attest.rs`: pure token and ordering tests require the same `privileges-present` refusal. |
| `windows-worker-skips-integrity-not-untrusted` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows/attest.rs`: untrusted primary-token integrity and the corresponding refusal/check order match the live startup oracle. |
| `windows-worker-skips-handle-not-allowed` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/tests/windows_probes.rs`: inherited-handle probes require the same rejection of a handle outside the startup allowlist. |
| `windows-worker-missing-package-sid-is-not-refused` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-worker/src/confinement/windows.rs`: pure argument parsing and live startup require the same missing-package-SID refusal, never a fabricated default SID. |
| `windows-creation-drops-prohibit-dynamic-code` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-launch/tests/windows_launch.rs`: live child-policy readback requires the same dynamic-code prohibition bit as the pure mitigation-policy test. |
| `windows-launcher-context-names-are-predictable` | CAUGHT_BROADLY → deferred; HUB | `crates/basal-launch/tests/windows_launch.rs`: random unique context names prevent reuse/collision with leftover names, the same rule pinned by the pure generator oracle. |
| `windows-allowlist-skips-alpc-close` | CAUGHT_BROADLY → deferred; CAUGHT | Narrow run set: the already-minimal ALPC closure branch leaves a loader port open; startup step 3 refuses `handle-not-allowed` before unrelated worker tests execute, so no smaller source anchor preserves this guard and package-wide breadth is not claimed. |
| `windows-allowlist-skips-private-file-close` | CAUGHT_BROADLY → deferred; CAUGHT | Narrow run set: the already-minimal private-file closure branch leaves a loader/crypto-device handle open; startup step 3 refuses `handle-not-allowed` before unrelated worker tests execute, so no smaller source anchor preserves this guard and package-wide breadth is not claimed. |
| `windows-creation-drops-child-process-policy` | CAUGHT_BROADLY → deferred; CAUGHT | Narrow run set: removing the sole creation-policy attribute makes startup readback refuse `mitigation-mismatch: NoChildProcessCreation is off` before the activation handshake; no smaller source anchor preserves the missing-attribute guard, and package-wide breadth is not claimed. |

The two handle-closure rows select
`confinement::windows::allowlist::tests::startup_closes_ports_and_private_files_only`
and the historically green
`confinement::windows::allowlist::tests::an_inheritable_handle_that_is_not_stdio_is_refused`.
The inherited-handle witness calls `allowlist::check`, the validation stage
after `closed_at_startup` decides which handles to close. It supplies a leaked
inheritable file/event and must still refuse those handles instead of hiding
them. It does
not separately test `closed_at_startup`; that function's retained-handle and
closure assertions are in the guard itself. The mutant is already a single
branch of `closed_at_startup`, so narrowing source further would replace the
property with a test-specific special case.

The child-policy row pairs `an_activation_completes_under_the_full_confinement`
with the historically green
`a_primary_without_the_lpac_claim_is_refused_as_not_lpac` in
`crates/basal-worker/tests/windows_startup.rs`. Both use the same native launcher;
the witness proves an earlier token refusal still works even when the later
mitigation readback is broken. Historical setup collateral for all three rows
was in `crates/basal-worker/tests/{budgets,clock,codemode,fuzz,ipc,lockdown,profile,replay,sandbox_integrity,windows_probes,windows_startup}.rs`.
None of these structural setup failures is labelled HUB.

## Verification and non-vacuity

- Pinned `ckdev-mutate 0.12.0 check` passed separately on Linux and macOS:
  catalogue anchors and exact test names verified. The catalogue partitions
  into 627 Linux, 507 macOS and 68 Windows rows, 1,202 total. Windows execution
  and Windows exact-name validation still require CI.
- `python3.11 .github/workflows/check_mutation_coverage.py` passed:
  1,202 rows, exactly one platform each. Tool: Python 3.11.15.
- `python3.11 script/check-local-paths.py` passed with the final evidence:
  628 text files checked, zero violations. An initial evidence failure included
  the GitHub runner's absolute checkout prefix; it is now explicitly normalized
  to a repository-relative path. Python 3.11 was not
  installed on the Linux image, so the two requested Python 3.11 scripts ran
  locally rather than silently substituting the image's Python 3.14.
- A field-for-field catalogue comparison against the base verified exactly
  the 43 logged issue IDs changed, no rows added/deleted, and no edited HUB
  lists its own target. Product/test source diffs are empty.
- TOML LSP inspection is unavailable (no registered producer); the runner's
  parser/anchor/name checks are authoritative for these catalogue-only edits.

Two independent negative controls also exercised the two narrowed Linux rows.
The eight catalogue files and the mutation target were staged first; the
unstaged diff was empty. Each **NON-VACUITY BREAK** changed
`crates/basal-host/src/core_consent.rs` by `2 +-` (one insertion, one deletion).
Each targeted Cargo run executed two tests. For `install-request-not-dedicated`,
only `install_cards_only_use_core_install_request_and_never_wake_answer_polling`
failed; for `policy-card-raised-no-longer-decodes`, only
`card_raised_without_elicitation_id_is_keyed_on_the_triple` failed.
`unknown_install_outcome_is_an_error_even_with_a_valid_hash_or_card_id` stayed
green in both runs. Originals were saved under the remote job's private temporary directory,
copied back and touched after each run; both restored source bytes and an empty
`git diff --stat` were checked. No remote Git index modification was used.
Cargo/rustc: 1.99.0. The JSON records the exact names and diff-stat pairs.

## Deferred own-platform replay commands

Run these from a clean checkout outside the macOS sandbox, or on Windows for
Windows rows, with repository-pinned ckdev-mutate 0.12.0 and
`BASAL_WORKER_BIN` / `BASAL_CUT_EXHAUSTIVE` unset. These **31 rows are not claimed
verified by mutation replay**: 16 macOS module rows, two native macOS baseline
failures, and 13 Windows rows. Every command below requests broad replay; selected
run-set rows will still correctly report that package-wide breadth was not
observed.

| Platform | Row and command |
|---|---|
| macOS | `ckdev-mutate run --only flow-list-agent-sees-another-agent-s-flow --broad` |
| macOS | `ckdev-mutate run --only flow-list-state-diverges-from-flow-health --broad` |
| macOS | `ckdev-mutate run --only a-local-caller-refused-an-operator-action-is-told-not-permitted-not-operator-attestation-required --broad` |
| macOS | `ckdev-mutate run --only an-agent-s-card-author-names-its-agent-id-instead-of-the-scope-ref-it-was-stamped-with --broad` |
| macOS | `ckdev-mutate run --only a-card-s-approval-is-applied-as-a-rejection --broad` |
| macOS | `ckdev-mutate run --only capture-mode-sends-calls-to-the-host --broad` |
| macOS | `ckdev-mutate run --only an-owning-agent-may-ask-for-a-live-dry-run --broad` |
| macOS | `ckdev-mutate run --only dry-runs-share-one-scratch-store-that-is-never-removed --broad` |
| macOS | `ckdev-mutate run --only health-s-auto-disabled-is-null-when-false --broad` |
| macOS | `ckdev-mutate run --only integration-model-store-initialization-omitted --broad` |
| macOS | `ckdev-mutate run --only package-core-only-callers --broad` |
| macOS | `ckdev-mutate run --only the-owner-may-not-re-enable-even-a-disable-it-made-itself --broad` |
| macOS | `ckdev-mutate run --only hosts-answer-acknowledged-despite-failed-decision-commit --broad` |
| macOS | `ckdev-mutate run --only hosts-undeclared-events-fabricated --broad` |
| macOS | `ckdev-mutate run --only hosts-the-consent-request-sends-a-session-ref-with-a-scope-author --broad` |
| macOS | `ckdev-mutate run --only decision-cards-action-options-carry-an-effect-core-does-not-take --broad` |
| macOS | `ckdev-mutate run --only interrupt-handler-never-stops-the-engine-catastrophic-regex --broad` |
| macOS | `ckdev-mutate run --only recovery-does-not-resend-keyed-mutations-parent-kill-9 --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-not-lpac --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-capabilities-present --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-restricting-sid-mismatch --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-group-not-deny-only --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-privileges-present --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-integrity-not-untrusted --broad` |
| Windows | `ckdev-mutate run --only windows-worker-skips-handle-not-allowed --broad` |
| Windows | `ckdev-mutate run --only windows-worker-missing-package-sid-is-not-refused --broad` |
| Windows | `ckdev-mutate run --only windows-creation-drops-prohibit-dynamic-code --broad` |
| Windows | `ckdev-mutate run --only windows-launcher-context-names-are-predictable --broad` |
| Windows | `ckdev-mutate run --only windows-allowlist-skips-alpc-close --broad` |
| Windows | `ckdev-mutate run --only windows-allowlist-skips-private-file-close --broad` |
| Windows | `ckdev-mutate run --only windows-creation-drops-child-process-policy --broad` |
