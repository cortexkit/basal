# The worker sandbox

basal runs every flow script in `ck-basal-worker`, a separate process. The
script is approved by the operator, but it is still untrusted input to a C
JavaScript engine (QuickJS), so basal assumes the engine can be subverted and
confines the worker so that a subverted worker can do little more than talk to
its parent over stdin and stdout. Every effect a flow has on the world is a
host call that the parent checks against the approved manifest and journals.
The worker holds no durable state.

The worker confines itself before it reads its first message from the parent.
The parent then accepts a worker only if its first message, `Welcome`, reports
the confinement this operating system requires. A worker that reports anything
else is killed before it sees any flow code.

This page describes, for each operating system, which layers apply, what a
confined worker can still do, and how the systems differ.

## Linux

### Layers

In order, before the first read:

1. **Startup checks.** The worker refuses to start unless `/proc` is a genuine
   procfs, it has exactly one thread, stdin is the read end and stdout and
   stderr are write ends of anonymous pipes, the `READ_IMPLIES_EXEC`
   personality is off, and no memory mapping is shared, writable and
   executable at once, or an executable stack.
2. **Descriptor close.** Every descriptor above stdio is closed, so nothing the
   parent might have leaked (a store, a socket) reaches the engine.
3. **`no_new_privs` and non-dumpable.** The worker can never gain privileges
   through exec, and other processes, its parent included, cannot ptrace it or
   read its memory through `/proc`.
4. **Landlock.** An empty ruleset that restricts every filesystem, TCP and
   scope right of the kernel's Landlock ABI. It is required by default; the
   module runs workers without it only when the operator configures the
   optional policy, and even then only when the kernel lacks Landlock.
5. **Descriptors are closed again** after Landlock, so the ruleset's own
   descriptor cannot survive.
6. **seccomp.** A fixed allowlist of system calls, with argument checks,
   installed on all threads. Any other call, or an admitted call with a refused
   argument, kills the whole process with SIGSYS. The handler cannot catch it.

**seccomp is the Linux boundary.** Landlock is a second layer, and no system
call is admitted because Landlock would stop it. Landlock alone leaves gaps:
it does not stop stat, readlink or getxattr on files outside any rule, UDP
traffic, netlink sockets, or connecting to path-based Unix sockets. The
seccomp allowlist kills all of those at their entry system call.

The module accepts a Linux worker only if its `Welcome` reports seccomp
installed and, under the default policy, a Landlock ruleset at the highest ABI
both the kernel and the worker support. It counts workers killed by SIGSYS in
`flow.health`, under `worker_confinement`, so a filter violation is visible to
the operator rather than looking like an ordinary crash.

### What a confined Linux worker can still do

The allowlist and its review are in
[`syscall-admission.md`](../crates/basal-worker/sandbox/linux/syscall-admission.md),
with native traces of the release worker. In short, the worker can:

- read stdin and write stdout and stderr, the descriptors it was started with;
- allocate, resize and release private, non-executable memory;
- read the realtime, monotonic and own-thread CPU clocks, and sleep;
- read random bytes from the kernel;
- wait on and wake private futexes, handle its own signals, and signal itself
  (an abort must still end it with SIGABRT, where the parent can see it);
- exit.

It cannot open files, create sockets, start processes or threads, make memory
executable, map shared memory, or signal any other process.

## macOS

### Layers

- **Seatbelt.** The worker applies the deny-by-default profile
  [`worker.sb`](../crates/basal-worker/sandbox/worker.sb) to itself with
  `sandbox_init` before its first read. The profile denies everything (files,
  network, process execution, service lookups) and allows only signals to
  the worker itself.
- **Descriptor close.** Every inherited descriptor above stdio is closed
  before the profile applies.
- **Hardened runtime.** Both binaries are signed ad hoc with the hardened
  runtime, a plain identifier and no entitlements, never `get-task-allow`, so
  a debugger cannot attach to the worker. See [deploy.md](deploy.md).
- **Disclaimed launch.** The module launches each worker disclaimed from its
  own privacy (TCC) grants, so flow code can never use a Files & Folders or
  similar grant the module holds. The module checks at startup that it can
  launch this way, and exits rather than ever launching a worker plainly.

The module accepts a macOS worker only if its `Welcome` reports Seatbelt.

### What a confined macOS worker can still do

Reads and writes on descriptors open before the profile applied are not file
opens, so stdio keeps working. The worker can allocate memory, read clocks,
and signal itself. **Thread creation is not blocked on macOS**: Seatbelt has no
rule for it. A script cannot start a thread, because the JavaScript runtime
exposes none, but a subverted engine could.

This page makes no claim about the Mach rights a macOS worker holds at launch,
or about executable memory in the macOS worker. That evidence has not been
published yet.

## Differences between the systems

| | Linux | macOS |
| --- | --- | --- |
| Boundary | seccomp allowlist, fatal SIGSYS | Seatbelt profile, denied calls fail |
| Second layer | Landlock | none |
| Threads | blocked by seccomp | not blocked |
| Ptrace and memory reads | blocked by non-dumpable | blocked by the hardened runtime |
| Privacy grants | not applicable | worker launched disclaimed |
| Health counter | SIGSYS deaths | none |

A denied operation on Linux kills the worker; on macOS the call fails and the
engine keeps running.

### CPU bound of a pooled worker

Each activation has a JavaScript CPU budget, measured on the engine thread's
CPU clock, and a wall-clock deadline (60 seconds by default); the parent kills
a worker that passes the deadline. Workers are pooled: a worker is bound to one
flow and reused for that flow's later activations until it is retired after 256
activations or after sitting idle for 10 minutes.

On Linux the engine thread is the worker's only thread, so the CPU budget
covers all of the worker's CPU use. On macOS a subverted engine could start
another thread whose CPU the engine thread's clock does not count, and that
thread could keep running after its activation returns. The CPU such a pooled
worker can use is therefore bounded only by time: the wall-clock deadline
while an activation runs, plus the idle-retire limit once the worker sits idle.

### Budget exhaustion and retirement

A worker that reports an exhausted CPU, memory or stack budget is retired and
replaced before the flow's next activation, because the engine's state after
such a failure is not trusted. A caught memory or stack failure does not retire
the worker: if the script catches the error and its activation completes
normally, the worker is reused for that flow.

## Windows

Windows confinement is not provided yet. The worker has no Windows sandbox,
and the module refuses any worker whose `Welcome` reports no confinement, so
no flow runs there.
