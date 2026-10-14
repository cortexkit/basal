# Mutation catalogue repair after the first package-wide audits

The push replay of `a9608f6` failed one macOS shard on a surviving mutant. The
nightly `--broad` audit of the same commit failed macOS shards 1–3, both Linux
shards and the coverage summary. This note records why each row failed or
warned and how it was dispositioned. No row was removed. No production guard,
test assertion or test deadline was weakened. Runner: `ckdev-mutate` 0.9.5
(commons `73c7e66145e131eadffdd874c82d93548868b668`), the revision CI pins.

## What fails a shard

`ckdev-mutate` 0.9.5 counts only CAUGHT, CAUGHT_BROADLY, HUB, EQUIVALENT,
UNREACHABLE, SKIPPED_PLATFORM and DESK_ONLY as passing rows
(`Report::passes`). CAUGHT_BROADLY is therefore a warning, never an exit 1.
Every failing nightly shard failed only on **WRONG_TEST** rows. The Linux logs
name them as `WRONG_TEST`, followed by an `unexpected red:` block. They do not
use `SURVIVED`, and there was no runner error, timeout or platform mismatch.
The `mutation-coverage` job runs no mutants. It reads every shard's report and
fails on the first row that did not pass, so it failed for the same rows: on
push at the surviving row, and on the nightly run at the first WRONG_TEST row.

## SURVIVED on push: flow-scope-fixture-keeps-the-production-activation-deadline

**Cause:** the mutant gives the stalled scope fixture the production 60 s
activation deadline instead of its own 3 s one. An earlier change stopped the
test asserting elapsed time, because a loaded machine fired the 3 s deadline late
but correctly. It raised the test's hang bound to the same 60 s. After that, a
fixture with the production deadline still ended with a deadline failure inside
the bound, so the test could not tell which deadline fired.

**Fix:** the driver thread times `activate` itself. The test now requires the
activation to end before `Config::default().activation_deadline`. A production
deadline is measured from inside `activate`, so it can never end the activation
earlier. The bound is the same generous 60 s the hang check already uses, so the
load tolerance is unchanged. This check runs before the hang check, which would
otherwise hide which deadline was configured.

**Proof:** macOS `ckdev-mutate run --only
flow-scope-fixture-keeps-the-production-activation-deadline` returned CAUGHT.
The only red test was `a_stalled_scope_activation_deadline_reaps_its_worker`,
which failed with `the stalled activation took 60.006353875s, at least the
production activation deadline of 60s`. The other 22 tests in the target stayed
green.

## WRONG_TEST in the nightly audit: `only = true` against package-wide breadth

`only = true` makes 0.9.5 grade WRONG_TEST for any extra red test. Under
`--broad`, that check covers every target in the package, and it runs before
HUB grading. In all ten rows below, the nightly report shows the named guard
red. The only extra red test is in another target, and that test asserts the
same property. Each row's authoring commit says that its proof failed only the
named test in the row's own target (`70994e7`, `8189846`, `29c38ca`, `e94cd42`;
the retention, migration and worker rows are exact-name proofs from `efa26d7`,
`9e3ccaf`, `f40e7c1` and `f6cb202`). Each row now has `only = false` and records
the other target as a reviewed HUB. A comment on the row explains the change.
The plain replay below still runs only the row's own target. It confirms that
the named test is red and that no other test in that target went red.

| Row | Extra red test (target) | Shared property |
|---|---|---|
| builtin-git-leaves-descendant-pipes-open | `builtins::git::tests::git_waiter_timeout_kills_the_process_group_and_reaps_the_child` (`basal_host`) | a git timeout kills and reaps git's whole process group |
| production-retention-prunes-history | `production_retention_catches_up_full_batches_without_waiting_a_whole_interval` (`retention_backlog`) | the production deadline pass prunes expired history |
| production-retention-prunes-runs | same test (`retention_backlog`) | the production deadline pass prunes expired runs |
| builtin-encoded-result-becomes-null | `escaped_listing_exceeds_encoded_cap_as_a_typed_refusal` (`builtin_host_regressions`) | an over-cap result is a typed refusal, never null |
| builtin-write-carries-special-permission-bits | `atomic_write_drops_special_permission_bits` (`builtin_host_regressions`) | writes strip setuid/setgid/sticky bits |
| health-index-predicate-selects-deferred | `keyed_snapshot_lookup_ignores_unrelated_corruption_and_migrates_legacy_params` (`broca_store`) | the pending index covers unacknowledged, undeferred calls: the test requires the pending lookup to use `broca_calls_pending` and `pending_ids` to omit deferred calls |
| worker-prelude-primitive-code-diverges | `divergent_call_fails_with_a_typed_error` (`replay`) | prelude primitives carry the Rust wire codes (it saw `Classify` for `Facts`) |
| worker-top-level-evaluation-skips-js-time-metering | `cpu_bound_loop_…` (`budgets`), `catastrophic_regex_…` (`fuzz`) | every engine entry is metered by the JS time budget (both hung) |
| package-core-only-callers | `local_package_operations_require_operator_attestation` (`module_lifecycle`) | package operations refuse the local caller; with the check removed the local call reached parameter parsing (`invalid_params`) |
| worker-file-name-follows-parent | `privacy::production_worker_is_its_own_responsible_process` (`e2e`) | the module finds its worker by its own file name; under the mutant the development-named module looked for `ck-basal-worker` and never started a worker |

## CAUGHT_BROADLY warnings

