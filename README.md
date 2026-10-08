# basal

basal is the flow engine for the CortexKit fleet. A flow is a small script with a manifest: it reacts to an event or a schedule, reads facts and module data, decides, and writes what an agent should hear about into that agent's sinks through prefrontal-core. Every flow is approved by the operator before it runs, and approval binds to the exact code: any edit is a new version that needs a new approval.

The name comes from the basal ganglia, the part of the brain that selects actions and runs habits and routines.

basal runs as a module supervised by the subc daemon, with the binary `ck-basal`. Scripts run in a separate, confined QuickJS worker, `ck-basal-worker`, that holds no durable state: the parent process checks every call a script makes against its approved manifest, journals it before it leaves and records its outcome when it comes back, so a crash anywhere resumes the run without repeating an effect.

## Crates

| Crate | What |
|---|---|
| `basal-proto` | The versioned IPC protocol between the parent and its worker processes. |
| `basal-worker` | `ck-basal-worker`: the confined QuickJS engine. It applies its own Seatbelt sandbox before reading anything, so it runs on macOS only. |
| `basal-core` | Runtime state: the SQLite store, admission, the run state machine, the journal and mailbox, manifests and authorization, the scheduler and the activation driver. |
| `basal-host` | The `Host` boundary that flow calls go through (module ops, facts, model calls, sinks), the adapters to the fleet's modules, and a deterministic mock. |
| `basal-module` | `ck-basal`: the supervised module, with its subc manifest, the worker pool, the flow ops, dry runs and consent cards. |
| `basal-testkit` | Test parents, crash and cut harnesses, and benchmarks. |
| `basal-rig` | Test support for the isolated ckdev-flows rig only: a callosum stub and the live contract suite against a real prefrontal-core. Never deployed. |

## Building and testing

basal needs Rust 1.88 or newer (edition 2024) on macOS. Dependencies are published on crates.io or pinned to an immutable Git revision.

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

## Mutation proofs

A passing safety test proves little until it has been seen to fail. [`mutations.toml`](mutations.toml) is the checked-in catalogue of source edits and the exact full libtest paths that must catch them. The shared `ckdev-mutate` runner builds each mutant separately, runs its target's tests, requires the named test to fail, and restores the saved source bytes and checks `Cargo.lock`. The existing proofs name one guard each; other tests may also catch the mutant (`only = false`). Install the reviewed, immutable revision:

```sh
cargo install --locked --git https://github.com/cortexkit/commons --rev 0097d269a4c409db13306a11fc2c504057629791 cortexkit-mutate
mkdir -p target/mutations
ckdev-mutate check
ckdev-mutate run --all --report target/mutations/all.json
ckdev-mutate run --diff origin/main --report target/mutations/diff.json
ckdev-mutate run --only worker-leaves-inherited-descriptors-open --report target/mutations/one.json
```

Run from a clean tree with `BASAL_WORKER_BIN` and `BASAL_CUT_EXHAUSTIVE` unset, so the tests build the edited worker and use their normal scope. Build and test deadlines are separate: rows allow 3600 seconds to build on a loaded host and 600 seconds to run the tests. A successful replay reports every row `CAUGHT`; compilation errors, missing anchors, missing tests, failures of other tests without the named guard failing, and timeouts are not catches. Never check out an edited file while a replay is running. Reports stay under gitignored `target/mutations/` locally and are uploaded as CI artifacts; benchmark measurements remain in [`evidence/`](evidence/README.md).

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

`prove` appends a row only when it is caught. Inspect the appended row, run `ckdev-mutate check`, and commit the source, guarding test and catalogue together. The [pinned runner's README](https://github.com/cortexkit/commons/blob/0097d269a4c409db13306a11fc2c504057629791/crates/cortexkit-mutate/README.md) documents multi-file edits and survivor diagnosis. When a manifest edit changes dependency resolution, resolve it without `--locked` in a scratch copy and include the resulting `Cargo.lock` changes in `edits`; the runner restores and byte-verifies the lockfile like any other edit target. Rows guarding the rig script's offline Python tests use `runner = "command"`: prove one with `--runner command --test-count-pattern 'Ran {count} test' --expect-red script.tests.flows_rig.RigChecks.<test> --command python3 -m unittest '{test}'`, putting `--command` last because it consumes everything after it. The runner first checks that the named test passes on the unmutated tree, then that it fails on the mutant. PR CI replays rows touched by the committed diff against `origin/main`; pushes to main replay the entire catalogue in isolated shards. A nightly run replays it with `--broad`, against every test target in each row's package, and grades a control CAUGHT_BROADLY when its mutant also breaks tests in another target. Reviewed HUB rows name the shared property and list only the other targets asserting it; the broad replay accepts only that set or a subset, warning on any new target. Structural breaks must be narrowed instead of labelled HUB. Diff selection cannot see a changed helper or fixture that is neither an edit target nor `test_file`, so the full replay remains necessary.

## The test rig

`script/flows-rig.sh` builds, places and runs ckdev-flows, a private subc daemon with its own port, homes and binaries, so basal can be tested against the real prefrontal-core, Broca and entorhinal without touching a production daemon. Its header documents the subcommands, what the rig isolates and the revisions it is pinned to.

## Documentation

- [docs/manifest.md](docs/manifest.md): the flow manifest schema: triggers, grants, limits and the code hash.
- [docs/ops.md](docs/ops.md): the operations agents reach through prefrontal-core's relay, with their request and reply shapes.
- [docs/deploy.md](docs/deploy.md): staging, signing and placing the two binaries, and rolling them back.

## License

MIT, as declared in `Cargo.toml`.
