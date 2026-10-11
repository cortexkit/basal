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

| | Linux | macOS | Windows |
| --- | --- | --- | --- |
| Boundary | seccomp allowlist, fatal SIGSYS | Seatbelt profile, denied calls fail | Untrusted LPAC restricted token, job and process mitigations |
| Second layer | Landlock | none | startup token, mitigation and handle attestation |
| Threads | blocked by seccomp | not blocked | allowed inside the worker |
| Ptrace and memory reads | blocked by non-dumpable | blocked by the hardened runtime | worker cannot open another process for query or memory reads |
| Privacy grants | not applicable | launched detached from the module's privacy grants | no capabilities; environment cleared except SystemRoot |
| Health counter | SIGSYS deaths | none | `fatal_status_deaths` for invalid-handle and access-violation exits |
| Host fs mounts and links | mounts traversed; in-root links allowed | mounts traversed; in-root links allowed | mounts and in-root links not traversed; every reparse point refused, including stat |
| Strict-handle misuse | not applicable | not applicable | fatal `0xc0000008`, not a returned error |

A denied operation on Linux kills the worker; on macOS the call fails and the
engine keeps running. On Windows most denied operations return an error, but
strict-handle misuse is fatal rather than an error the engine can catch.

### CPU bound of a pooled worker

Each activation has a JavaScript CPU budget, measured on the engine thread's
CPU clock, and a wall-clock deadline (60 seconds by default); the parent kills
a worker that passes the deadline. Workers are pooled: a worker is bound to one
flow and reused for that flow's later activations until it is retired after 256
activations or after sitting idle for 10 minutes.

On Linux the engine thread is the worker's only thread, so the CPU budget
covers all of the worker's CPU use. On macOS and Windows a subverted engine could start
another thread whose CPU the engine thread's clock does not count, and that
thread could keep running after its activation returns. The CPU such a pooled
worker can use is therefore bounded only by time: the wall-clock deadline
while an activation runs, plus the idle-retire limit once the worker sits idle.
Retiring or killing a worker ends its process and every thread in it, so a
stray thread lives no longer than its worker: until the worker is killed at a
deadline, or retired after the idle-retire period or its 256th activation.

### Budget exhaustion and retirement

A worker that reports an exhausted CPU, memory or stack budget is retired and
replaced before the flow's next activation, because the engine's state after
such a failure is not trusted. A caught memory or stack failure does not retire
the worker: if the script catches the error and its activation completes
normally, the worker is reused for that flow.

## Windows

### Layers and guarantees

The native launcher creates a GUI-subsystem worker suspended, with a
Less Privileged AppContainer (LPAC) primary token for the fixed profile
`cortexkit.basal.worker`, zero capabilities, no privileges, NULL-only
restricting SIDs, and every access group deny-only. The primary is Low at birth
so the loader can initialize. A matching Low impersonation token on the initial
thread is used only during startup. The environment is cleared except
`SystemRoot`; an explicit handle list passes only the three stdio pipes.

Before resume, the parent checks the actual suspended primary and initial
thread tokens, the owned job's limits and the child's membership in that job.
The job has kill-on-close, no breakaway, an active-process limit of one, and all
UI restrictions. Its process memory limit bounds **committed memory**, not
address space as Linux's `RLIMIT_AS` does. The flow and codemode profiles have
separate commit limits with headroom above their JavaScript heap budgets.
Dropping or killing a worker terminates its job and every thread in the process.

Before reading its first frame, the worker reverts the thread token, checks that
none remains, lowers its actual primary to Untrusted, and closes all Advanced
Local Procedure Call (ALPC) ports
and non-inheritable non-stdio File handles (including KsecDD on Server 2022).
It attests the actual token, required process mitigations and the handle table.
A failed check exits 70 with a named reason, never a weaker sandbox. The parent
accepts only a Windows `Welcome` with all five confinement fields true. Its
`fatal_status_deaths` health counter counts worker exits `0xc0000008` (invalid
handle) and `0xc0000005` (access violation), not refusal exit 70 or an intentional
parent kill.

The mitigations prohibit dynamic code, child-process creation, Win32k system
calls and extension points; require Microsoft-signed images; reject remote and
low-label images; prefer System32 when loading images; and permanently enable
strict handle checks.
Thread opt-out and remote downgrade of dynamic-code policy are forbidden.

A confined worker can read stdin, write stdout and stderr, allocate private
memory, read clocks, create threads inside itself and exit. The measured probes
deny file data and metadata opens, package-folder creates and native package-hive
writes, new IPC, process creation, access to another process, and network access
through Winsock, including loopback. Host calls remain the only authorized way
for a flow to affect the outside world: the parent checks their manifest grants.