A `--broad` catch is CAUGHT_BROADLY when the mutant also reds tests in a target
other than the named test's. A row may record a reviewed `hub` reason and
`hub_targets`, the other targets that assert the same property. The catch then
grades HUB, unless a red target is missing from that list. Each collateral
test was read. Each asserts the property its row guards, so none of the mutants
was narrowed.

Rows that had no `hub` reason now record one:

| Row | New hub target | Collateral and why it is the same property |
|---|---|---|
| broca-acknowledged-completion-is-delivered-again | `broca_dispatch_regressions` | `one_corrupt_snapshot_does_not_block_healthy_completions` asserts the accepted call is persisted acknowledged |
| a-bound-worker-is-never-retired-for-idling | `basal_module` | `engine::lifecycle_tests::worker_retirement_wakes_before_the_fallback` asserts the idle worker is retired |
| install-gate-a-version-core-does-not-know-activates | `package_instances` | `instance_core_revocation_does_not_mutate_package_bytes` sets status Unknown and expects Revoked |
| pruning-touches-runs-that-have-not-ended | `dispatch_slots` | both deadline tests lose their pending second run to the deadline pass's pruning (`NoSuchRun`) |
| stack-limit-not-applied | `sandbox_integrity` | `top_level_wrapper_code_has_activation_budgets`: its run with a 4 MiB stack budget ended `BudgetExhausted(Stack)`, so the configured budget was not applied |
| intrinsics-are-not-frozen | `basal_worker`, `sandbox_integrity` | wrapper-level declarations extended the unfrozen global object |
| lockdown-forbidden-globals-are-not-removed (and `-inventory`) | `sandbox_integrity` | `top_level_wrapper_code_is_locked_down_journaled_and_replayed` saw removed globals present |
| math-random-is-not-a-host-call | `basal_worker`, `sandbox_integrity` | the wire-code unit test and the top-level journal test saw no Random host call |
| recorded-clock-and-random-outcomes-are-not-served-from-the-prefix | `sandbox_integrity` | the top-level journal test broke on the unreplayed prefix |
| memory-limit-not-applied | `basal_worker`, `sandbox_integrity` | the allocator cap unit test and top-level memory budget test |
| module-owner-disable-leaks-raw-actor-kind | `list_contract` | list replies must decode against the documented closed disable kinds |
| net-redirect-hops-are-not-checked-against-the-allowlist | `net_fetch_regressions` | `nonstandard_redirect_port_is_refused_before_a_second_connection`: the port policy lives in the same per-hop allowlist check |

Rows that already had a `hub` reason, whose mutant also reddened a target
missing from `hub_targets`: the target was added, and the reason was widened
to name it.

| Row | Added target | Collateral |
|---|---|---|
| a-storage-error-does-not-end-the-process, a-storage-error-does-not-raise-the-fatal-latch-in-process | `basal_module` | lifecycle unit tests asserting the latch is raised for a journal store failure and for a panicked activation. Both rows' mutant makes `Fatal::raise` record nothing, which disables the latch for every cause, so the hub reason now names both causes |
| enabling-resumes-from-the-stale-next-due-time-and-catches-up-the-disabled-hours | `package_instances` | `removed_instance_schedule_pauses_and_resumes_without_catch_up` |
| disabling-a-flow-leaves-its-schedule-ticking | `package_instances` | `schedules_and_resource_windows_stay_per_instance_across_changes`: a disabled flow's schedule must stay stopped across instance changes |
| planned-fires-are-not-persisted-restart-mid-admission, planned-fires-are-not-persisted-with-the-advance | `package_instances` | the resumed instance's planned fire is never admitted |
| health-does-not-say-who-disabled-a-flow | `module_lifecycle` | `health_uses_the_same_owner_disable_kind_as_list` |
| interrupt-handler-never-stops-the-engine-cpu-loop, interrupt-handler-never-stops-the-engine-catastrophic-regex | `sandbox_integrity` | `top_level_wrapper_code_has_activation_budgets` hung on its JS time case |
| a-card-s-approval-is-applied-as-a-rejection | `idle_wake`, `module_lifecycle` | fixtures that approve an install before running a flow |
| a-local-caller-refused-an-operator-action-is-told-not-permitted-not-operator-attestation-required | `module_lifecycle`, `packages` | both assert a local package caller receives `operator_attestation_required` |

## Proofs

All proofs used `ckdev-mutate` 0.9.5, built from commons
`73c7e66145e131eadffdd874c82d93548868b668` (the revision CI pins).
`ckdev-mutate check` passed with it on both hosts.

On each host, every changed row was replayed in one session with
`ckdev-mutate run --diff 21b03f4`, where `21b03f4` is the commit just before the
catalogue change. That selects exactly the rows whose catalogue entry changed,
and each row runs the same replay that `--only <id>` would run. The session
was then repeated with `--broad`.

- Linux (remote build server): the plain replay graded all 7 Linux rows CAUGHT,
  and the `--broad` replay graded all 7 HUB. In the plain replay, none of the
  six rows that used to carry `only = true` reddened any test in its own target
  besides the named one.
- macOS (local): the plain replay graded all 27 macOS rows CAUGHT, and the
  `--broad` replay graded all 27 HUB. None of the four rows that used to carry
  `only = true` showed another red test in its own target.
- `flow-scope-fixture-keeps-the-production-activation-deadline`: macOS
  `run --only` is CAUGHT, failing at `the stalled activation took 60.004697292s,
  at least the production activation deadline of 60s`. With `--broad` it is also
  CAUGHT: of the package's 206 tests only the named test went red, so its
  `only = true` holds there as well.

