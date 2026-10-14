# basal

basal is the fleet's flow runtime. A flow is a small JavaScript script with a JSON manifest; it runs on a schedule or in response to a module event, reads facts and module data, decides what to do, and sends its output to an agent through prefrontal-core, the module that manages agents and their output destinations. Each script runs in `ck-basal-worker`, a separate QuickJS process confined by the operating system, that holds no durable state. The parent module, `ck-basal`, supervised by the subc daemon, checks every call a script makes against its approved manifest and journals it before it leaves and records its outcome when it comes back, so a crash anywhere resumes the run without repeating an effect. The operator approves each exact script version: an edit requires a new approval. basal also provides the agent-facing `codemode` tool, which runs a short JavaScript program over the calling agent's own tools in the same confined worker ([docs/codemode.md](docs/codemode.md)).

The name comes from the basal ganglia, the part of the brain that selects actions and runs habits and routines.

## Crates

| Crate | Role |
|---|---|
| `basal-proto` | The versioned IPC protocol between the parent and its worker processes. |
| `basal-worker` | `ck-basal-worker`: the confined QuickJS engine, which checks its own confinement before reading anything ([docs/sandbox.md](docs/sandbox.md)). |
| `basal-launch` | Windows process creation: restricted LPAC workers and job-owned child processes, with a shared spawn lock. |
| `basal-core` | Runtime state: the SQLite store, admission, the run state machine, the journal and mailbox, manifests and authorization, the scheduler and the activation driver. |
| `basal-host` | The `Host` boundary that flow calls go through (module ops, facts, model calls, sinks, built-ins), the adapters to the fleet's modules, and a deterministic mock. |
| `basal-module` | `ck-basal`: the supervised module, with its subc manifest, the codemode tool-provider role, the worker pool, the flow ops, dry runs and consent cards. |
| `basal-testkit` | Test parents, crash and cut harnesses, and benchmarks. |
| `basal-rig` | Test support for the isolated ckdev-flows rig only: a callosum stub and the live contract suite against a real prefrontal-core. Never deployed. |

## Build and test

basal needs Rust 1.88 or newer (edition 2024) on macOS, Linux (x86_64 or aarch64), or Windows. Dependencies are published on crates.io or pinned to an immutable Git revision.

These need only the Rust toolchain:

```sh
cargo build --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings
```

The portable crates' tests also need `git` on `PATH`, because the git built-ins' tests run it:

```sh
cargo test --locked -p basal-proto -p basal-core -p basal-host
cargo test --locked -p basal-proto -p basal-core -p basal-host --features basal-host/rig-kill-hook
```

The worker packages start real confined workers, so they run only where the worker can confine itself: macOS, Linux with seccomp and Landlock, or Windows. Their module-event tests also need `nats-server` v2.15.0 on `PATH`, and fail rather than skip without it:

```sh
cargo test --locked -p basal-worker -p basal-module -p basal-testkit -p basal-rig
cargo test --locked -p basal-worker -p basal-module -p basal-testkit -p basal-rig --features basal-module/rig-kill-hook
```

The script checks need Python 3.11 or newer and shellcheck:

```sh
python3 -m unittest script.tests
python3 script/check-path-deps.py
python3 script/check-ckdev-names.py
python3 script/check-local-paths.py
python3 .github/workflows/check_mutation_coverage.py
shellcheck -x script/*.sh
```

The mutation proofs need the pinned `ckdev-mutate` runner; [docs/mutations.md](docs/mutations.md) installs it and describes the catalogue. [CONTRIBUTING.md](CONTRIBUTING.md) lists the full gate, and [docs/testing.md](docs/testing.md) covers test-only features, development executable names and the ckdev-flows rig.

## Documentation

- [docs/manifest.md](docs/manifest.md): the flow manifest schema: triggers, grants, limits and the code hash.
- [docs/script.md](docs/script.md): how a flow script runs, and two deliberate limits of the script runtime.
- [docs/builtins.md](docs/builtins.md): the file, git and network built-ins and their scope rules.
- [docs/packages.md](docs/packages.md): flow packages, their manifests and persona instances.
- [docs/templates.md](docs/templates.md): parameterised templates and agent-created instances.
- [docs/ops.md](docs/ops.md): the operations agents reach through prefrontal-core's relay, with their request and reply shapes.
- [docs/codemode.md](docs/codemode.md): the `codemode` tool and its result contract.
- [docs/sandbox.md](docs/sandbox.md): how the worker is confined on each operating system, what it can still do, and the differences between systems.
- [docs/deploy.md](docs/deploy.md): staging, signing and placing the two binaries, and rolling them back.
- [docs/testing.md](docs/testing.md): test-only code, development executable names and the ckdev-flows rig.
- [docs/mutations.md](docs/mutations.md): the mutation-proof catalogue and the `ckdev-mutate` runner.
- [CONTRIBUTING.md](CONTRIBUTING.md): the gate, commits and where code goes.
- [SECURITY.md](SECURITY.md): reporting a vulnerability and the threat model.

## License

MIT, as declared in `Cargo.toml`.