### Residual handles and production measurements

Windows needs runtime handles beyond stdio. The startup allowlist fits the
entire inventory to **one image profile**: either the windows-latest ceilings
or the windows-2022 ceilings in the table below, never their union.
Stdio and `\KnownDlls` must be present at exactly their counts; all other rows
are ceilings. Unknown types, access masks, extra inheritable handles and excess
counts are refused. Each profile permits at most **26 handles**:

| Type | Access | windows-latest ceiling | windows-2022 ceiling | windows-latest production | windows-2022 production |
| --- | --- | ---: | ---: | ---: | ---: |
| File stdin | `0x120189` | 1 (exact) | 1 (exact) | 1 | 1 |
| File stdout/stderr | `0x120196` | 2 (exact) | 2 (exact) | 2 | 2 |
| Directory `\KnownDlls` | `0x3` | 1 (exact) | 1 (exact) | 1 | 1 |
| Event | `0x1f0003` | ≤7 | ≤7 | 6 | 6 |
| IoCompletion | `0x1f0003` | ≤2 | ≤2 | 2 | 2 |
| TpWorkerFactory | `0xf00ff` | ≤2 | ≤2 | 2 | 2 |
| IRTimer | `0x100002` | ≤4 | ≤4 | 4 | 4 |
| WaitCompletionPacket | `0x1` | ≤6 | ≤5 | 6 | 5 |
| Semaphore | `0x100003` | 0 | ≤2 | 0 | 2 |
| SchedulerSharedData | `0x1` | ≤1 | 0 | 1 | 0 |
| **Total** | | **≤26** | **≤26** | **25** | **25** |

The production counts and creator chains are committed in
[`windows-handle-provenance.md`](../crates/basal-worker/tests/data/windows-handle-provenance.md).
CI measurements counted 25 pre-input handles per production release worker
and traced their creators in a production build without test-only options that
can weaken the worker's startup checks. The images were `win25-vs2026` version
`20260925.250.1` and `win22` version `20261004.326.1`. An image change requires
remeasurement, not an automatic widening of the allowlist. Production used six
Event handles per image, one below each seven-Event ceiling.

`\KnownDlls` is shared and read-only (query and traverse). Every other residual
object beyond stdio is unnamed and was private to the worker in the measured
snapshots. Their access masks are **not all read-only**: events, queues, timers,
wait packets and semaphores carry process-local runtime authority. Closing the
pool's completion and synchronization handles crashes real pool work, so they
must remain open.

On images with SchedulerSharedData, the worker can duplicate its `0x1` handle
with `GENERIC_ALL` and obtain `0x000f0001`, the type's full rights. This
**self-escalation** affects its own private, unnamed scheduler object; no other
process held that object in the measurement. It is not a file, network, peer or
persistent-state capability. Closing it crashes the worker when the pool runs.

### Non-claims and profile reuse

These measurements do not establish denial of direct native access to the AFD
network device bypassing Winsock, or of KnownDlls-relative section opens.
Native API probing covered a finite set of operations, not every operation
against every residual handle type.
The CSR ALPC port is closed before input; whether it would accept messages is
not part of the residual claim. An unnamed handle is not, by itself, proof of
harmlessness; the inventory, creator chains and finite probes delimit the claim.

All workers reuse the `cortexkit.basal.worker` AppContainer profile. Profile
reuse is a state channel for a **weaker token** that can create package files or
write the package hive. The full token denies both operations; there is no
per-worker profile and no claim that LPAC alone gives the full boundary.

### Clock and thread bounds

The engine thread's CPU clock uses `GetThreadTimes`, kernel plus user time, with
scheduler-tick resolution. A sub-tick CPU budget is imprecise; the wall deadline
is the hard bound. Small budgets are not refused. A subverted engine can use
additional threads whose CPU is not counted by that clock. For a pooled worker,
those threads are bounded by the activation wall deadline plus idle retirement,
not by the engine-thread CPU budget. Killing or retiring the worker ends every
thread. Pooled workers are retired after 256 activations or 10 minutes idle;
extra threads cannot outlive that retirement or a kill at the wall deadline.

### Production placement requirements

Windows confinement is implemented, but production placement also needs SUBC,
the module-hosting daemon, to support Windows placement, an install-directory
read/execute ACE for basal's
AppContainer SID, and a deployment account with a loaded user profile. The
launcher does not add that install ACE in production; test harnesses grant it
only on their worker binary's directory. Store and log files must retain their
owner-only Windows DACLs. Users install with `ck setup` and update with
`ck upgrade`; `script/stage.sh` is not a Windows or Linux installer.
