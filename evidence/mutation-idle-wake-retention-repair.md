# Idle-wake and retention mutation repairs

Three catalogue rows failed in the full CI replay using commit
`585a6ea94347fdaa52a462a48d351f79088c6d33`. The causes below were identified
before editing. Source line references in the cause sections refer to that
commit; the fixes are test and catalogue changes only. No production guard,
query-plan assertion, or catalogue row was removed or weakened.

All three repaired rows replayed as **CAUGHT** with `ckdev-mutate 0.9.5`
built from the shared runner in the commons repository at revision
`73c7e66145e131eadffdd874c82d93548868b668`, on their configured
platforms. The decision-card mutation anchors were unchanged.

## Failed answer must not be acknowledged

Catalogue control (`mutations.toml`): `decision-cards-a-page-is-acknowledged-although-applying-its-answer-failed`.

**Cause:** The crash test cut the store before calling `poll_once`
(`crates/basal-module/tests/decisions.rs:702-714`). A cut fails both reads and
writes (`crates/basal-core/src/store.rs:163-184`). Polling now checks
`has_open_cards` before fetching a page, unless retry debt is already set
(`crates/basal-host/src/core_consent.rs:387-409`). Thus the expected error and
unchanged acknowledgement count could be satisfied by the inventory read
failing, without reaching `sink.answer`. The mutant still disables exactly the
apply-error handling at `core_consent.rs:487-494`; its anchor had not moved.

**Fix:** A test-only sink delegates the open-card read to the real runtime,
then cuts that runtime's store inside `answer`, immediately before invoking
`Runtime::answer_decision`. The test now requires a fetched page, a cut store,
an error identifying answer application, and no acknowledgement. Restart still
has to recover the open card, apply its answer, and record exactly one effect
and one audit entry. A deliberately failed acknowledgement after recovery
leaves retry debt, so the subsequent duplicate page is also actually fetched.

**Proof:** The macOS replay failed only
`a_crash_between_receiving_an_answer_and_applying_it_applies_it_exactly_once`;
the other 12 tests in `basal-module --test decisions` stayed green. The failure
was `called Result::unwrap_err() on an Ok value: ()`: swallowing the apply
failure incorrectly allowed acknowledgement and a successful poll.
An additional exact-filter proof failed the same named test (one failure,
12 tests filtered out).

## Redelivered answer must not be applied twice

Catalogue control (`mutations.toml`): `decision-cards-an-answer-delivered-twice-is-applied-twice`.

**Cause:** The duplicate test successfully applied and acknowledged the first
answer, then delivered copies and polled again
(`crates/basal-module/tests/decisions.rs:526-545`). The last card was no longer
open and no retry debt remained, so polling returned before reading those
copies (`crates/basal-host/src/core_consent.rs:394-409`). Even its old
acknowledgement assertion could pass using the earlier install and answer
acks. The mutant's open-state guard at
`crates/basal-core/src/decisions.rs:708-721` was still the right anchor.

**Fix:** Lose the acknowledgement of the first successfully applied answer.
Verify the card is closed, deliver duplicates, and require exactly one new
page fetch and acknowledgement. The existing post count, run state, and
single-audit assertions remain. This uses the production retry-debt rule,
rather than bypassing the idle-poll gate. The crash recovery test also exercises
this redelivery rule.

**Proof:** The macOS replay failed `a_duplicate_answer_changes_nothing`
(**4 audit entries instead of 1**). It also failed the repaired crash-answer
test, which observed two extra `decision.stale` audit entries. The other
11 tests stayed green. The row already permits collateral catches
(`only = false`), so this second catch did not require a catalogue change.
An additional exact-filter proof failed only `a_duplicate_answer_changes_nothing`
(one failure, 12 tests filtered out).

## Refusal intent must keep its terminal run

Catalogue control (`mutations.toml`): `retention-keeps-refusal-intents`.

**Cause:** The mutant appends constant false to the refusal obligation in the
shared SQL predicate (`crates/basal-core/src/retention.rs:59-65`). The plan test
uses that same predicate to construct the production candidate query
(`crates/basal-core/src/schema_retention_tests.rs:210-214`) and requires a
`SEARCH` through `outbox_refusal_run` (`schema_retention_tests.rs:233-274`).
Under the mutant, SQLite instead reports a non-correlated `SCALAR SUBQUERY`
with `SCAN o`, so the indexed refusal lookup is absent. This is a legitimate
second catch of a changed shared query, not a fragile assertion on an unchanged
query. The behavioral regression independently requires the run to survive
until refusal delivery (`crates/basal-core/src/runtime_regressions.rs:741-765`).

**Fix:** Keep the SQL mutant and both guards; list
`schema::retention_tests::outbox_refusal_run_plan` alongside
`runtime::regressions::pruning_preserves_a_terminal_run_until_its_refusal_acknowledgement_is_delivered`
in the row's `expect_red` list in `mutations.toml`. Keep that row's
`only = true` setting, requiring exactly those two failures and no others.

**Proof:** The Linux replay failed exactly those two named tests; the other
61 `basal-core --lib` tests stayed green. The behavior failure was
`provider snapshot was pruned before its acknowledgement`; the plan failure
showed `SCAN o` instead of the required index search. Separate exact-filter
runs failed each named test independently (one failure and 62 filtered tests
per run).

## Replay and restoration

For each row, the replay command was:

```sh
ckdev-mutate run --allow-dirty --only <row-id> --report target/mutations/<row-id>.json
```

The retention command ran on Linux; the two decision-card commands ran on
macOS. `--allow-dirty` allowed the staged test/catalogue repairs to be tested
before committing. Baselines were green in every successful reported replay.
The first failed-answer replay caught its mutant but could not write a report
because `target/mutations` did not yet exist; creating that directory and
replaying produced the successful report.

For every additional exact-filter proof, the live files were staged and
`git diff --stat` was empty before mutation. Each mutant was marked
`NON-VACUITY BREAK` and produced a nonempty diff: one source file, one insertion
and one deletion. After the check, `git checkout -- <mutated-path>` followed by
`touch <mutated-path>` restored the indexed live state; `git diff --stat` was
empty again. The mutated paths were `crates/basal-host/src/core_consent.rs`,
`crates/basal-core/src/decisions.rs`, and `crates/basal-core/src/retention.rs`.
The retention mutation was kept in place for its two independent filtered
checks, then restored. No mutant remains in the delivered sources.
