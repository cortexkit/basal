# Contributing to basal

## The gate

A change lands only when everything below passes. CI runs it on every pull request and every push to `main` (`.github/workflows/ci.yml`); run it locally first. [README.md](README.md#build-and-test) says which commands need more than the Rust toolchain.

Formatting and lints, both clippy configurations:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings
```

Tests, with and without the rig kill hook. The portable crates run on Linux in CI; the worker packages run on every Linux, macOS and Windows leg:

```sh
cargo test --locked -p basal-proto -p basal-core -p basal-host
cargo test --locked -p basal-proto -p basal-core -p basal-host --features basal-host/rig-kill-hook
cargo test --locked -p basal-worker -p basal-module -p basal-testkit -p basal-rig
cargo test --locked -p basal-worker -p basal-module -p basal-testkit -p basal-rig --features basal-module/rig-kill-hook
```

CI also builds every target at the declared minimum Rust version:

```sh
cargo +1.88.0 check --workspace --all-targets --locked
cargo +1.88.0 check --workspace --all-targets --locked --features basal-module/rig-kill-hook
```

The script checks:

```sh
shellcheck -x script/*.sh
/bin/sh -n script/stage.sh
python3 script/check-path-deps.py
python3 script/check-ckdev-names.py
python3 script/check-local-paths.py
python3 -m unittest script.tests
python3 -m unittest discover -s .github/workflows -p 'test_*.py'
```

The mutation catalogue checks, with the pinned runner installed as [docs/mutations.md](docs/mutations.md) describes:

```sh
ckdev-mutate check
python3 .github/workflows/check_mutation_coverage.py
```

`ckdev-mutate check` validates every row's anchors and test names, and `check_mutation_coverage.py` enforces that every row declares exactly one replay platform. On a pull request CI then replays the rows the diff touches, with `ckdev-mutate run --diff origin/main`; on a push to `main` it replays the whole catalogue in shards, and `check_mutation_coverage.py --reports` requires exactly one successful replay of every row across all shards. A separate Windows job runs the launcher, worker, module and host suites natively on windows-latest and windows-2022.

## Commits

- Author commits with your GitHub no-reply address (`<id>+<user>@users.noreply.github.com`), not a personal or work address.
- Keep local paths out of tracked files. `script/check-local-paths.py` refuses absolute user-home paths; also leave out scratch directories, machine names and anything else that only exists on one machine.
- Don't commit raw logs or generated machine output. Mutation reports stay under the git-ignored `target/mutations/`, and CI uploads its own reports as artifacts. Measurements that back a performance claim belong in [`evidence/`](evidence/README.md), with the command that produced them.
- Commit a safety guard together with the source it guards and its catalogue row.

## Where code goes

| Crate | Owns |
|---|---|
| `basal-proto` | The parent–worker IPC protocol. A change that isn't additive needs the stop-the-world update ([docs/deploy.md](docs/deploy.md#the-update-card)). |
| `basal-worker` | `ck-basal-worker`: the QuickJS engine and its self-confinement on each operating system ([docs/sandbox.md](docs/sandbox.md)). |
| `basal-launch` | Windows process creation: the LPAC worker launch, job-owned child processes and the shared spawn lock. |
| `basal-core` | The SQLite store and schema, admission, the run state machine, the journal and mailbox, manifests and authorization, the scheduler and the activation driver. |
| `basal-host` | The `Host` boundary for flow calls, the built-ins, the adapters to the fleet's modules, and the deterministic mock. |
| `basal-module` | `ck-basal`: the subc manifest and serving, the codemode tool-provider role, the worker pool, the flow ops, dry runs and consent cards. |
| `basal-testkit` | Test parents, crash and cut harnesses, and benchmarks. |
| `basal-rig` | Test support for the ckdev-flows rig only; never deployed. |

Test doubles and test-only helpers stay behind `cfg(test)` or the owning crate's `test-support` feature ([docs/testing.md](docs/testing.md#test-only-code)). Scripts live in `script/`, with their offline tests in `script/tests/`. Checked-in flows live in `flows/` and templates in `templates/`.

## The mutation catalogue

Every safety guard has a mutation proof: a source edit that the named test must catch. The catalogue is the `mutations.d/*.toml` files, one per crate or area; add a row to the file for the area of the source it edits. Every row declares exactly one platform in `platforms` (`linux`, `macos` or `windows`), the one CI host that replays it, and row ids are unique across all files. Record a row with `ckdev-mutate prove`, which writes it only when the mutant is caught. [docs/mutations.md](docs/mutations.md) gives the file naming, the platform rules, anchor scopes, run sets and the `prove` command.
