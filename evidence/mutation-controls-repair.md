# Mutation control repairs

Five catalogue rows stopped catching their mutants when the catalogue was first
replayed in full on CI. Four mutants survived and one no longer compiled. This
note records why each row went vacuous and how it was repaired. No row was
removed, and no production guard was changed. Every repaired row was proved
CAUGHT with `ckdev-mutate` 0.9.5 (commons
`73c7e66145e131eadffdd874c82d93548868b668`), the revision CI pins.

## Root causes, fixes, and catches

### broca-acknowledged-completion-is-delivered-again

**Reproduction:** `SURVIVED` on Linux, with the original row and runner 0.9.5.
The baseline executed the real `broca` integration target, not a macOS-only
worker path.

**Cause:** `BrocaHost::poll` now enumerates `StateStore::pending_ids`, rather than
all saved calls. The trait default and `MemoryStore::pending_ids` both exclude
acknowledged and deferred calls (`crates/basal-host/src/broca/mod.rs:122-129`,
`crates/basal-host/src/broca/fake.rs:375-381`). The durable store uses the same
predicate in `crates/basal-core/src/broca.rs:10-14`. Removing only the acknowledged
check in `poll` cannot deliver an acknowledged call: it never enters that loop.
The test already counts delivery attempts, including duplicates; the test was
not weakened and is not cfg-gated by platform.

**Fix:** Re-anchor the row in `BrocaHost::advance`, disabling the persisted
`call.acknowledged = true` after sink acceptance. Include the following
`self.store.save(call)?` in the anchor to distinguish this acknowledgement from
an unsent refusal's acknowledgement. The mutant leaves accepted completions
pending across polling/restart, breaking exactly the once-only delivery claim.

**Proof:** Linux `ckdev-mutate run --allow-dirty --only
broca-acknowledged-completion-is-delivered-again` returned `CAUGHT`.
`a_module_restart_with_calls_in_flight_delivers_each_exactly_once` observed
**8 attempts instead of 3**. The full 19-test target also caught the mutant in
`unknown_run_for_an_accepted_call_is_reported_as_unknown_not_rejected`; 17 tests
remained green. An additional exact-filter hand proof failed only the named
restart test (18 others filtered out).

### flow-route-cache-is-not-reused

**Reproduction:** `SURVIVED` on Linux with the original row and runner 0.9.5.
The baseline ran the one real test in `flow_scope_routes`; it has no platform cfg.

**Cause:** The row correctly suppresses `ScopedRoutes::prepare`'s cache hit,
but `ScopedRoutes::install` now deduplicates again after opening outside the
route-table lock (`crates/basal-host/src/flow_scope.rs:230-244,254-291`). The
original test supplied a new handle from its second opener and only asserted
that the returned handle was the old one. Installation returned that old handle
even after the unnecessary open, hiding the resource/cache regression.

**Fix:** Keep the existing mutant and handle/selector assertions. Make the
second opener panic if invoked: cached reuse must not open another route, not
merely return the previous handle after doing so.

**Proof:** Linux runner returned `CAUGHT`; only
`route_cache_reuses_handles_and_replaces_changed_selectors` failed, with
`a cached route must not be opened again`. The exact-filter hand proof produced
the same failure. Epoch and replacement-selector assertions remain intact.

### broca-value-can-masquerade-as-usage-metadata

**Reproduction:** Original row returned `DID_NOT_COMPILE` on macOS with 0.9.3.
Its compiler output was `error[E0425]: cannot find value 'value' in this scope`
at `crates/basal-core/src/tokens.rs:398`. All four baseline reservation tests
passed.

**Cause:** `tokens::settle_outcome` reads `outcome.usage` and parses
`outcome.value` only inside a rejected/unavailable match guard
(`crates/basal-core/src/tokens.rs:389-412`). There is no outer `value` binding for
the old fallback mutant to use. This is stale mutant text, not an absent test.

**Fix:** Rewrite only the fallback mutant to parse `outcome.value.as_str()` as
`Value`, extract its `usage` property, and deserialize it as token usage when
trusted metadata is absent. The production metadata-only settlement code and
the existing reservation assertions are unchanged.

**Proof:** macOS 0.9.5 replay returned `CAUGHT`; only
`script_value_cannot_supply_usage_and_absent_ledger_fields_are_nullable` failed
at the token-ledger assertion (**556 versus 0**). All three other reservation
tests stayed green. An exact-filter hand proof failed at the same assertion.

