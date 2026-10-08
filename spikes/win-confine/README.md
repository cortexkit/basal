# Native Windows confinement experiment

This standalone, disposable experiment measures native authority, not QuickJS
behavior. It does not change basal's worker or claim a production sandbox.

**Measured outcome:** the full restricted birth recipes fail DLL initialization
on both native runner images; LPAC-only controls complete. The workflow's full
runtime gate intentionally remains red. See [RESULTS.md](RESULTS.md) for the
complete residual, failure ledger and the not-reached primary-replacement test.

Run from this directory on 64-bit Windows:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo check --locked
cargo test --locked
cargo build --release --locked
target\release\win-confine.exe --output report.json
python summarize.py report.json
```

The executable has parent and `--child` roles. The parent owns the profile,
fixtures and job. All three modes use an explicit three-pipe handle list as
transport plumbing. `plain` has no security layers; `lpac` adds only the
zero-capability LPAC identity; `full` adds the restricted primary token,
loader impersonation, job, mitigations and child-process prohibition. The
original Untrusted-at-birth recipe is recorded first. The next candidate starts
with a restricted Low primary and a Low, same-package, zero-capability lowbox
loader created by `NtCreateLowBoxToken`. If that fails, the same delayed-integrity
candidate uses an LPAC-born loader source. No birth mitigation or job limit is
removed in either candidate, and the lockdown primary is never replaced by the
loader token.

The loader token is a same-access approximation, not a literal implementation
of Chromium's token builder: it retains the source's groups, disables maximum
privileges and restricts against the same user's SID and groups so its legacy
access check is unchanged. The parent records both constructed and
actually assigned tokens before resume. The initial handle is explicitly
non-inheritable, excluded from the handle list and closed before resume. The
worker borrows its own primary's adjustment handle while loading, reverts before
any attestation, self-lowers to Untrusted when requested, closes the adjustment
handle, and refuses before input if any token handle remains. The package-SID
read/execute ACE is
confined to a copy of this binary in `target/release/placed`, never a user or
system directory. The deliberately permissive named section/event have a NULL
DACL and are not inherited. The deliberately inheritable writable file is
excluded from the handle list. Successful file-create fixtures are cleaned up
after measurement. Deleting the profile removes package state at the end.

A never-resumed LPAC process materializes the kernel's package namespace and
holds it alive while the parent inventories it. The parent queries its package
registry location under a temporary impersonation and reverts immediately.
Package-namespace fixtures are pre-created with NULL DACLs too. This makes
same-profile sharing measurable instead of accidentally testing a nonexistent
directory. This initializer is terminated before profile deletion.

Every attempted operation carries an access mask or operation name and an
exact Win32, HRESULT or NTSTATUS code. Unsupported native object types and
enumeration failures are **coverage gaps**, not successful denials. ALPC uses
identification-only security QoS and a 100 ms connection timeout; a timeout is
not a denial. Endpoints may disappear between enumeration and probing.
Directory listing access is stronger than merely reading attributes; section
query is weaker than mapping its bytes. Separate section read/write-map and
event-modify access requests distinguish read-only authority from IPC channels.
File and directory probes also request read-attributes and zero access, then
query metadata: a rejected data-read open must not hide a successful stat.
`validate.py` rejects missing measurements and mismatched launch-layer
attestation, but never rejects a measured residual merely because it succeeded.
Neither a finite endpoint list nor
one granted-access request proves all stronger access is unavailable.

The workflow runs only on `spike/windows-confinement`, measures both runner
images and uploads the raw report, host information, full import listing and
summary. `pe_inventory.py` independently inventories normal and delay imports
from the exact artifact. Import names alone do not prove loader resolution;
the child also inventories the actual module paths. Broker-only Userenv/Ole32
entry points are resolved dynamically in the parent: static imports of those
libraries load GUI/COM code into the child before any Rust code can revert.
Native Winsock calls record initialization failures instead of panicking like
`std::net` does when WSAStartup is denied. The raw excluded-handle write runs
in a separate identically confined child: strict-handle policy can terminate
that process with STATUS_INVALID_HANDLE instead of returning an error. Its
exit/error is recorded separately so the other reachability results survive.
The workflow also stages, neutralizes
and restores the token-handle fence and proves a native test holding a real
TOKEN_QUERY handle goes red; restored native tests must pass. The raw mutation
proof is uploaded with the report.

Use an isolated runner. This intentionally launches an unsandboxed control,
probes public IPC endpoints, creates temporary fixtures and creates/deletes
the fixed `basal.spike.worker` profile. Concurrent runs under the same account
would race. A stale profile is an error, not silently deleted at startup.

## Loader and Chrome reference measurements

The workflow enables image-specific loader snaps (`GlobalFlag=2`) for
`win-confine.exe` and removes the IFEO key in a `finally` block. The full-policy
`loader-trace` diagnostic requests `DEBUG_ONLY_THIS_PROCESS`; if creation refuses
that flag, it retries suspended and attaches before resuming. The creating thread
records every debug string, DLL load/unload and exception event while separate
threads drain the pipes. The x64 child PEB's `NtGlobalFlag` is read back: if the
registry setting did not set bit `0x2`, the parent sets only that diagnostic bit
before resuming and records the before/after values. This fallback changes no
token, ACL or mitigation and is not a production launch sequence. Debugger-assisted
startup is not accepted as a production confinement result.

`chrome_reference.ps1` launches installed Chrome headless with a fresh profile
and no sandbox-disabling switches. It records the observed descendant command
lines and invokes `win-confine.exe --observe <pid> --broker <browser-pid> --output
<file>` for renderer, GPU, network and browser processes. Inspection is from the
parent, using the same token and handle-table routines as the worker. A second
process-tree sample is taken ten seconds after the first renderer observation to
avoid presenting its startup impersonation token as the settled state. A timer
alone does not prove every transition completed. The
selected thread is the earliest surviving thread by creation time: Windows
Toolhelp does not identify a main thread. Exact Chrome job limits are queried
through duplicated browser-owned Job handles after checking target membership;
outer runner jobs may remain unavailable. Query errors are coverage gaps, not
evidence of absent authority. Process and handle observations are snapshots, not
a census of every process or transient handle during startup.

`compare_reference.py evidence evidence/reference-diff.json` compares every
renderer primary-token field with the failing actual birth token and a separately
labelled intended final token. It retains raw lists and flags; failed queries
produce unknown comparisons rather than an equality or a denial result.

Two additional diagnostics retain the enabled session logon SID observed in
Chrome while keeping the NULL restricting SID, removed primary privileges,
job, mitigations, child-process restriction and handle list. One retains LPAC;
the other omits lowbox attributes and uses a non-AppContainer Low loader token.
Both run the ordinary complete probe path if they pass entry, self-lowering and
handle attestation. They remain diagnostics: retaining an access group or
omitting LPAC is not the intended deny-all final token.

The observer also reads the x64 process parameters (current directory, desktop,
console handle, environment variable names and selected path values). Other
environment values are omitted because runners may hold credentials. User32 is
resolved dynamically only in the broker for station/desktop ACL queries, so it
does not become a worker import. KnownDlls and session BaseNamedObjects ACLs are
sampled from the broker. `AccessCheck` evaluates their DACLs against a duplicate
of the actual primary token; it is neither an object operation nor a mandatory
integrity check. These samples do **not** identify the objects KernelBase opened
inside its attach routine. CSR/console endpoint identity and an NtOpen* trace
remain explicit gaps.

The additional loader-privilege diagnostic retains disabled source privileges
on the non-AppContainer initial token. A final diagnostic changes only that
variant's primary `TokenDefaultDacl` to the user/Administrators/SYSTEM full-access
ACL observed on Chrome. TokenDefaultDacl controls default security for newly
created objects; it is not the DACL protecting the token object itself. Neither
diagnostic is promoted to the full acceptance path.