## Scheduled audit 38384837131 (e52441ae)

This follow-up concerns the 2026-10-13 scheduled run of
`e52441ae74096baef398aef3d555009f9bf9948a`, not the earlier audit above.
All ten jobs that actually ran were retrieved through the GitHub REST
`actions/jobs/{job_id}/logs` endpoint and their complete payloads inspected.
The five skipped jobs had no steps and their log endpoints returned HTTP 404.
The eight available artifacts were also downloaded through REST; six contained
mutation reports, and two contained Windows application/restoration witnesses.
The runner was the workflow-pinned `ckdev-mutate` 0.12.0, commons
`be120e9254c4972191b39b2e6814987918206afc`.

### Strict revocation witness

`instance-revocation-preserves-enable` remains `only = true`. The template test
`template_version_revocation_stops_only_that_version_and_keeps_registry` checks
the same property as `instance_core_revocation_does_not_mutate_package_bytes`:
revoking the selected version must clear approval without flipping the instance's
enable switch. Its admission assertion at `template_instances.rs:162` requires
`NotApproved`, not `Disabled`; it then selects a new version and successfully
activates it. Both tests are now required in `expect_red`; the row's ordinary
replay also includes both `package_instances` and `template_instances`, so push
CI executes both required witnesses rather than looking for a test outside its
run set. No assertion changed.

### Six breadth decisions

| Row | Decision | Exact other-target witnesses / narrowed collateral |
|---|---|---|
| dry-runs-share-one-scratch-store-that-is-never-removed | Narrow the mutant to ordinary dry runs, preserving template preview naming and cleanup. | Template previews test bound values, self-end, timestamps and quota-free results, rather than the ordinary scratch-store oracle. All four previously red template tests now stay green: `instance_dry_run_uses_bound_values_owner_grants_and_approved_version`, `template_dry_run_at_fires_once_immediately_with_canonical_timestamp`, `template_dry_run_captures_self_end_with_values_and_bound_instance`, `template_dry_run_creates_no_rows_and_uses_no_quota`. `each_dry_run_has_its_own_scratch_store_and_the_real_kv_and_runs_are_untouched` still fails; the same-target `dry_runs_capture_every_built_in_and_live_runs_only_reads_in_scope` also detects reuse of an earlier dry run's admission. Both are now required in `expect_red`, with `only = true` rejecting any other red test. No HUB targets are needed. |
| health-reports-a-flow-nobody-approved-by-its-enabled-switch | Reviewed HUB; add `install_policy` to the existing `health_contract`, `install_gate`, `list_contract` targets. | Policy witnesses `declined_and_expired_hints_settle_unapproved_cards_and_are_visible`, `mismatched_policy_reply_hash_never_asks_status_or_approves`, `policy_status_must_be_active_with_the_local_hash`, and `unknown_install_outcome_is_an_error_even_with_a_valid_hash_or_card_id` all explicitly require `unapproved` with no approved version. Existing health/list/revocation witnesses assert exactly the same state rule. |
| hosts-undeclared-events-fabricated | Narrow the mutant and review the genuine declaration witnesses as HUB `catalog_events`. | Keep acknowledgement, transport and module lookup intact; retain real matching declarations and their body/provenance. Only a missing, empty or unmatched event declaration is fabricated. `catalog_events_require_an_acknowledged_daemon_capability` now stays green. `missing_empty_or_unmatched_catalog_events_are_refused` and `recorded_live_plexus_catalog_accepts_github_pr_changed_v1` still fail on their undeclared-name/version refusals. |
| install-agent-owned-foreign-targets | Reviewed HUB; add `audience` alongside `ops`. | `install_and_dry_run_apply_audience_authorship_and_resolution` reaches its explicit foreign-agent install refusal, the same self-only grant rule as the named `agent_targets` oracle and `install_refuses_what_an_agent_may_not_do` in `ops`. |
| install-gate-a-revoked-version-activates | Reviewed HUB `template_instances`. | `template_version_revocation_stops_only_that_version_and_keeps_registry` requires `ActivationEnd::Revoked` before checking retained registry state and newer-version selection. Same-target resumed-run and new-approval tests also require the revoked gate to close. |
| recovery-does-not-resend-queries-cut-harness | Reviewed HUB; add `flow_scopes` alongside `builtins_dispatch`, `journal_kill`. | `replay_reasks_core_and_outside_audience_call_fails_unsent` deliberately cuts after a query's answer but before persistence, recovers, and requires that query to be reissued through a fresh audience check. With query resend removed, its run cannot finish; this is the same unresolved-query recovery property as the cut, kill and built-in witnesses. |

The hosts oracle was strengthened, not weakened: it now acknowledges
`catalog-events/v1` before checking for an undeclared event. Previously the test
could obtain `event_not_declared` merely from the missing acknowledgement, without
exercising the declaration lookup. All its existing assertions remain unchanged.
No catalogue row was deleted and no production source change is committed.

### The six-hour limit and scheduled-only sharding

Elapsed job minutes are `completed_at - started_at` from the REST jobs response;
replay minutes use those same fields on each job's "Replay the full catalogue in
an isolated shard" step:

| Platform/shard | Job minutes | Replay minutes | Conclusion |
|---|---:|---:|---|
| Linux 1/2 | 360.95 | 358.23 | cancelled |
| Linux 2/2 | 361.38 | 355.93 | cancelled |
| macOS 1/5 | 327.72 | 325.63 | failure (WRONG_TEST) |
| macOS 2/5 | 329.60 | 327.68 | success |
| macOS 3/5 | 361.17 | 359.83 | cancelled |
| macOS 4/5 | 340.75 | 339.12 | success |
| macOS 5/5 | 310.45 | 309.08 | success |
| Windows 1/2 | 49.87 | 34.22 | success |
| Windows 2/2 | 45.95 | 32.42 | success |

`WRONG_TEST` means the named witness failed but the strict row also reddened a
test it had not declared. Here that extra red was the same-property template
revocation witness, now explicitly required.

The cancelled jobs began around 10:39 UTC and were stopped around 16:40 UTC,
with no completed report to upload. This is the hosted runner's per-job cap,
not evidence of a hung test. Increasing a timeout cannot evade that cap.

Scheduled counts are now **16 Linux / 12 macOS / 2 Windows**. Push and manual
runs retain **2 / 5 / 2**. The matrix generator selects the larger counts only
for `event == "schedule"`; the original push prerequisite and event filter
remain on the matrix job. More shards can queue behind the account's concurrency
limit, but queue time does not consume their individual six-hour execution cap.

Sizing uses the seven reported timing fields (`build_ms`, `test_ms`,
`baseline_build_ms`, `baseline_test_ms`, `prebuild_ms`, `baseline_prebuild_ms`,
`restore_prebuild_ms`). Their sums on the four completed macOS shards are
325.54, 327.58, 339.04 and 309.00 minutes, close to the measured replay durations.
The cancelled Linux and macOS shards have **no per-row report**, so their final
costs cannot be measured and the six-hour values are censored lower bounds.
The catalogue supplies deadlines, not measured per-row durations.

For the current catalogue's sorted-ID assignments, imputing unreported macOS
rows at the largest reported cost for their package (from the completed macOS
and Windows reports) gives at most **240.1 minutes** with 12 shards. For Linux,
using the completed macOS package maxima as deliberately pessimistic proxies,
using the `basal-testkit` maximum for unreported `basal-core` breadth, and allowing
10 minutes per row for packages with no native report, gives at most **286.2
minutes** with 16 shards. Those projections leave about 120 and 74 minutes,
respectively, before the hosted cap, before modest setup/teardown overhead.
They are conservative sizing assumptions, not newly measured Linux runtimes or
a guarantee against an individually hung row. A full scheduled replay cannot
be triggered from this worktree; the next hosted audit must confirm headroom.

The maximum selected row counts per new scheduled shard are 42 Linux, 46 macOS,
and 34 Windows, versus roughly 300 Linux and 100 macOS previously. Sorting is
across **all** catalogue IDs, before platform skips: the pinned runner's
`main.rs:463-476` assigns `index % count == shard - 1`. Package grouping changes
execution order later, not assignment. The workflow's generator emits every
one-based shard once. `test_mutation_matrix.py` checks its CLI output and
workflow wiring, then feeds assignments using that pinned algorithm through the
unchanged coverage checker. The complete current catalogue (1,163 rows: 591
Linux, 504 macOS, 68 Windows) is covered exactly once, including every
other-platform skip, for both the scheduled and unchanged push matrices.
These are assignment checks, not fabricated successful replay claims.

`mutation-coverage` did **not** fail only because shards were cancelled. Its log
explicitly fails first with:

```text
ValueError: instance-revocation-preserves-enable (mutations.d/core-install.toml:842): unsuccessful replay: WRONG_TEST
```

That unsuccessful row alone fails coverage. After correcting it, the absent
Linux reports and absent macOS shard 3 would independently fail the check's
missing-ID/exactly-once checks. The coverage gate and its accepted outcomes were
not changed. Three additional Windows breadth warnings in this run are outside
the six rows requested here and were not altered.

### Replays and negative controls

Each of the seven changed rows ran individually on its assigned platform,
macOS, with the pinned 0.12.0 runner:

`env -u BASAL_WORKER_BIN -u BASAL_CUT_EXHAUSTIVE ckdev-mutate run --only <id> --broad --allow-dirty --report <temporary-report>`

| Row | Outcome | Red / green tests | Other red targets |
|---|---|---:|---|
| instance-revocation-preserves-enable | CAUGHT | 2 / 241 | none beyond the two required witnesses |
| dry-runs-share-one-scratch-store-that-is-never-removed | CAUGHT | 2 / 386 | none beyond the two required same-target witnesses |
| health-reports-a-flow-nobody-approved-by-its-enabled-switch | HUB | 12 / 376 | `health_contract`, `install_gate`, `install_policy`, `list_contract` |
| hosts-undeclared-events-fabricated | HUB | 3 / 385 | `catalog_events` |
| install-agent-owned-foreign-targets | HUB | 3 / 385 | `audience`, `ops` |
| install-gate-a-revoked-version-activates | HUB | 4 / 239 | `template_instances`; two same-target witnesses |
| recovery-does-not-resend-queries-cut-harness | HUB | 5 / 238 | `builtins_dispatch`, `flow_scopes`, `journal_kill`; one same-target witness |

