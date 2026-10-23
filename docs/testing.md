# Testing basal

[README.md](../README.md#build-and-test) lists the build and test commands, and [CONTRIBUTING.md](../CONTRIBUTING.md) lists what must pass before a change lands. This page describes the test-only code those commands build, the checks that keep test builds apart from the placed fleet, and the ckdev-flows rig.

## Test-only code

Mock hosts, fake providers and test-only runtime helpers require `cfg(test)` or the owning crate's `test-support` feature. Development dependencies enable that feature for `cargo test`; production builds do not enable it. The harness and testkit binaries also require `test-support` when built by hand, for example `cargo build -p basal-module --bin ck-basal-harness --features test-support`. Production staging rejects the retained test-support marker as well as the rig kill-hook marker, even if the linker removes unused test doubles.

The `rig-kill-hook` feature adds a one-shot kill switch used only by the test rig; a production build never enables it. CI runs hook-enabled tests on x86_64 Linux and macOS, not Linux aarch64. All three `checks` legs retain hook-enabled clippy; x86_64 Linux also runs clippy without the hook.

On Windows, `basal-launch` and `basal-worker` also have a `deviations` feature that adds test-only launch variants, one per confinement check; the production worker refuses the launcher's request to skip a check. CI runs natively on both Windows runner images, windows-latest and windows-2022, because each image leaves a different set of handles in the worker ([sandbox.md](sandbox.md#residual-handles-and-production-measurements)). Each image runs two package-group jobs in parallel with each other: launcher + worker + host, and module + core + testkit + rig. Host library and integration tests, including the shared spawn-lock regressions, run once in the remaining-workspace pass, rather than being repeated in separate host steps.

## Boundaries CI enforces

CI also refuses Cargo path dependencies that resolve outside this repository and test or script launches under production `ck-` file names. `python3 script/check-path-deps.py` checks the former and `python3 script/check-ckdev-names.py` the latter; tests run hard-linked (or copied) `ckdev-` executables so process monitors distinguish them from the placed fleet. `python3 script/check-local-paths.py` refuses tracked text files containing an absolute user-home path.

## The ckdev-flows rig

On macOS, `script/flows-rig.sh` builds, places and runs ckdev-flows, a private subc daemon with its own port, homes and binaries, so basal can be tested against the real prefrontal-core, Broca and entorhinal without touching a production daemon. The rig root defaults to `~/Work/Projects/CortexKit-scratch/ckdev-flows`; set `BASAL_RIG_ROOT` to an absolute path to override it. The guard refuses roots under `~/.local/share/cortexkit/` or macOS external volumes (`/Volumes/`). Before a worker rehearses a production stage, the head runs `copy-stage <stage-dir>` to copy the binaries, sidecars, card and revision under `<rig-root>/stages/`; the worker then uses `place --from-stage <rig-root>/stages/<stage-name>`. Its header documents the subcommands and isolation rules. [deploy.md](deploy.md#the-rig-run-before-a-handover) gives the rig run that precedes a handover.
## Disposable stores

Test fixtures use `Durability::throwaway()`: SQLite synchronous OFF with both
fullfsync settings off. This constructor is available only with `cfg(test)` or
the core's `test-support` feature. OFF gives up power-loss and OS-crash safety,
not process-kill recovery: the cut and kill harnesses leave the OS page cache
intact. Production uses `Durability::default()` (FULL and fullfsync on).

`tests::production_store_opens_with_full_durability` in the `ck-basal` binary
opens a store through the configuration factory used by serving and the store
opener used by module startup, and checks the pragmas SQLite actually reports.
`store_runs_synchronous_full_and_the_chosen_fullfsync` and `journal-bench` retain
FULL deliberately because they assert or measure production durability.
`basal-test-parent --fullfsync` is an explicit full-durability probe; crash tests
omit that option and use throwaway stores.

The `basal-module` integration target `durability_boundary` scans every workspace
`src/` Rust file for the `throwaway` identifier outside test-only code. It uses
syn's Rust parser instead of line matching, follows both inline and external
modules, and treats unknown platform/feature cfg conditions as possibly
production. This avoids hiding other platforms' production code while allowing
test modules, test-support features, comments and strings. Unreferenced source
files are scanned too. Planted-use tests and mutation catalogue rows exercise
both the source fence and the production pragma check.
