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

Every changed row was replayed in a single session per host with
`ckdev-mutate run --diff 21b03f4`. That selects exactly the rows whose catalogue
entry changed, and each row runs the same replay that `--only <id>` would run.
The sessions then ran again with `--broad`.

- Linux (remote build server, ckdev-mutate 0.9.5): the plain replay graded
  all 7 Linux rows CAUGHT, and the `--broad` replay graded all 7 HUB. In the
  plain replay, none of the six rows that used to carry `only = true` reddened
  any test in its own target besides the named one.
- macOS (local): the plain replay graded all 27 macOS rows CAUGHT, and the
  `--broad` replay graded all 27 HUB. The four rows that used to carry
  `only = true` showed no other red test in their own target.
- `flow-scope-fixture-keeps-the-production-activation-deadline`: macOS
  `run --only` CAUGHT, as above. With `--broad` it is also CAUGHT, and across
  the package's 206 tests only the named test went red, so its `only = true`
  holds there too.