Every broad baseline was green. The two strict rows (instance revocation and
ordinary scratch isolation) also graded CAUGHT on ordinary, non-broad replays,
so their expanded required witness sets remain valid on push CI. Catalogue
anchor/name checks passed on both Linux and macOS. In addition, each row's first
named oracle was
run with `cargo test --locked -p <package> --test <target> <test> -- --exact
--test-threads=1`: one baseline test passed, then exactly that test failed after
applying the catalogue edit, with every other test filtered out. Before every
mutation, its source was staged and the worktree diff was empty; the mutation
produced a nonempty diff, and `git checkout -- <source> && touch <source>` restored
an empty diff. Exact witness details are included in the delivery's
`mutation_evidence`; no transient mutant remains in the tree.

The matrix negative control replaced scheduled Linux shard 16 by a second
shard 15. Of the six matrix tests, only
`MutationMatrixChecks.test_nightly_matrix_covers_every_row_once` went red
(`linux: reports must contain every catalogue ID exactly once`, with missing
and repeated IDs). The other five stayed green. During the break,
`.github/workflows/mutation_matrix.py` had `2 +-` / one insertion and one deletion;
after index restoration and touch, the diff was empty. All six tests then
passed. The full workflow Python suite passed 27 tests and the catalogue/witness
script suite passed 21. Python was 3.14.4 on Linux. Cargo 1.99.0 successfully
checked `basal-module --test hosts`; rustfmt 1.10.0 checked workspace formatting.
Local actionlint 1.7.12 validated the workflow (the Linux host lacks actionlint),
and PyYAML 6.0.3 parsed it with three matrix/coverage wiring assertions.

## Scheduled audit 38537440166 (1d207f37)

The 2026-10-14 scheduled run used commit
`1d207f37f104131f096a02f1a1ecfa5dc8679758`. It was also the newest scheduled
run in `cortexkit/basal` returned by
`gh run list --repo cortexkit/basal --event schedule` when this
repair began. Both `gh run view 38537440166 --repo cortexkit/basal --log-failed`
and the full `--log` output were retrieved, along with all 30 platform/shard
mutation artifacts. Failed-job logs contain eleven WRONG_TEST rows, one
TIMED_OUT row and six CAUGHT_BROADLY warnings. In particular, coverage failed
first on `event-verified-trigger-not-forwarded`, which was absent from the
initial list of rows to repair.

The full artifacts contain **48 distinct issue rows**: those 18 rows plus
30 warnings from successful shards. Counting the repeated coverage error as
another row would overcount. The repair covers the 18 failed-job rows; the
remaining 30 are triaged below, without changing their catalogue entries.
No workflow file or production implementation is changed.

### Replay-first observations and dispositions

Every assigned row was replayed on the prepared current checkout
`1d15cdc3b59c8ef185d3528ced62ccaccf7b8d5b`, on Linux with eight vCPUs, before
editing any row. None had already been repaired: all eleven WRONG_TEST rows,
the timeout and all six warnings reproduced. The checkout adds GitHub CI
template and state-epoch witnesses to some collateral sets; the HUB target
sets below include those newly observed witnesses rather than merely copying
the old run's sets.

`ckdev-mutate` 0.12.0 calls a catch **HUB** when tests in other targets fail
because they require the same guarded behavior, rather than an unrelated
fixture or setup failure. Its `hub` field records the reviewed shared property;
`hub_targets` names only other targets, not additional tests in the primary
target. Same-target additional witnesses remain visible in the replay's red
set. HUB does not mean arbitrary collateral is permitted: a newly red target
outside the reviewed list still warns. The following decisions do not change
any test assertion or its meaning.

