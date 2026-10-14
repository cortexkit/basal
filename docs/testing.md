# Testing basal

[README.md](../README.md#build-and-test) lists the build and test commands, and [CONTRIBUTING.md](../CONTRIBUTING.md) lists what must pass before a change lands. This page describes the test-only code those commands build, the checks that keep test builds apart from the placed fleet, and the ckdev-flows rig.

## Test-only code

Mock hosts, fake providers and test-only runtime helpers require `cfg(test)` or the owning crate's `test-support` feature. Development dependencies enable that feature for `cargo test`; production builds do not enable it. The harness and testkit binaries also require `test-support` when built by hand, for example `cargo build -p basal-module --bin ck-basal-harness --features test-support`. Production staging rejects the retained test-support marker as well as the rig kill-hook marker, even if the linker removes unused test doubles.

The `rig-kill-hook` feature adds a one-shot kill switch used only by the test rig; a production build never enables it. CI runs the workspace tests and clippy both with and without it.

On Windows, `basal-launch` and `basal-worker` also have a `deviations` feature that adds test-only launch variants, one per confinement check; the production worker refuses the launcher's request to skip a check. CI runs natively on both Windows runner images, windows-latest and windows-2022, because each image leaves a different set of handles in the worker ([sandbox.md](sandbox.md#residual-handles-and-production-measurements)).

## Boundaries CI enforces

CI also refuses Cargo path dependencies that resolve outside this repository and test or script launches under production `ck-` file names. `python3 script/check-path-deps.py` checks the former and `python3 script/check-ckdev-names.py` the latter; tests run hard-linked (or copied) `ckdev-` executables so Activity Monitor distinguishes them from the placed fleet. `python3 script/check-local-paths.py` refuses tracked text files containing an absolute user-home path.

## The ckdev-flows rig

`script/flows-rig.sh` builds, places and runs ckdev-flows, a private subc daemon with its own port, homes and binaries, so basal can be tested against the real prefrontal-core, Broca and entorhinal without touching a production daemon. The rig root defaults to `~/Work/Projects/CortexKit-scratch/ckdev-flows`; set `BASAL_RIG_ROOT` to an absolute path to override it. The guard refuses roots under `~/.local/share/cortexkit/` or `/Volumes/`. Before a worker rehearses a production stage, the head runs `copy-stage <stage-dir>` to copy the binaries, sidecars, card and revision under `<rig-root>/stages/`; the worker then uses `place --from-stage <rig-root>/stages/<stage-name>`. Its header documents the subcommands and isolation rules. [deploy.md](deploy.md#the-rig-run-before-a-handover) gives the rig run that precedes a handover.
