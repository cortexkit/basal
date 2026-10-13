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