| Row | Initial replay | Disposition and shared property / structural reason | Exact other targets or narrowed run set |
|---|---|---|---|
| template-params-activation | WRONG_TEST | HUB: live and preview workers receive parameter values normalized at creation and saved in the admitted run's journal. Published scripts need those same values before reading files, filtering events or delivering notifications. | `flow_templates` |
| template-one-shot-ends | WRONG_TEST | HUB: a terminal one-shot/self-end removes its own live instance and closes admission; published-template live snapshots and ended-event assertions require the same transition. | `flow_templates`, `module_events` |
| template-dry-run-self-end-not-captured | WRONG_TEST | HUB: authorized preview self-end is captured without a persisted instance; published-template previews require that same captured, not refused, call. | `flow_templates` |
| template-dry-run-no-instance-or-quota | WRONG_TEST | HUB: a preview leaves the live registry and quota untouched. Published previews must work at full quota, and epoch previews must not create a live instance. | `flow_templates`, `state_epoch` |
| template-missing-stat | WRONG_TEST | HUB: a bounded missing file returns `exists=false`, allowing a file watch to wait rather than emit a stopped notification. | `flow_templates` |
| template-canonical-path-bounds | WRONG_TEST | HUB: creation and preview refuse canonical file/repo values outside their declared roots; the published-template outside-Work assertions check the same refusal. | `flow_templates` |
| host-workspace-double-management-result-unwrap | WRONG_TEST | HUB: an already-decoded registry payload is consumed exactly once; both valid exact-ID lookup and rejection of an extra result envelope enforce that management shape. | `audience` |
| digest-replace-key-catalog-capability-gate | WRONG_TEST | HUB: replace keys are refused unsent without core's capability. Board nudges and GitHub CI digests depend on that same refusal to retry without a key. | `board_hygiene`, `flow_templates` |
| test-module-fixture-cleanup-drops-module-first | WRONG_TEST | Narrow: reordering the shared fixture destructor makes 257 tests fail on the fixture's open-store assertion, not their own contracts. Execute only the teardown oracle; do not claim 256 unrelated catches as HUB. | `select = "expected"`; `fixture_releases_its_store_before_removing_the_directory` |
| template-dry-run-effective-grants | WRONG_TEST | Narrow: replacing params by null also crashes published previews at `validated placeholder type`, not a grant refusal. Keep the five existing required template-input witnesses and an unaffected captured-self-end witness, with `only = true`. | Exact six-test `select.tests` in `mutations.d/module.toml`; positive witness `template_dry_run_captures_self_end_with_values_and_bound_instance` |
| content-addressed-copy-is-not-rewritten | TIMED_OUT | Narrow: deleting every reused executable also deletes a worker source already in its destination directory, breaking startup and poisoning the copy cache. Audit the timestamp reuse oracle plus an unaffected changed-bytes/different-directory witness, not worker-startup contracts. Bound the blocked write test as described below. | `binaries::tests::pre_existing_complete_copy_is_reused_without_rewriting`, positive `binaries::tests::changed_binary_bytes_use_a_different_directory` |
| codemode-content-result-error-unmarked | CAUGHT_BROADLY | HUB: terminal content retains `isError` and the original code through omission/size bounding and end-to-end worker budget failures. | `codemode` |
| call-modes-codemode-catalog-serves-flow | CAUGHT_BROADLY | HUB: public codemode tools serve classic mode, not flow; SDK attestation and classic provider routes require that same catalog declaration. | `codemode_scope` |
| template-lifecycle-expiry-wake | CAUGHT_BROADLY | HUB: an unscheduled instance contributes its expiry to runtime next-wake; the engine and canonical expiry witness require that deadline. | `templates` |
| template-lifecycle-expiry-deadline | CAUGHT_BROADLY | HUB: expiry and its canonical event commit at the deadline itself. Engine wake, event vectors and transactional expiry witnesses require that exact boundary. | `basal_module`, `module_events` |
| template-lifecycle-ended-outbox | CAUGHT_BROADLY | HUB: ending atomically writes the ended event and advances sequence; vectors, transition ordering and rollback-on-outbox-failure all require that write. | `templates` |
| template-lifecycle-sequence | CAUGHT_BROADLY | HUB: every committed transition gets a new per-instance sequence and thus a distinct outbox `(flow_id, seq)` key. Create/extend/expire/end and epoch-preview witnesses require those commits without duplicate keys. | `basal_module`, `flow_templates`, `module_events`, `state_epoch` |
| event-verified-trigger-not-forwarded | WRONG_TEST | HUB: event activation supplies the verified body, not the notice, to the worker. GitHub notifications and CI digests need those same verified fields. | `flow_templates` |

### Why executable reuse timed out

`worker_binary()` returns a development copy; `WorkerProcess::spawn` places it
again through `dev_worker_binary`. `DevBinaries::new` can therefore have identical
source and destination paths. The mutant removes the destination before opening
the source, so this second placement fails and panics while the copy cache is
locked. The historical tail contains the resulting cache `PoisonError` failures
in `flow_scopes`; the fresh broad replay likewise exceeded the unchanged
600-second test deadline.

The next target's test
`a_cancelled_runs_write_that_dies_mid_flight_is_cleaned_up` waited on
`created_rx.recv()` for a temporary-file notification. When worker startup fails,
the driver never reaches that callback, but the host still owns its sender, so
the channel neither produces a message nor disconnects. Its receive now has a
ten-second deadline and a failure message naming the missing temporary file.
This leaves every file/record/cancellation assertion intact and does not raise
`timeout_s`. The structural executable mutation is also given a two-test unit
run set; `--broad` respects that explicit selection, so this row makes no
whole-package breadth claim.

### Deferred successful-shard warnings

These are **proposed follow-up dispositions**, not reviewed declarations that
the collateral catches share the guarded behavior, or new successful replay
claims. In this table, HUB proposes reviewing and naming that shared behavior
and its other targets; Narrow proposes limiting the source edit or run set to
exclude unrelated failures. Both proposals still need platform replays. The
exact collateral test names, primary
guards and historical target sets for every row are preserved in
[`mutation-nightly-38537440166-warnings.json`](mutation-nightly-38537440166-warnings.json).
The count/targets column identifies that row's collateral tests in the companion
file, including the large Windows and model-store sets without abbreviating
their names there. None of these 30 catalogue rows was edited here.

