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
loader impersonation, job, mitigations and child-process prohibition. If the
ordinary loader exits with STATUS_BAD_IMPERSONATION_LEVEL, the parent retries
that same full policy with a duplicated same-package LPAC loader token and
records both attempts. It never replaces the lockdown primary with the loader.

The loader token is a same-access approximation, not a literal implementation
of Chromium's token builder: it retains groups and integrity, disables maximum
privileges and has no restricting SIDs. The child records it, reverts before
input, and measures the primary token. The package-SID read/execute ACE is
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
`std::net` does when WSAStartup is denied.

Use an isolated runner. This intentionally launches an unsandboxed control,
probes public IPC endpoints, creates temporary fixtures and creates/deletes
the fixed `basal.spike.worker` profile. Concurrent runs under the same account
would race. A stale profile is an error, not silently deleted at startup.
