# Native Windows confinement experiment

This standalone, disposable experiment measures native authority, not QuickJS
behavior. It does not change basal's worker or claim a production sandbox.

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
privileges and has no restricting SIDs. The parent records both constructed and
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
`std::net` does when WSAStartup is denied. The workflow also stages, neutralizes
and restores the token-handle fence and proves a native test holding a real
TOKEN_QUERY handle goes red; restored native tests must pass. The raw mutation
proof is uploaded with the report.

Use an isolated runner. This intentionally launches an unsandboxed control,
probes public IPC endpoints, creates temporary fixtures and creates/deletes
the fixed `basal.spike.worker` profile. Concurrent runs under the same account
would race. A stale profile is an error, not silently deleted at startup.