| Row | Platform | Collateral tests (count; targets; exact names in companion) | Proposed disposition | Reason to investigate |
|---|---|---|---|---|
| audience-workspace-compares-display-name | Linux | 1; `basal_host` | HUB | Exact workspace IDs, never display names, supply membership; compare the transport's exact-ID assertion. |
| template-lifecycle-expired-params | Linux | 1; `module_events` | HUB | Expired lifecycle bodies retain canonical template params in both deadline and event-vector tests. |
| policy-card-raised-no-longer-decodes | Linux | 51; `decisions`, `hosts`, `install_gate`, `install_policy` | Narrow | A globally malformed reply discriminator breaks unrelated fixture installation before its own oracle. |
| template-lifecycle-live-ended | Linux | 17; `flow_templates`, `module_events` | HUB | Published-template end assertions and lifecycle snapshots require ended instances to disappear from live snapshots. |
| install-request-not-dedicated | Linux | 56; `decisions`, `hosts`, `install_gate`, `install_policy` | Narrow | Replacing every install request by generic elicitation structurally breaks unrelated fixture approval; isolate the dedicated-request witness. |
| policy-shared-approval-skipped | Linux | 27; `decisions`, `hosts`, `install_policy` | HUB | Shared approval must approve the selected version and establish its schedule/disable state before runs admit. |
| template-lifecycle-body-retention | Linux | 6; `basal_module`, `module_events`, `templates` | HUB | Lifecycle bodies must remain readable until publication acknowledgement and the retention boundary. |
| interrupt-handler-never-stops-the-engine-catastrophic-regex | macOS | 4; `budgets`, `codemode`, `fuzz`, `sandbox_integrity` | HUB | The same JS CPU interrupt bounds regex, loops, fuzz payloads and wrapper execution; review the new codemode target. |
| flow-list-state-diverges-from-flow-health | macOS | 7; `install_gate`, `install_policy`, `list_contract` | HUB | List must agree with health on unapproved/revoked state; policy tests also require that reported state. |
| capture-mode-sends-calls-to-the-host | macOS | 23; `board_hygiene`, `decisions`, `dry_run`, `e2e`, `flow_templates`, `hosts`, `templates` | HUB | Capture previews execute no live effects, including board and template sinks; review the three newly red targets. |
| decision-cards-action-options-carry-an-effect-core-does-not-take | macOS | 14; `consent_paths`, `decisions` | Narrow | Invalid action effects cause core's card decoder to reject unrelated decision setup; isolate the affected card action. |
| a-card-s-approval-is-applied-as-a-rejection | macOS | 56; `audience`, `board_hygiene`, `caller_binding`, `dry_run`, `e2e`, `engine`, `health_contract`, `idle_wake`, `list_contract`, `module_lifecycle`, `ops`, `pool`, `templates` | HUB | Approval admitting a flow is the shared prerequisite; review the added board-hygiene target. |
| flow-list-agent-sees-another-agent-s-flow | macOS | 3; `list_contract`, `templates` | HUB | List visibility remains owner-scoped for ordinary flows and template instances. |
| the-owner-may-not-re-enable-even-a-disable-it-made-itself | macOS | 4; `caller_binding`, `ops`, `templates` | HUB | Owners can re-enable their own owner-disabled flows, including idempotently re-ensured instances. |
| a-local-caller-refused-an-operator-action-is-told-not-permitted-not-operator-attestation-required | macOS | 4; `caller_binding`, `module_lifecycle`, `packages`, `templates` | HUB | Local callers lack operator attestation at ordinary, package and template operation boundaries. |
| hosts-answer-acknowledged-despite-failed-decision-commit | macOS | 1; `decisions` | HUB | Decision acknowledgement must follow durable sink commit so restart can replay an uncommitted answer. |
| integration-model-store-initialization-omitted | macOS | 225; 26 targets listed in companion | Narrow | Omitting shared fixture store binding crashes many non-model oracles; retain real model-store/startup witnesses rather than blanket fixture HUB. |
| windows-allowlist-skips-alpc-close | Windows | 61; 11 targets listed in companion | Narrow | Isolate inherited-port closure from unrelated budget/replay tests that cannot start the broken worker. |
| windows-creation-drops-child-process-policy | Windows | 62; 11 targets listed in companion | Narrow | Isolate the policy/readiness refusal; unrelated execution oracles never get a ready worker. |
| windows-creation-drops-prohibit-dynamic-code | Windows | 1; `windows_launch` | HUB | Pure mitigation-bit and live launch readback both require the dynamic-code prohibition. |
| windows-worker-skips-capabilities-present | Windows | 2; `basal_worker` | HUB | The pure broken-property and check-order cases directly require capabilities-present refusal. |
| windows-worker-skips-handle-not-allowed | Windows | 1; `windows_probes` | HUB | Both inherited-handle probes require rejection of a handle outside the startup allowlist. |
| windows-worker-skips-integrity-not-untrusted | Windows | 2; `basal_worker` | HUB | Pure attestation cases directly require untrusted integrity and the corresponding first refusal. |
| windows-worker-skips-not-lpac | Windows | 1; `basal_worker` | HUB | Pure attestation and live startup require the LPAC claim and the same not-LPAC refusal. |
| windows-worker-skips-restricting-sid-mismatch | Windows | 2; `basal_worker` | HUB | Pure attestation cases require the single expected restricting SID and the same ordered refusal. |
| windows-allowlist-skips-private-file-close | Windows | 61; 11 targets listed in companion | Narrow | Isolate private-file closure from worker-startup collateral that prevents other contracts executing. |
| windows-launcher-context-names-are-predictable | Windows | 1; `windows_launch` | HUB | Random context names must not repeat or collide with leftover PID-named directories. |
| windows-worker-missing-package-sid-is-not-refused | Windows | 1; `basal_worker` | HUB | Missing package SID is its own refusal in both argument handling and the pure startup decision. |
| windows-worker-skips-group-not-deny-only | Windows | 2; `basal_worker` | HUB | Pure attestation requires enabled logon groups to be refused as not deny-only. |
| windows-worker-skips-privileges-present | Windows | 2; `basal_worker` | HUB | Pure attestation requires privileges-present refusal and its position in the check order. |

### Verification of the repair

The replay-first run used `ckdev-mutate 0.12.0`. The shared Linux image changed
its default executable to 0.12.1 while this task was running; the final gates
instead installed the repository-pinned commons revision
`be120e9254c4972191b39b2e6814987918206afc` in a private Cargo target tool root
and used that 0.12.0 executable explicitly. No dependency manifest or lockfile
was changed. For the fixture row, `select = "expected"` runs only its required
teardown oracle. `only = false` disables the claim that those required tests
are the only failures across a broader set: a single selected oracle cannot
establish package-wide exclusivity.

