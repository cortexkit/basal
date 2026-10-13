# basal

basal runs approved automation scripts and provides the `codemode` tool for scripting an agent's tools. An automation flow reacts to an event or a schedule, reads facts and module data, decides what to do, and sends its output to an agent through prefrontal-core, the module that manages agents and their output destinations. The operator must approve each exact script version: an edit requires a new approval.

The name comes from the basal ganglia, the part of the brain that selects actions and runs habits and routines.

basal runs as a module supervised by the subc daemon, with the binary `ck-basal`. Scripts run in a separate, confined QuickJS worker, `ck-basal-worker`, that holds no durable state: the parent process checks every call a script makes against its approved manifest, journals it before it leaves and records its outcome when it comes back, so a crash anywhere resumes the run without repeating an effect.

## Codemode

Basal provides one agent-facing tool, `codemode({code, description?, limits?})`, on its tool route, where the daemon verifies the caller's principal and stamps its agent and scope identities. A short JavaScript program can await the agent's own tools, capture console output and return one JSON result. prefrontal-core, which manages agents and their tool grants, grants access to codemode and opens the run scope; basal owns the description, run records, budgets and rendered result. Tool schemas come from the daemon catalog, and each provider decides allow, ask and deny using the daemon-recorded agent identity and run scope, just as for a direct call. Model tools are a later extension.

A keyed call derives its run ID from the stamped agent and call key; retries attach without replay. A keyless call executes once under an OS-random ID and warns that its reply cannot be recovered. Basal's parent process (outside the confined worker) caps wall time at 30 minutes plus a separate cumulative 10 minutes waiting only on people: the worker must be blocked and every in-flight tool call waiting for a person's approval or decision. It closes the scope before cancellation kills its worker, and interrupts leftover runs at startup. Long keyed calls can return before the program finishes; `late_results` lets the caller retrieve the finished result later, and `tool.withdraw` cancels the run. Codemode runs on macOS, Linux and Windows under each OS's full worker confinement. See [the tool and role operations](docs/ops.md#codemode).

## Crates

| Crate | What |
|---|---|
| `basal-proto` | The versioned IPC protocol between the parent and its worker processes. |
| `basal-worker` | `ck-basal-worker`: the confined QuickJS engine. Before reading anything it checks its confinement: Seatbelt on macOS, seccomp and Landlock on Linux, and the launcher's restricted LPAC token, job and mitigations on Windows. See [docs/sandbox.md](docs/sandbox.md). |
| `basal-core` | Runtime state: the SQLite store, admission, the run state machine, the journal and mailbox, manifests and authorization, the scheduler and the activation driver. |
| `basal-host` | The `Host` boundary that flow calls go through (module ops, facts, model calls, sinks), the adapters to the fleet's modules, and a deterministic mock. |
| `basal-module` | `ck-basal`: the supervised module, with its subc manifest, the codemode tool-provider role, the worker pool, the flow ops, dry runs and consent cards. |
| `basal-launch` | Windows process creation: restricted LPAC workers and job-owned child processes, with a shared spawn lock. |
| `basal-testkit` | Test parents, crash and cut harnesses, and benchmarks. |
| `basal-rig` | Test support for the isolated ckdev-flows rig only: a callosum stub and the live contract suite against a real prefrontal-core. Never deployed. |

## Building and testing

basal needs Rust 1.88 or newer (edition 2024) on macOS, Linux (x86_64 or aarch64), or Windows. CI runs natively on both Windows runner images, windows-latest and windows-2022. Dependencies are published on crates.io or pinned to an immutable Git revision.

```sh
cargo build --workspace
cargo test --workspace --locked
cargo test --workspace --locked --features basal-module/rig-kill-hook
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings
shellcheck -x script/*.sh
```

CI also refuses Cargo path dependencies that resolve outside this repository and test or script launches under production `ck-` file names. `python3 script/check-ckdev-names.py` checks the latter boundary; tests run hard-linked (or copied) `ckdev-` executables so Activity Monitor distinguishes them from the placed fleet.

The `rig-kill-hook` feature adds a one-shot kill switch used only by the test rig; a production build never enables it.

Mock hosts, fake providers and test-only runtime helpers require `cfg(test)` or
the owning crate's `test-support` feature. Development dependencies enable that
feature for `cargo test`; production builds do not enable it. The harness and
testkit binaries also require `test-support` when built by hand, for example
`cargo build -p basal-module --bin ck-basal-harness --features test-support`.
Production staging rejects the retained test-support marker as well as the rig
kill-hook marker, even if the linker removes unused test doubles.

## Mutation proofs

A passing safety test proves little until it has been seen to fail. [`mutations.d/`](mutations.d) is the checked-in catalogue of source edits and the exact full libtest paths that must catch them, one TOML file per crate or area. The shared `ckdev-mutate` runner builds each mutant separately, runs its target's tests, requires the named test to fail, and restores the saved source bytes and checks `Cargo.lock`. The existing proofs name one guard each; other tests may also catch the mutant (`only = false`). Install the reviewed, immutable revision (ckdev-mutate 0.12.0):