One initial updated replay returned `ERROR`: its test binary received SIGKILL
before identifying any running test. The binary passed `codesign --verify`,
and source was confirmed restored. A single retry succeeded; this transient
launch failure was not counted as a catch and did not prompt any guard changes.

### net-a-non-https-url-is-fetched

**Reproduction:** `SURVIVED` on macOS with the original row and runner 0.9.5.

**Cause:** The test's HTTP URL specified the server's ephemeral port. Disabling
`parse_url`'s HTTPS check still led to refusal by the independent port-443 policy
in `allowed_url` (`crates/basal-host/src/builtins/net.rs:164-192,277-287`).
Thus `DENIED` plus zero connections proved either of two refusals, not HTTPS
policy alone. The test's `StaticResolver` already maps the approved host to the
local server socket (`crates/basal-testkit/tests/builtins_net.rs:110-122`).

**Fix:** Keep the mutant, denial assertion, and zero-connection assertion.
Use `http://api.test/hello`, without an explicit port, so the production parser's
otherwise-approved default port 443 cannot mask a missing scheme check; the
resolver continues to connect only to the local TLS server.

**Proof:** macOS replay returned `CAUGHT`; only `only_https_urls_are_fetched`
failed: it expected a refusal but received a **200 response with body 'hello'**.
All 13 other network tests stayed green. The exact-filter hand proof produced
the same failure.

### pruning-ignores-open-obligations

**Reproduction:** `SURVIVED` on macOS with the original row and runner 0.9.5.

**Cause:** The named test now leaves all runs pending or suspended while calls
are outstanding. Its failed script remains suspended until its long call settles
(`crates/basal-testkit/tests/journal_admin.rs:64-139`). The candidate query and
transactional recheck admit only terminal runs (`retention.rs:71-78,132-159`),
so status alone protected every outstanding call. The old mutant disables only
the unsettled-journal branch of `OBLIGATIONS`, not the other OR branches
(`retention.rs:59-65`). It never exercised that branch on a terminal run.

**Fix:** Add a core regression that directly constructs a failed terminal run
with just one unsettled journal call, verifies pruning keeps it and reports it
unsettled, then accepts the outcome and verifies it becomes prunable. No mailbox,
reserved token ledger, open decision card, Broca snapshot, or refusal outbox row
can mask the journal obligation. Keep the original unfinished-run test intact.
Point this row to `basal-core --lib` and the new exact test. Its platform becomes
Linux, as required by the existing CI partition rule for all basal-core proofs.
The SQL mutant itself is unchanged.

**Proof:** Linux replay returned `CAUGHT`.
`runtime::regressions::pruning_keeps_a_terminal_run_with_an_unsettled_journal_call`
failed with `PruneReport { pruned: ["run"], kept_unsettled: [] }`. The full
63-test lib target also failed `schema::retention_tests::quarantine_obligation_plan`
because the disabled branch vanished from the SQL plan; 61 tests remained green.
The exact-filter hand proof failed only the new semantic regression (62 others
filtered out), so the catch does not depend on that incidental query-plan failure.

## Platform and worker-artifact findings

The two original Linux rows survived because of the redundant pending filter and
install-time deduplication described above, not because tests were compiled out.
Linux baseline and mutant replays both ran their named tests. Their final proofs
were performed with the real Linux runner, not a copied macOS catalogue.

None of these five mutants changes worker code. `basal-worker` depends on
`basal-proto`, libc, and rquickjs, not basal-core or basal-host
(`crates/basal-worker/Cargo.toml:10-16`). The mutated host/core functions run in
the parent test/runtime process and rebuilt in the mutant runs. A stale embedded
worker cannot explain these outcomes. The testkit build script also declares
rerun inputs for the entire worker and proto directories before building its
separate worker artifact (`crates/basal-testkit/build.rs:13-46`). No global worker
invalidation gap was found in this investigation; a full worker-row audit was
not run.

## macOS entropy test reproduction

The additionally requested 20-run loop stopped on its **first baseline attempt**
at `production_random_is_not_the_same_first_draw_after_restart`'s `child draw`
expectation. The child exited successfully: it was neither killed nor missing,
and this code has no child timeout. Running the child directly showed:

```text
running 1 test
test production_random_is_not_the_same_first_draw_after_restart ... ENTROPY_BITS=4600883758265805594
ok
test result: ok. 1 passed; 0 failed
```