Catalogue anchors and exact names passed; coverage reports Linux 615, macOS
504, Windows 68, total 1,187 rows, exactly one host each. Coverage's catalogue
partition check is not a claim that all 1,187 rows were replayed in this repair.
`cargo fmt --all -- --check` and
`cargo test --locked -p basal-core -p basal-host -p basal-module -p basal-testkit --no-fail-fast`
passed on Linux: 1,159 tests across 103 test/doctest targets (92 nonempty),
including 42 template tests and the bounded write test.
Tools: Cargo 1.99.0, rustfmt 1.10.0, Python 3.14.4. File inspection could not
start rust-analyzer because its worker process capacity was exhausted; the
compiled crate suites provide the authoritative Rust diagnostics instead.

Each assigned row then ran with
`env -u BASAL_WORKER_BIN -u BASAL_CUT_EXHAUSTIVE ckdev-mutate run --only <id> --broad --allow-dirty --report <temporary-report>`,
using the explicit pinned executable on Linux with eight vCPUs. All baselines
were green. One interim expiry-deadline replay still warned because its HUB
annotation had been placed on the adjacent expired-params row; moving the
annotation to the intended row and replaying expiry-deadline yielded HUB.
The expired-params row is unchanged from
`1d15cdc3b59c8ef185d3528ced62ccaccf7b8d5b`, keeping that deferred warning out of
the repair's changes. Final outcomes are
**15 HUB and 3 CAUGHT**, with no remaining WRONG_TEST, timeout or warning among
the assigned rows. The three selected run sets explicitly disclaim broad
coverage; the other fifteen actually exercised package-wide breadth.

| Row | Final outcome | Red / green tests |
|---|---|---:|
| template-params-activation | HUB | 37 / 402 |
| template-one-shot-ends | HUB | 19 / 420 |
| template-dry-run-self-end-not-captured | HUB | 2 / 437 |
| template-dry-run-no-instance-or-quota | HUB | 5 / 434 |
| template-missing-stat | HUB | 4 / 435 |
| template-canonical-path-bounds | HUB | 3 / 436 |
| host-workspace-double-management-result-unwrap | HUB | 3 / 143 |
| digest-replace-key-catalog-capability-gate | HUB | 3 / 436 |
| test-module-fixture-cleanup-drops-module-first | CAUGHT (expected-only) | 1 / 0 |
| template-dry-run-effective-grants | CAUGHT (six-test selection) | 5 / 1 |
| content-addressed-copy-is-not-rewritten | CAUGHT (two-test selection) | 1 / 1 |
| codemode-content-result-error-unmarked | HUB | 5 / 434 |
| call-modes-codemode-catalog-serves-flow | HUB | 9 / 430 |
| template-lifecycle-expiry-wake | HUB | 2 / 437 |
| template-lifecycle-expiry-deadline | HUB | 7 / 432 |
| template-lifecycle-ended-outbox | HUB | 5 / 434 |
| template-lifecycle-sequence | HUB | 31 / 408 |
| event-verified-trigger-not-forwarded | HUB | 17 / 422 |

Three additional negative controls restored the saved live state from Git's
index: the intended changes and each mutation target were staged first, the worktree diff was
empty, each break produced a nonempty `2 +-` diff, and
`git checkout -- <path> && touch <path>` restored an empty diff. No temporary
source break is committed.

* Suppressing only the temporary-file notification while retaining its sender
  made `a_cancelled_runs_write_that_dies_mid_flight_is_cleaned_up` fail with
  `the write must reach its temporary file within ten seconds: Timeout`.
  All six other `fs_write_temps` tests stayed green; the suite finished in
  11.52 seconds. This demonstrates that the new deadline is exercised, not
  merely present in source.
* Applying the original executable-rewrite mutation to all five binary unit
  tests failed only
  `binaries::tests::pre_existing_complete_copy_is_reused_without_rewriting`.
  Changed bytes, copies outside the build folder, module/worker placement and
  repeated path/inode placement stayed green. The timestamp assertion compared
  a new modification time with the deliberately old one-second timestamp.
* Running the original executable-rewrite mutation against the single formerly
  hanging write test produced `place development executables: NotFound` in its
  driver, then the named ten-second receive failure. It finished in 10.24
  seconds, with six other tests filtered out. This confirms the executable
  mutation actually reaches the missing-notification path described above.

The notification-control green witnesses were:
`a_crash_mid_write_is_cleaned_up_by_recovery_without_following_a_swapped_path`,
`a_crash_record_whose_directory_left_its_root_is_not_acted_on`,
`a_resent_write_replaces_the_file_its_earlier_send_left`,
`a_successful_write_leaves_no_record_and_no_temporary_file`,
`legacy_temporary_files_are_removed_once_from_written_directories`, and
`remove_temp_removes_only_a_recorded_regular_file`.
The binary-control green witnesses were:
`binaries::tests::changed_binary_bytes_use_a_different_directory`,
`binaries::tests::copies_never_run_from_their_build_folder_whatever_the_name`,
`binaries::tests::module_and_worker_share_a_content_addressed_development_directory`, and
`binaries::tests::repeated_placement_reuses_the_same_file_and_inode`.

The binary unit tests and all seven write-temp tests passed again after
restoration. The sidekick comment review examined all ten text/code files;
the historical JSON was correctly skipped as data without comments. Its
clarity findings were addressed by defining HUB and explaining normalized,
journaled parameters and the deferred proposals' unreviewed status.