```sh
cargo install --locked --git https://github.com/cortexkit/commons --rev be120e9254c4972191b39b2e6814987918206afc cortexkit-mutate
mkdir -p target/mutations
ckdev-mutate check
ckdev-mutate run --all --report target/mutations/all.json
ckdev-mutate run --diff origin/main --report target/mutations/diff.json
ckdev-mutate run --only worker-leaves-inherited-descriptors-open --report target/mutations/one.json
```

Run from a clean tree with `BASAL_WORKER_BIN` and `BASAL_CUT_EXHAUSTIVE` unset, so the tests build the edited worker and use their normal scope. Build and test deadlines are separate: rows allow 3600 seconds to build on a loaded host and 600 seconds to run the tests, unless a row extends the test bound for many stalled tests. A successful replay reports every row `CAUGHT`; compilation errors, missing anchors, missing tests, failures of other tests without the named guard failing, and timeouts are not catches. Never check out an edited file while a replay is running. Reports stay under gitignored `target/mutations/` locally and are uploaded as CI artifacts; benchmark measurements remain in [`evidence/`](evidence/README.md).

Each proof has one CI host, set by its row's `platforms`. Worker-using packages run on macOS (Seatbelt) or Linux (seccomp and Landlock); rows editing Linux-only confinement code run on Linux, and rows editing Windows-only modules run on Windows. Portable crates and offline command fixtures run only on Linux, so they are not replayed twice. Offline signing and debugger probes replace macOS tools with fixtures rather than invoking codesign or lldb. `python3 .github/workflows/check_mutation_coverage.py` enforces the one-host rule.

### Catalogue files

The runner reads every visible `*.toml` file directly inside `mutations.d/`, in filename byte order; other files and subdirectories are ignored. A `mutations.toml` beside the directory is an error, so there is no single-file copy. Files are named for the crate or area their rows edit: `worker.toml`, `launch.toml`, `proto.toml`, `module.toml`, `rig.toml`, `testkit.toml` and `scripts.toml` for whole crates and `script/`; `core-*.toml` and `host-*.toml` for areas of basal-core and basal-host, such as `core-journal.toml`, `core-codemode.toml` and `host-builtins.toml`. Add a row to the file for the area of the source it edits, or start a new file for a new area. Row ids must be unique across all files; `ckdev-mutate check` refuses a duplicate and names every file defining it. Runner reports, `check` errors and the repository's catalogue scripts name the file and line of each row they report.

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

`prove` writes a row only when it is caught, as a new `mutations.d/<id>.toml`. Inspect the row, move it unchanged into the file for its area, run `ckdev-mutate check`, and commit the source, guarding test and catalogue together. The [pinned runner's README](https://github.com/cortexkit/commons/blob/be120e9254c4972191b39b2e6814987918206afc/crates/cortexkit-mutate/README.md) documents multi-file edits, anchor scopes, run sets and survivor diagnosis. When a manifest edit changes dependency resolution, resolve it without `--locked` in a scratch copy and include the resulting `Cargo.lock` changes in `edits`; the runner restores and byte-verifies the lockfile like any other edit target. Rows guarding the rig script's offline Python tests use `runner = "command"`: prove one with `--runner command --test-count-pattern 'Ran {count} test' --expect-red script.tests.flows_rig.RigChecks.<test> --command python3 -m unittest '{test}'`, putting `--command` last because it consumes everything after it. The runner first checks that the named test passes on the unmutated tree, then that it fails on the mutant. PR CI replays rows touched by the committed diff against `origin/main`; pushes to main replay the entire catalogue in isolated shards. A nightly run replays it with `--broad`, against every test target in each row's package, and grades a control CAUGHT_BROADLY when its mutant also breaks tests in another target. A HUB row is a control whose mutant is expected to break tests in other targets too, because they assert the same property; reviewed HUB rows name that shared property in `hub` and list only those other targets in `hub_targets`. The broad replay accepts only that set or a subset, warning on any new target. Structural breaks must be narrowed instead of labelled HUB. Diff selection cannot see a changed helper or fixture that is neither an edit target nor `test_file`, so the full replay remains necessary.

## The test rig

`script/flows-rig.sh` builds, places and runs ckdev-flows, a private subc daemon with its own port, homes and binaries, so basal can be tested against the real prefrontal-core, Broca and entorhinal without touching a production daemon. The rig root defaults to `~/Work/Projects/CortexKit-scratch/ckdev-flows`; set `BASAL_RIG_ROOT` to an absolute path to override it. The guard refuses roots under `~/.local/share/cortexkit/` or `/Volumes/`. Before a worker rehearses a production stage, the head runs `copy-stage <stage-dir>` to copy the binaries, sidecars, card and revision under `<rig-root>/stages/`; the worker then uses `place --from-stage <rig-root>/stages/<stage-name>`. Its header documents the subcommands and isolation rules.

## Documentation

- [docs/manifest.md](docs/manifest.md): the flow manifest schema: triggers, grants, limits and the code hash.
- [docs/script.md](docs/script.md): how a flow script runs, and two deliberate limits of the script runtime.
- [docs/ops.md](docs/ops.md): the operations agents reach through prefrontal-core's relay, with their request and reply shapes.
- [docs/deploy.md](docs/deploy.md): staging, signing and placing the two binaries, and rolling them back.
- [docs/sandbox.md](docs/sandbox.md): how the worker is confined on each operating system, what it can still do, and the differences between systems.

## License

MIT, as declared in `Cargo.toml`.
