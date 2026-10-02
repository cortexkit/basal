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
| `basal-testkit` | Test parents, crash and cut harnesses, benchmarks and the mutation-control runner. |
| `basal-rig` | Test support for the isolated ckdev-flows rig only: a callosum stub and the live contract suite against a real prefrontal-core. Never deployed. |

## Building and testing

basal needs a recent stable Rust toolchain (edition 2024) on macOS. Every dependency comes from crates.io.

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings
shellcheck -x script/*.sh
```

The `rig-kill-hook` feature adds a one-shot kill switch used only by the test rig; a production build never enables it.

## Mutation controls

A passing safety test proves little until it has been seen to fail. Each mutation control disables one mechanism with a temporary source edit, runs the one test named for that mechanism, records whether it went red, and restores the source. On a clean tree:

```sh
script/verify-controls.sh               # every set
script/verify-controls.sh journal       # one set
```

The script fails unless every set ran, rewrote its evidence file and turned every one of its tests red, and then restores the committed evidence. The runner itself is `cargo run -p basal-testkit --bin mutation-controls -- [--journal | --dispatch | ...]`. The committed evidence and the benchmark measurements are in [`evidence/`](evidence/README.md).

## The test rig

`script/flows-rig.sh` builds, places and runs ckdev-flows, a private subc daemon with its own port, homes and binaries, so basal can be tested against the real prefrontal-core, Broca and entorhinal without touching a production daemon. Its header documents the subcommands, what the rig isolates and the revisions it is pinned to.

## Documentation

- [docs/manifest.md](docs/manifest.md): the flow manifest schema: triggers, grants, limits and the code hash.
- [docs/ops.md](docs/ops.md): the operations agents reach through prefrontal-core's relay, with their request and reply shapes.
- [docs/deploy.md](docs/deploy.md): staging, signing and placing the two binaries, and rolling them back.

## License

MIT, as declared in `Cargo.toml`.
