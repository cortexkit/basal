# Structure

The directory map, and where new code goes. `ARCHITECTURE.md` explains how the pieces work together.

## Top level

| Path | Contents |
|---|---|
| `crates/` | The Rust workspace: eight crates, below. |
| `flows/` | Checked-in flows the fleet installs (`board-hygiene`, `ci-failure-router`, `github-events`), each a `flow.js` and `manifest.json`. |
| `templates/` | Parameterised templates core registers (`remind-at`, `watch-file`, `watch-git`, `watch-github`, `github-ci-watch`). |
| `docs/` | Contracts and operating notes: manifest, script runtime, built-ins, ops, codemode, templates, packages, sandbox, deploy, testing. |
| `test-vectors/` | Pinned samples shared with peer modules, one directory per contract (for example `flows-phone-v1`, `flow-room-post-v1`, `diagnostics-findings-v1`), most with a `SHA256SUMS`. |
| `script/` | `gate.sh` (the CI-matching gate), `stage.sh` (stages a deploy card), `sign-worker.sh`, `flows-rig.sh` (the isolated ckdev-flows rig), the Python checks, and their tests in `script/tests/`. |
| `.github/workflows/ci.yml` | CI on Linux, macOS and Windows. |

## Crates

Dependencies point downwards in this order: `basal-proto` ← `basal-worker`; `basal-proto` ← `basal-host` ← `basal-core` ← `basal-module`. `basal-core` never depends on `basal-worker` and never spawns a process. On Windows, `basal-module` starts workers through `basal-launch`, and `basal-host`'s git runner shares its spawn lock.

### `basal-proto`: the parent–worker protocol

`types.rs` (messages, `Primitive`, budgets), `wire.rs` and `codec.rs` (encoding), `frame.rs` (length-prefixed frames), `limits.rs` (`PROTOCOL_VERSION` and hard size limits).

### `basal-worker`: `ck-basal-worker`

- `engine.rs`, `engine/`: one activation: a fresh QuickJS runtime, the prelude, replay, and new calls through `link.rs`.
- `prelude.js`, `codemode_prelude.js`, `harden.rs`: the script globals for flows and for codemode programs, and the freeze of every reachable intrinsic.
- `clock.rs`, `clock/`: the JavaScript CPU budget.
- `confinement.rs`, `confinement/{linux,windows}*`: self-confinement before the first frame; macOS uses the Seatbelt profile.
- `probe.rs`, `probe/`: `--confinement-probe`, evidence that the sandbox denies what it should.

### `basal-launch`: Windows process creation

The LPAC worker launch, job-owned child processes and the shared spawn lock (`src/windows/`).

### `basal-host`: what a call reaches

- `lib.rs`: the `Host` trait, `CallRequest`, outcomes, the consent and catalog traits.
- `routing.rs`: sends module ops to providers, core's sinks and facts to core, and model calls to the model host.
- `transport.rs`: the blocking subc client: management, tool and run-scope routes.
- `core_host.rs`: core's sinks, facts, carrier reads and install status.
- `broca/`: model calls through Broca, the `run.ended` wake, and a deterministic fake.
- `selector.rs`: model choice through the routing module (the `routing/v1` provider).
- `builtins/`: `fs`, `git` and `net.fetch`, with their Windows implementations.
- `secrets.rs`, `secrets/`: vault references, injection and echo redaction.
- `subc_catalog.rs`, `catalog.rs`: what the daemon declares; `flow_scope.rs`, `target_scope.rs`, `run_scope.rs`, `scope_describe.rs`: scopes basal carries.
- `consent.rs`, `core_consent.rs`, `cingulate_consent.rs`: install requests to core, decision cards to Cingulate.
- `flow_refusal.rs`: refusal codes decoded into typed reasons.
- `mock.rs`: the deterministic mock host for tests.

### `basal-core`: everything durable

- **Store and schema:** `store.rs` (open, durability, read and write transactions), `run_cutover/schema.rs` (`MIGRATIONS`), `run_cutover.rs` and `run_cutover/` (the one-time conversion to the run log).
- **Flows and approval:** `manifest.rs`, `install.rs`, `install_policy.rs`, `cards.rs`, `gate.rs`, `revocation.rs`, `ids.rs` (code hash), `flow_ops.rs`, `packages.rs`, `templates.rs`, `template_events.rs`.
- **Triggers:** `schedule/` (spec, due times, the schedules table, ticks), `events.rs` (event receipts and per-flow queues), `admission.rs`, `run_admission.rs`.
- **Runs:** `runtime.rs` (`Runtime`, `Config`), `staged_runtime.rs` (how the runtime is composed), `driver.rs` and `run_driver/` (an activation), `run_store/` (in-memory run actors, the terminal committer, lock order), `run_records/` (the record types), `run_log/` (segments, sync, recovery, restore), `run_store_sql.rs` (the SQLite control plane), `run_host.rs`, `run_readers.rs`, `run_removal.rs`, `run_retention.rs`, `preparation.rs`.
- **Limits and accounting:** `authorize.rs`, `rate.rs`, `tokens.rs`, `classify_cost.rs`, `kv.rs`, `state_epoch.rs`.
- **Operator side:** `decisions.rs`, `run_decisions.rs`, `reconcile.rs`, `grant_loss.rs`, `owner_notice.rs`, `secret_revocation.rs`, `ops.rs` (health), `retention.rs`, `maintenance.rs` (the next wake time).
- **Codemode:** `codemode/` (admission, catalog digest, supervisor, held calls and custody, the `model` tool, the run store, retention).
- **Test-only:** `hooks.rs` (named crash boundaries; `NoHooks` in production), `test_support.rs`, `run_fixture.rs`.

