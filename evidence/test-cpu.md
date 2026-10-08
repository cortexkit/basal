# Test worker builds and SQLite allocation accounting

Measurements were made on the shared Mac with Cargo 1.99.0 and rustc 1.99.0.
Times are observations, not test assertions or performance guarantees.

## Worker builds

The original `cargo test -p basal-testkit -p basal-module -p basal-host --locked`
run passed and took 165.74 s real, 144.61 s user and 75.12 s sys. A temporary
append-only counter immediately before the worker's Cargo launch recorded
**41 worker build invocations**. The counter was then removed.

The updated local `cargo test -p basal-testkit -p basal-module --locked` run
passed 351 tests across 61 libtest summaries and took 296.42 s real, 142.22 s
user and 62.63 s sys, including 155 s of compilation. The host suite was moved
to Linux, so these wall times are not an apples-to-apples speed comparison.

An initial unscoped launch-site counter recorded two build-script executions
while the updated suite ran. To avoid counting concurrent analyzer builds, an
environment-scoped counter was used for the same Cargo selection's `--no-run`
build stage: it recorded **one worker Cargo build**. Both temporary counters
were removed. The discovery regression test launches two independent test
processes with a recording Cargo executable and observes **zero runtime Cargo
invocations**; both processes successfully greet and reap the built worker.

The worker is built once per Cargo testkit fingerprint in its build script,
with a private target directory to avoid the outer Cargo build lock. Worker
and protocol inputs, workspace configuration and the lockfile invalidate that
build. Worker contract tests use the same testkit-selected binary, so replaying
`worker-stack-trace-hook-exposes-private-functions` exercises this freshness
path, not a separate `CARGO_BIN_EXE` binary. That replay caught only
`stack_trace_hooks_cannot_recover_private_prelude_functions`; the other three
sandbox-integrity tests stayed green.

## SQLite measurements

Each row below used `/usr/bin/time -l <binary> -q`, with `--test-threads=1`
added for serial rows. `journal_races` ran 11 tests; `dispatch_slots` ran four.
Every measurement passed.

| SQLite build | Binary | Threads | Real (s) | User (s) | Sys (s) |
|---|---|---|---:|---:|---:|
| Original | journal_races | default | 1.13 | 0.65 | 0.38 |
| Original | journal_races | 1 | 1.50 | 0.62 | 0.26 |
| Original | dispatch_slots | default | 3.68 | 0.28 | 0.15 |
| Original | dispatch_slots | 1 | 3.86 | 0.28 | 0.14 |
| DEFAULT_MEMSTATUS=0 | journal_races | default | 0.88 | 0.52 | 0.27 |
| DEFAULT_MEMSTATUS=0 | journal_races | 1 | 1.29 | 0.49 | 0.19 |
| DEFAULT_MEMSTATUS=0 | dispatch_slots | default | 3.30 | 0.18 | 0.08 |
| DEFAULT_MEMSTATUS=0 | dispatch_slots | 1 | 3.50 | 0.19 | 0.06 |

A warm confirmation before the configuration change measured journal_races at
0.89/0.64/0.39 s (real/user/sys) with default threads and 1.50/0.63/0.26 s
serial. Dispatch_slots measured 3.40/0.27/0.15 s and 3.67/0.28/0.14 s.
Journal_races' approximately 46–50% higher parallel sys time justified disabling
unused SQLite allocation accounting. These figures do not isolate that mutex
from all other effects: the later measurements also include the worker-discovery
change and changed machine load. No after-change measurement was repeated.

SQLite's `DEFAULT_MEMSTATUS` compile option turns on allocation statistics,
which every allocation updates under one process-wide mutex. basal never reads
those statistics. The production-store test asks the connection basal actually
opens whether the option was compiled as `DEFAULT_MEMSTATUS=0`, and expects
yes. Changing the build flag back to `DEFAULT_MEMSTATUS=1` made that test alone
fail, and the flag was then restored.

## Platform and proof caveats

On the shared Linux build server, the full `basal-host` test run failed one
test, `atomic_write_drops_special_permission_bits`. The server's sandbox
forbids setting setuid and setgid bits, so `chmod` returned `EPERM` at
builtin_host_regressions.rs:136. The test is unchanged and passes on macOS,
and GitHub's Linux runners allow those bits. The same run, excluding only that
test, passed 91 tests.

The new git timeout control was proved on macOS, then marked
`platforms = ["linux"]` so CI replays it on Linux. The build server couldn't
run the mutation runner at the time. The git timeout mutant was also applied by
hand on Linux, and only its named timeout test failed. The existing
descendant-pipe control was re-anchored to the waiter's stored process-group
pid, and a hand-applied mutant on Linux again failed only its named test.