The parser incorrectly required the marker to begin a line, although libtest
with `--nocapture` may print its test-name prefix on that same line. Locate the
marker anywhere in the line and parse its value as `u64`. Preserve the child
success check and the unequal-draw assertion, and include successful-child stdout
in the missing-draw diagnostic.

After the fix, all **20 of 20** macOS repetitions passed. Replacing OS entropy
with the existing predictable-counter mutant failed the **unequal-draw assertion**
on macOS with identical bits `4603741974828149071`; it did not fail the parser.
The unchanged `production-random-restarts-a-predictable-counter` catalogue row
also returned `CAUGHT` on Linux, with only that test red and the other ten green.

## Final verification and isolation evidence

- `ckdev-mutate check` (0.9.5): catalogue anchors and exact test names verified.
- `python3 .github/workflows/check_mutation_coverage.py` on Linux (Python 3.14.4):
  **109 Linux + 431 macOS = 540 rows**, each owned by exactly one platform.
  The Mac's default Python 3.9.6 cannot import stdlib `tomllib`; that environment
  failure was not a pass. The Linux coverage gate passed after assigning the
  moved basal-core row to Linux.
- `cargo fmt --all --check` on Linux (rustfmt 1.10.0-stable): passed after fixing
  the entropy diagnostic's formatting.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` and
  `cargo clippy --workspace --all-targets --locked --features basal-module/rig-kill-hook -- -D warnings`
  on Linux (clippy 0.1.99): both passed, checking all targets of all seven workspace
  packages. These are also the authoritative Rust typechecking gates for this edit.
- `cargo test -p basal-host --test broca --test flow_scope_routes --test host_boundary_regressions --locked`
  on Linux: **19 + 1 + 11 = 31 passed** after restoring mutants.
- `cargo test -p basal-core --lib --locked` on Linux: **63 passed** after restore.
- `cargo test -p basal-host --test host_boundary_regressions --locked` on macOS:
  **11 passed** after restoring the entropy mutant.
- `cargo test -p basal-testkit --test broca_reservations --test builtins_net --locked`
  on macOS: **4 + 14 = 18 passed** after restoring mutants.
- macOS entropy repetition command:
  `for attempt in $(jot 20); do cargo test -p basal-host --test host_boundary_regressions --locked production_random_is_not_the_same_first_draw_after_restart -- --exact || exit $?; done`:
  **20 passed**, one test per run.
- The five final row replays all used
  `ckdev-mutate run --allow-dirty --only <row-id> --report <report-path>` with 0.9.5;
  three ran on Linux and two on macOS as specified above. `--allow-dirty` admitted
  the staged implementation; baseline tests were still required to pass.
- All six additional hand proofs used the exact named test filter plus
  `-- --exact`; each ran one test and failed that test only. For every proof,
  the specific live files were staged first, `git diff --stat` was empty before
  mutation, a `NON-VACUITY BREAK` was applied, and the following nonempty diff was
  captured. Restoration used `git checkout -- <path> && touch <path>` and another
  empty `git diff --stat`. No hand mutant remains in the delivered tree.

| Mutated path | Nonempty diff during proof | Diff after restore |
| --- | --- | --- |
| `crates/basal-host/src/broca/mod.rs` | 1 file, 1 insertion, 1 deletion | empty |
| `crates/basal-host/src/flow_scope.rs` | 1 file, 1 insertion, 1 deletion | empty |
| `crates/basal-core/src/retention.rs` | 1 file, 2 insertions, 1 deletion (including marker) | empty |
| `crates/basal-core/src/tokens.rs` | 1 file, 1 insertion, 1 deletion | empty |
| `crates/basal-host/src/builtins/net.rs` | 1 file, 1 insertion, 1 deletion | empty |
| `crates/basal-host/src/core_host.rs` | 1 file, 4 insertions, 11 deletions | empty |

AFT inspection was partial because the checkout call graph was unavailable and
rust-analyzer was still indexing/checking; it was not treated as a clean
snapshot. The complete clippy/typechecking and relevant test gates above passed.
No workspace-wide test run or full 540-row replay was attempted. Beyond the five
requested rows, only the entropy row affected by the additional requested test
repair was replayed; it was caught. No other row was silently repaired or had its
grading compared between old and new runners. Package manifests and Cargo.lock
were unchanged.
