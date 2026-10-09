# Linux syscall admission

The seccomp boundary admits only the calls below, with exact native numbers in
`src/confinement/linux/filter.rs`. Every unmatched call or rejected argument kills
the whole process. The native audit architecture is checked; x32 calls cannot
match an x86_64 entry. TSYNC installation checks negative errors and positive
offending-thread returns. Landlock is a second, empty-ruleset boundary, not the
reason for admitting any syscall.

`yes` denotes an entry in that architecture's filter, not a claim that every call
was observed. aarch64 has no `time` syscall. Loader calls (`arch_prctl`,
`set_tid_address`, `rseq`) grant no post-confinement requirement and are absent.

| syscall | x86_64 | aarch64 | review and predicate |
| --- | --- | --- | --- |
| read | yes | yes | stdin only, fd exactly 0 |
| readv | yes | yes | stdin only, fd exactly 0 |
| write | yes | yes | stdout/stderr only, fd exactly 1 or 2 |
| writev | yes | yes | stdout/stderr only, fd exactly 1 or 2 |
| mmap | yes | yes | flags exactly MAP_PRIVATE + MAP_ANONYMOUS; no PROT_EXEC |
| mprotect | yes | yes | no PROT_EXEC; cannot add executable memory |
| munmap | yes | yes | releases this process's mappings |
| brk | yes | yes | adjusts this process's private heap |
| mremap | yes | yes | flags 0 or MAYMOVE; no FIXED or DONTUNMAP |
| madvise | yes | yes | DONTNEED only, allocator page release |
| futex | yes | yes | WAIT_PRIVATE or WAKE_PRIVATE only; no cross-process futex |
| clock_gettime | yes | yes | REALTIME, MONOTONIC, THREAD_CPUTIME_ID only |
| clock_nanosleep | yes | yes | REALTIME, MONOTONIC, THREAD_CPUTIME_ID only |
| nanosleep | yes | yes | waits without external authority |
| gettimeofday | yes | yes | vDSO fallback, reads wall clock |
| time | yes | no | x86_64 vDSO fallback, reads wall clock |
| getcpu | yes | yes | vDSO fallback, reads CPU placement |
| getrandom | yes | yes | seeds std HashMap without opening a device |
| rt_sigreturn | yes | yes | returns from this process's signal handler |
| rt_sigprocmask | yes | yes | changes own signal mask |
| rt_sigaction | yes | yes | registers own handlers, including glibc abort |
| sigaltstack | yes | yes | own signal stack |
| exit | yes | yes | terminates own thread |
| exit_group | yes | yes | terminates own process |
| restart_syscall | yes | yes | resumes an interrupted admitted call |
| sched_yield | yes | yes | yields own CPU |
| getpid | yes | yes | own process identity |
| gettid | yes | yes | own thread identity |
| kill | yes | yes | positive pid exactly the worker's pid, baked into filter |
| tgkill | yes | yes | tgid and tid exactly the worker's pid/tid, baked into filter |

## Evidence

Argument-bearing traces and clock measurements are produced by
`trace-native.sh`, using native Linux, strace and a release worker. Tracing starts
before exec so setup can be distinguished from post-confinement calls at the
successful TSYNC `seccomp` call. Startup's procfs opens and loader calls are not
admitted. The scenarios exercise cold start, repeated activations, host-call
waits, allocator refusal, script errors, panic, abort, stack overflow and normal
teardown. The traced worker is the release binary, not the Rust test runner.

x86_64: see `traces/x86_64/` for native traces and `clock-cost.txt` for the measured
`CLOCK_THREAD_CPUTIME_ID` syscall cost: 95.94–103.39 ns/call across five batches
of 1,000,000 calls, on Linux 7.0.0 / glibc 2.43 / Rust 1.99. This cost is
descriptive, not a test bound. The 22 confined release engine children passed all
21 selected budget, IPC, clock and replay tests. Separate panic and abort probes
reached readiness and exited 101 and SIGABRT respectively.

Every post-filter call in this trace set was reviewed: `read(0)`, `write(1|2)`,
`brk`, `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, `futex(..., FUTEX_WAKE_PRIVATE, ...)`,
`getpid`, `gettid`, `tgkill(own pid, own tid, SIGABRT)` and `exit_group` convey only
the authority listed in the table. `observed-syscalls.txt` records counts before
trace reduction. All non-clock calls are retained; long CPU-budget traces retain
the first and last eight thread-clock samples and explicitly count the omitted
samples. Loader/setup calls before successful TSYNC are not part of these
post-confinement traces.

An initial trace exposed glibc's lazy vDSO `getrandom` state allocation using
MAP_DROPPABLE. That mapping mode was **not** admitted. The worker now primes that
single-thread libc state before step 0 audits mappings, keeping initialization
out of activations and keeping the exact MAP_PRIVATE + MAP_ANONYMOUS predicate.

aarch64: see `traces/aarch64/`, produced natively by CI's `aarch64-syscall-trace`
job on a GitHub `ubuntu-24.04-arm` runner (Linux 6.17 Azure / glibc 2.39 /
Rust 1.99 / strace 6.8; `tools.txt` has the exact versions). No emulated trace is
used. `clock-cost.txt` records the measured `CLOCK_THREAD_CPUTIME_ID` syscall
cost there: 281.12–281.92 ns/call across five batches of 1,000,000 calls. This
cost is descriptive, not a test bound. The 23 confined release engine children
passed all 22 selected budget, IPC, clock and replay tests, and the panic and
abort probes reached readiness and exited 101 and SIGABRT respectively.

Every post-filter call in the aarch64 trace set was reviewed against the table:

- `read(0)` and `write(1|2)`: stdio only, as on x86_64.
- `brk`: the private heap.
- `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`: the CPU-budget clock; every sample
  in the set uses that clock.
- `futex(..., FUTEX_WAKE_PRIVATE, ...)`: process-private wakeups only.
- `getrandom(buf, 16, GRND_INSECURE)`: one call per worker, seeding std's
  HashMap. glibc 2.39 has no vDSO `getrandom`, so on this runner the seed is a
  real syscall where the x86_64 trace (glibc 2.43) shows none. It returns random
  bytes into the worker's own memory and opens no device.
- `rt_sigprocmask(SIG_UNBLOCK, ...)`, `gettid`, `getpid` and
  `tgkill(own pid, own tid, SIGABRT)`: glibc's `abort`, signalling only the
  worker itself.
- `exit_group`: normal exit, the panic probe's 101, and exit 3 after the worker
  reports a broken channel on stderr.

None of these grants filesystem, network, executable memory, process creation
or other-process authority, so no call was removed from the worker and the
aarch64 column is unchanged. Workers the test parent kills appear as
`killed by SIGKILL`; no trace shows SIGSYS.

The intentionally admitted vDSO fallbacks need not appear on every machine.
Private allocator and signal plumbing convey no filesystem, network, executable
memory, process creation or other-process control authority. Unknown allocator
advice or futex operations stay fatal, even if a future glibc trace uses them.