### `basal-module`: `ck-basal`

- `main.rs`: startup and the production hosts. `serve.rs`: the subc handler. `module.rs`: assembly and recovery order. `manifest.rs`: the subc manifest (`ck-basal --manifest`).
- `caller.rs`: who is calling, from the daemon's stamp.
- `ops.rs`: dispatches every management op and implements the `flow.*`, `package.*`, template and `events_get` ops. `phone_flows.rs`: `flow.overview`, `flow.details`, `flow.uninstall`. `diagnostics_findings.rs`: `diagnostics.findings`. `serve.rs` refuses a flow-scoped route everything but `diagnostics.findings`.
- `tool.rs`, `codemode.rs`: the `codemode` tool. `flow_tool.rs`, `flow_tool/text.rs`: the `flow` tool.
- `engine.rs`: the loop. `pool.rs`, `process.rs`, `windows.rs`: worker processes. `events.rs`: the `m_basal` reader.
- `card.rs`: install card fields. `dryrun.rs`: dry runs. `engram_catalog.rs`: the backup catalog. `fatal.rs`, `metrics.rs`.
- `harness.rs`, `harness_transport.rs`, `unconfigured.rs`, `bin/ck-basal-harness.rs`: the module against mocks, for tests. `rig_kill.rs`: the rig's kill switch (feature `rig-kill-hook` only).

### `basal-testkit` and `basal-rig`

`basal-testkit`: a test parent over the real worker, cut and kill harnesses, the local HTTPS server, write-amplification benchmarks and the cutover rehearsal tool. `basal-rig`: the ckdev-flows rig's callosum stub and its live contract suite against a real core; never deployed.

## Where to add new code

- **A management op:** the handler in `basal-module/src/ops.rs` (or `phone_flows.rs` for operator screens) with its caller check (callers are classified in `caller.rs`), the entry in `manifest.rs` `OPERATIONS`, the row in the table at the top of `ops.rs`, the shape in `docs/ops.md`, and sample files under `test-vectors/` when a peer module consumes it.
- **A script call (primitive):** a `Primitive` in `basal-proto/src/types.rs` and its encoding, which needs a `PROTOCOL_VERSION` bump in `limits.rs` and a stop-the-world deploy; the JavaScript binding in `basal-worker/src/prelude.js`; its grant in `basal-core/src/manifest.rs` and its check in `authorize.rs`; its host in `basal-host` (a built-in in `builtins/`, or routing in `routing.rs`); documentation in `docs/script.md` or `docs/builtins.md`.
- **A store change:** a new entry at the end of `MIGRATIONS` in `basal-core/src/run_cutover/schema.rs`, the version assertion in `run_store_sql_tests.rs`, and a test that migrates a populated store of the previous version. Rehearse it on a copy of the live store before deploying.
- **A codemode feature:** `basal-core/src/codemode/` for behaviour, `basal-module/src/codemode.rs` for rendering. A change to the tool's description or schema (`tool.rs`, pinned by `basal-module/tests/fixtures/codemode-description.txt`) waits for a host restart window.
- **A host adapter for a module:** `basal-host`, routed from `routing.rs`; catalog facts in `subc_catalog.rs`.
- **A checked-in flow:** `flows/<name>/flow.js` and `manifest.json`, tested in `basal-module/tests/<name>.rs`. It runs only after it is installed and the operator approves its card.
- **A template:** `templates/<name>/`, documented in `docs/templates.md`; core registers template versions.

## Conventions

- Unit tests sit in a sibling `<module>_tests.rs` file (or `tests.rs` inside a module directory), included with `#[cfg(test)]`. Integration tests go in each crate's `tests/`.
- Test doubles and test-only helpers stay behind `cfg(test)` or the owning crate's `test-support` feature. Production binaries never compile crash hooks.
- Tests assert structure (counters, recorded order, durable state), never wall-clock speed.
- Test and development executables use `ckdev-` names; only placed production binaries use `ck-`.
- Git test fixtures run git with an isolated environment (`PATH` and `HOME` only, no system or global config).
- Wording is OS-neutral; a platform is named only where the text is about that platform.
