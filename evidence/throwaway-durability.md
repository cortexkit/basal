# Throwaway-store measurement

One before run and one after run on the shared Mac, using the same 279-test
basal-core unit suite and Rust toolchain. No root-only tracing was used.

Command for each run:

```sh
rustc --version
cargo --version
/usr/bin/time -p cargo test -p basal-core --lib
```

Toolchain: `rustc 1.99.0 (b940084d7 2026-09-28)`;
`cargo 1.99.0 (5f94df478 2026-08-27)`.

| Metric (seconds) | Before: FULL, fixtures mostly fullfsync off | After: OFF, fullfsync off |
|---|---:|---:|
| Command wall time (`real`) | 112.25 | 39.63 |
| Cargo build time | about 86 | 10.48 |
| Libtest suite wall time | 25.33 | 29.10 |
| Command user CPU | 90.68 | 30.97 |
| Command system CPU | 12.97 | 3.95 |
| Tests passed | 279 | 279 |

The before build was cold and the after build incremental; the after command
also waited for Cargo package-cache locks. Command times are therefore **not a
test speedup measurement**. The test-only time increased by 3.77 seconds on this
shared, loaded machine. These single samples do not establish a runtime speedup.
No sync syscall count was collected: `fs_usage` needs root, and SQLite's exposed
statement/database counters do not count OS sync calls. The pragma regression
test reads SQLite's actual OFF setting for throwaway stores, independently of
timing. Production's construction path is separately checked to remain FULL.

The crash harnesses simulate process termination, not power loss. `journal_cut`
freezes later store operations and reopens after quiescence; `journal_kill`
terminates a parent with SIGKILL (or TerminateProcess on Windows). Both preserve
the OS and its page cache. Their before/after summaries remain equal with OFF;
neither harness claims that unsynced commits survive an OS crash or power loss.

The journal benchmark intentionally keeps FULL to measure production commit
durability with and without F_FULLFSYNC. The explicitly named production-pragma
test also keeps FULL; neither is a general-purpose disposable fixture.
