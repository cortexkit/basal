# Linux confinement probes

`tests/linux_probes.rs` runs a fresh worker for each attempted operation and
checks the readiness line before inspecting its exit status. Production probes
use the engine's `enter` path with required Landlock and seccomp, once without
and once with a checked SIGSYS handler installation. Only SIGSYS is a denial;
exit 70, a missing readiness line, an errno, or a different death fails the test.
The readiness line names both the operation and its raw libc call.

File probes use `openat`, `newfstatat`, `getdents64` and `readlinkat`. No directory
descriptor survives startup, so the `getdents64` probe uses fd 0 (the stdin pipe).
Seccomp must kill that listing at syscall entry rather than let the kernel
return ENOTDIR. Socket probes target `socket` for TCP, UDP, path-based Unix and
netlink families; they do not claim a refused connection demonstrates seccomp.
Process probes target execve, clone, fork, pthread_create, kill, ptrace and
process_vm_readv. The signal target is the live supervisor with harmless SIGCONT,
so a broken filter cannot kill the test runner. libc's fork and pthread_create
issue their kernel calls themselves; no std process or thread helper is involved.
The executable-mapping probes never require a writable-executable page:
`mmap` requests read-execute, and `mprotect` adds execute to a read-write page.
The writable page is prepared before readiness, so a setup failure is not a pass.
Shared mmap, io_uring_setup and userfaultfd are also denied at entry.

The unconfined controls use the same files and live listeners with
`--no-sandbox`, never a single layer removed. They prove open/create, TCP, UDP,
Unix sockets, live loopback connect, exec, clone/fork, pthread_create and executable
memory work on the runner. Kernel-sensitive io_uring, userfaultfd, netlink and
ptrace of a non-child have no positive-control requirement.

## Landlock's gap

Landlock-only probes check `applied_abi >= 4` before interpreting any result.
ABI 4 is necessary for TCP scoping; a lower ABI fails with the named
`landlock-tcp-scoping-unavailable` message. An outside-file open and a connection
to a live loopback TCP listener must return EACCES or EPERM and exit normally.
The test prints the runtime/applied ABI readback.

Landlock alone still permits stat and readlink on an outside file, UDP traffic,
and path-based Unix connections. The gap test records each success, receives the
UDP datagram and accepts the Unix connection. These are gaps, not capabilities
granted by the complete worker: seccomp kills their entry syscalls. getxattr and
netlink are also part of the documented Landlock gap, but are not exercised by
the Landlock-only gap test here. Abstract Unix sockets differ from path-based
Unix sockets and are subject to Landlock scope rights on newer ABIs.

## Attestation

`tests/linux_startup.rs` holds a production engine child before its first stdin
read. Without sending stdin, the supervisor waits for the child's seccomp-filter
count to exceed the supervisor's inherited count. This identifies the worker's
own filter even on a runner that already applies seccomp. The supervisor then
checks Seccomp=2, NoNewPrivs=1 and Threads=1. A separate
dumpable test attempts PTRACE_ATTACH and opening proc mem on that blocked engine:
they must return EPERM and EACCES. An unconfined probe held on stdin provides the
live attach and mem-open control, establishing that parent attach is permitted by
the runner's Yama policy. Only after attestation does the parent send Hello,
check the required-mode Landlock report in Welcome, and shut down the child.

Descriptor probes plant fds 3 and 64 in pre_exec. The inventory after inherited
descriptor close reports exactly [0, 1, 2]. Independently, after Landlock and
before seccomp, F_GETFD must yield EBADF for every fd 3 through 1023. Both normal
close_range and the forced legacy-close path are tested. The parent never tries
to list a non-dumpable child's proc fd directory.

On procfs hidepid runners a fallback logged to the test's stderr uses a probe's
self-status and confinement report; proc mem is invisible with ENOENT.
This fallback is not exact supervisor attestation. Set BASAL_REQUIRE_VISIBLE_PROC=1
to require the engine status readback and proc-mem EACCES on CI.

`tests/ipc.rs::required_linux_welcome_activation_and_shutdown` additionally
checks required-mode Welcome, completes one activation, and observes exit 0
after Shutdown through basal-testkit's Linux launcher. The module's production
ProcessSpawner acceptance test belongs to basal-module, not this worker-only
test target.
