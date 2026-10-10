# Windows build failures

This file lists every failure the `windows-baseline` CI job recorded when it
first built and tested the workspace on Windows. Each later fix removes its
entries from this list. A test that is gated to Unix instead of rewritten is
named here by the change that gates it.

## The measured run

- **Run:** GitHub Actions run 38053807781, job `windows-baseline`
  (https://github.com/cortexkit/basal/actions/runs/38053807781/job/114218084062),
  on 2026-10-10. Logs are in the run's `windows-baseline-logs` artifact.
- **Commit:** `8bb6e319ec56505d0e61b3376cb94acd8068bb0d`, built as the pull
  request merge commit `d98c101a330ee1139c554c8a1398b5ee8c546b59`.
- **Runner image:** `windows-latest`, which resolved to `windows-2025-vs2026`
  version 20260925.250.1.
- **Toolchain:** `rustc 1.99.0 (b940084d7 2026-09-28)`,
  `cargo 1.99.0 (5f94df478 2026-08-27)`, target `x86_64-pc-windows-msvc`.
- **Cache:** none was restored, so every crate was built from scratch.

| Command | Exit | Result |
|---|---:|---|
| `cargo check --workspace --all-targets --locked` | 101 | stops at `basal-host` (lib), 42 errors |
| `cargo check --workspace --all-targets --locked --keep-going` | 101 | 46 errors in 3 targets, plus the `basal-testkit` build script |
| `cargo test --workspace --locked` | 101 | stops at `basal-host` (lib), the same 42 errors; no test ran |
| `cargo test --workspace --locked --no-fail-fast` | 101 | the same 42 errors; no test ran |

The job continues past each failure, so its own conclusion is `success`.
The step summary and the logs show each command's real outcome.

No test ran, so no test failures are known yet. The test list gets measured
once the workspace compiles.

## What did build

These C build scripts ran without error on the runner: `ring` 0.17.14,
`blake3` 1.8.7, `rquickjs-sys` 0.14.0 and `libsqlite3-sys` 0.30.1. On a Mac,
the cross-target check stops in them, but with MSVC on the runner they are not
a Windows failure.

`basal-proto` checks cleanly, and so does the `basal-rig` library.

## Failures, by crate and file

Every error below is from the `--keep-going` check. Paths are relative to the
repository root, and locations are `line:column`.

### basal-host

The library target fails with 42 errors. The library's test target fails with
those 42 plus 2 more, in `src/builtins/git/tests.rs`, for 44.

#### `crates/basal-host/src/builtins/fs.rs` (31 errors)

| Location | Code | Error |
|---|---|---|
| 35:14 | E0432 | unresolved import `std::os::fd` (`use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};`) |
| 36:14 | E0433 | cannot find `unix` in `os` (`use std::os::unix::ffi::OsStrExt;`) |
| 38:14 | E0433 | cannot find `unix` in `os` (`use std::os::unix::fs::OpenOptionsExt;`) |
| 220:27 | E0425 | cannot find value `O_NOFOLLOW` in crate `libc` |
| 220:46 | E0425 | cannot find value `O_CLOEXEC` in crate `libc` |
| 220:64 | E0425 | cannot find value `O_NONBLOCK` in crate `libc` |
| 222:24 | E0425 | cannot find value `O_DIRECTORY` in crate `libc` |
| 297:24 | E0425 | cannot find type `mode_t` in crate `libc` |
| 301:15 | E0531 | cannot find unit struct, unit variant or constant `S_IFLNK` in crate `libc` |
| 317:28 | E0425 | cannot find function `fstatat` in crate `libc` |
| 317:76 | E0425 | cannot find value `AT_SYMLINK_NOFOLLOW` in crate `libc` |
| 335:29 | E0425 | cannot find value `O_NOFOLLOW` in crate `libc` |
| 335:48 | E0425 | cannot find value `O_CLOEXEC` in crate `libc` |
| 335:66 | E0425 | cannot find value `O_DIRECTORY` in crate `libc` |
| 406:19 | E0531 | cannot find unit struct, unit variant or constant `DT_REG` in crate `libc` |
| 407:19 | E0531 | cannot find unit struct, unit variant or constant `DT_DIR` in crate `libc` |
| 408:19 | E0531 | cannot find unit struct, unit variant or constant `DT_LNK` in crate `libc` |
| 409:19 | E0531 | cannot find unit struct, unit variant or constant `DT_UNKNOWN` in crate `libc` |
| 454:30 | E0425 | cannot find function `fdopendir` in crate `libc` |
| 463:36 | E0425 | cannot find function `readdir` in crate `libc` |
| 485:20 | E0425 | cannot find function `closedir` in crate `libc` |
| 587:22 | E0425 | cannot find type `mode_t` in crate `libc` |
| 603:23 | E0425 | cannot find function `unlinkat` in crate `libc` |
| 614:33 | E0425 | cannot find type `mode_t` in crate `libc` |
| 614:50 | E0425 | cannot find type `mode_t` in crate `libc` |
| 661:67 | E0425 | cannot find type `mode_t` in crate `libc` |
| 668:19 | E0425 | cannot find function `openat` in crate `libc` |
| 671:71 | E0425 | cannot find value `O_NOFOLLOW` in crate `libc` |
| 671:90 | E0425 | cannot find value `O_CLOEXEC` in crate `libc` |
| 718:56 | E0425 | cannot find value `S_IFLNK` in crate `libc` |
| 776:32 | E0425 | cannot find function `renameat` in crate `libc` |

#### `crates/basal-host/src/builtins/git.rs` (10 errors)

| Location | Code | Error |
|---|---|---|
| 19:14 | E0432 | unresolved import `std::os::fd` (`use std::os::fd::AsRawFd;`) |
| 20:14 | E0433 | cannot find `unix` in `os` (`use std::os::unix::process::CommandExt;`) |
| 321:31 | E0425 | cannot find function `fcntl` in crate `libc` |
| 321:47 | E0425 | cannot find value `F_SETFL` in crate `libc` |
| 321:62 | E0425 | cannot find value `O_NONBLOCK` in crate `libc` |
| 424:19 | E0425 | cannot find function `kill` in crate `libc` |
| 424:50 | E0425 | cannot find value `SIGKILL` in crate `libc` |
| 490:36 | E0422 | cannot find struct, variant or union type `pollfd` in crate `libc` |
| 492:35 | E0425 | cannot find value `POLLIN` in crate `libc` |
| 502:45 | E0425 | cannot find function `poll` in crate `libc` |

#### `crates/basal-host/src/builtins/git/tests.rs` (2 errors, test target only)

| Location | Code | Error |
|---|---|---|
| 77:24 | E0425 | cannot find function `waitpid` in crate `libc` |
| 77:65 | E0425 | cannot find value `WNOHANG` in crate `libc` |

#### `crates/basal-host/src/core_host.rs` (1 error)

| Location | Code | Error |
|---|---|---|
| 243:29 | E0425 | cannot find function `getentropy` in crate `libc` |

### basal-worker

The library target fails with 2 errors.

#### `crates/basal-worker/src/clock.rs` (2 errors)

| Location | Code | Error |
|---|---|---|
| 25:29 | E0425 | cannot find function `clock_gettime` in crate `libc` |
| 25:49 | E0425 | cannot find value `CLOCK_THREAD_CPUTIME_ID` in crate `libc` |

### basal-testkit

#### `crates/basal-testkit/build.rs` (build script panic)

The build script exits with code 101. It panics at `build.rs:37:5` with
`building the test worker failed: exit code: 101`. The script runs a nested
`cargo build -p basal-worker --bin ck-basal-worker`, and that build fails on
the same two `crates/basal-worker/src/clock.rs` errors listed above. No other
error is in that nested build's output.

## Targets the run did not reach

Cargo does not build a target whose dependency failed, so these targets were
never compiled and may hide more failures. They have to be measured again once
the failures above are fixed.

- **basal-core:** every target. It depends on `basal-host`.
- **basal-module:** every target except its build script, which compiled. It
  depends on `basal-core` and `basal-host`.
- **basal-testkit:** every target. Its build script failed, and it depends on
  `basal-core` and `basal-host`.
- **basal-worker:** the `ck-basal-worker` binary and every test target. They
  need the failed library, and the tests also need `basal-testkit`.
- **basal-host:** the integration tests in `crates/basal-host/tests/`. They
  need the failed library.
- **basal-rig:** its test targets, including `crates/basal-rig/tests/`. They
  dev-depend on `basal-host`.
- **Tests:** every test in the workspace. Both test commands stopped while
  compiling `basal-host`.
