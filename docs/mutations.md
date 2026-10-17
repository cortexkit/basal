# Mutation proofs

A passing safety test proves little until it has been seen to fail. [`mutations.d/`](../mutations.d) is the checked-in catalogue of source edits and the exact full libtest paths that must catch them, one TOML file per crate or area. The shared `ckdev-mutate` runner builds each mutant separately, runs its target's tests, requires the named test to fail, and restores the saved source bytes and checks `Cargo.lock`. Each row names, in `expect_red`, the guard tests that must fail. With `only = false` other tests may also catch the mutant; with `only = true` the named tests must be the only failures. Install the reviewed, immutable revision (ckdev-mutate 0.12.4):

```sh
cargo install --locked --git https://github.com/cortexkit/commons --rev 85c8b577795d556fb27e54b548f06ea913d8b337 cortexkit-mutate
mkdir -p target/mutations
ckdev-mutate check
ckdev-mutate run --all --report target/mutations/all.json
ckdev-mutate run --diff origin/main --report target/mutations/diff.json
ckdev-mutate run --only worker-leaves-inherited-descriptors-open --report target/mutations/one.json
```

Run from a clean tree with `BASAL_WORKER_BIN` and `BASAL_CUT_EXHAUSTIVE` unset, so the tests build the edited worker and use their normal scope. Build and test deadlines are separate: rows allow 3600 seconds to build on a loaded host and 600 seconds to run the tests, unless a row extends the test bound for many stalled tests. A successful replay reports every row `CAUGHT`; compilation errors, missing anchors, missing tests, failures of other tests without the named guard failing, and timeouts are not catches. Never check out an edited file while a replay is running. Reports stay under gitignored `target/mutations/` locally and are uploaded as CI artifacts; benchmark measurements remain in [`evidence/`](../evidence/README.md).

Each proof has one CI host, set by its row's `platforms`. Worker-using packages run on macOS (Seatbelt) or Linux (seccomp and Landlock); rows editing Linux-only confinement code run on Linux, and rows editing Windows-only modules run on Windows. Portable crates and offline command fixtures run only on Linux, so they are not replayed twice. Offline signing and debugger probes replace macOS tools with fixtures rather than invoking codesign or lldb. `python3 .github/workflows/check_mutation_coverage.py` enforces the one-host rule.

For PR replay, `ckdev-mutate run --diff origin/main --list` prints the selected IDs without building or running tests. CI skips the replay when that list is empty and writes an empty JSON report for the remaining artifact and witness steps.

## Catalogue files

The runner reads every visible `*.toml` file directly inside `mutations.d/`, in filename byte order; other files and subdirectories are ignored. A `mutations.toml` beside the directory is an error, so there is no single-file copy. Files are named for the crate or area their rows edit: `worker.toml`, `launch.toml`, `proto.toml`, `module.toml`, `rig.toml`, `testkit.toml` and `scripts.toml` for whole crates and `script/`; `flows.toml` for the checked-in flows and templates under `flows/` and `templates/`; `core-*.toml` and `host-*.toml` for areas of basal-core and basal-host, such as `core-journal.toml`, `core-codemode.toml` and `host-builtins.toml`. Add a row to the file for the area of the source it edits, or start a new file for a new area. Row ids must be unique across all files; `ckdev-mutate check` refuses a duplicate and names every file defining it. Runner reports, `check` errors and the repository's catalogue scripts name the file and line of each row they report.

Moving rows between files is not a change to any proof, and `ckdev-mutate run --diff` does not select a row only because it moved. `python3 script/mutation_catalogue.py --same-rows-as <revision>` compares every row with the catalogue at a Git revision, field for field, and fails on any added, removed or changed row, so a reorganisation can show it moved rows without editing them.

Rows can narrow two things the runner otherwise applies to a whole file or a whole target; the catalogue uses both `scope` and `select`:

- **Anchor scope.** `scope = { after = "fn verify_signature(", before = "\nfn " }` bounds where `old` may match. `after` must occur exactly once in the file, and the region starts right after it; the region ends at the first `before` after that, or at the end of the file when `before` is omitted. `old` must occur exactly once inside the region, though it may appear elsewhere in the file. Use it when the anchor text could also appear in another function: a file-wide anchor that still matches once after a refactor could otherwise edit the wrong code. `check` reports a missing or ambiguous boundary, or an `old` that is not unique in the region. A row has one scope, so all its `edits` must be in one file. `prove --scope-after <text> [--scope-before <text>]` writes it.
- **Run set.** `select = { filter = "trust_roster_" }` runs only the target's tests whose full name contains that literal text (not a regex), and `select = { tests = ["tests::a", "tests::b"] }` runs exactly the listed full names. The set must contain every `expect_red` name and at least one other test that executes; otherwise use `select = "expected"`. With `only = true`, a failure of any other test in the set grades `WRONG_TEST`; tests outside the set do not run, and `--broad` does not widen it. Command rows (`runner = "command"`) and HUB rows (rows with `hub` and `hub_targets`, described below) cannot use a run set. `prove --select-filter <text>` or `prove --select-tests <a>,<b>` writes it.

To add a guard, first resolve its full test name with `cargo test -p <package> --test <target> --locked -- --list` (or `--lib` for a unit test). Then prove an exact-once source edit. For example, this existing proof shows the command shape; choose a new unique ID and a new mechanism for a new row:

```sh
ckdev-mutate prove --id worker-closes-descriptors-proof \
  --guards 'worker leaves inherited descriptors open' \
  --file crates/basal-worker/src/confinement.rs \
  --old 'for fd in macos::open_descriptors().map_err(ConfinementError::Descriptors)? {' \
  --new 'for fd in Vec::<i32>::new() { // NON-VACUITY BREAK' \
  --test-file crates/basal-worker/tests/inherited_descriptors.rs \
  --package basal-worker --target='--test inherited_descriptors' \
  --expect-red worker_closes_extra_inherited_descriptors_at_startup --only \
  --build-timeout-s 3600 --timeout-s 600 --report target/mutations/proof.json
```

`prove` writes a row only when it is caught, as a new `mutations.d/<id>.toml`. Inspect the row, move it unchanged into the file for its area, run `ckdev-mutate check`, and commit the source, guarding test and catalogue together. The [pinned runner's README](https://github.com/cortexkit/commons/blob/85c8b577795d556fb27e54b548f06ea913d8b337/crates/cortexkit-mutate/README.md) documents multi-file edits, anchor scopes, run sets and survivor diagnosis. When a manifest edit changes dependency resolution, resolve it without `--locked` in a scratch copy and include the resulting `Cargo.lock` changes in `edits`; the runner restores and byte-verifies the lockfile like any other edit target. Rows guarding the rig script's offline Python tests use `runner = "command"`: prove one with `--runner command --test-count-pattern 'Ran {count} test' --expect-red script.tests.flows_rig.RigChecks.<test> --command python3 -m unittest '{test}'`, putting `--command` last because it consumes everything after it. The runner first checks that the named test passes on the unmutated tree, then that it fails on the mutant. PR CI replays rows touched by the committed diff against `origin/main`; pushes to main replay the entire catalogue in isolated shards. A nightly run replays it with `--broad`, against every test target in each row's package, and grades a control CAUGHT_BROADLY when its mutant also breaks tests in another target. A HUB row is a control whose mutant is expected to break tests in other targets too, because they assert the same property; reviewed HUB rows name that shared property in `hub` and list only those other targets in `hub_targets`. The broad replay accepts only that set or a subset, warning on any new target. Structural breaks must be narrowed instead of labelled HUB. Diff selection cannot see a changed helper or fixture that is neither an edit target nor `test_file`, so the full replay remains necessary.
