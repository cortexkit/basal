# Windows confinement measurements

## Evidence status

Native measurement is in progress. No residual is inferred from a failed
launcher, and cross-compilation is not runtime evidence.

## Iteration ledger

- Run [37802630936](https://github.com/cortexkit/basal/actions/runs/37802630936),
  image source `137dd60d91baa0c8f37317691bbddfc9073b3b77`:
  Windows-latest passed formatting, checking, three native tests and a release
  build with `-C target-feature=+crt-static` (rustc 1.99.0). The measurement
  command exited 1 with `GetTokenInformation(size): Win32 87` (invalid
  parameter), before writing its report. That diagnostic lacked the token
  information class, so it cannot identify the failed class retrospectively.
  Attestation now skips package-only information for non-AppContainer tokens,
  records the exact class on errors, and can query the LPAC flag via NT when
  the Win32 dispatcher rejects it. A native test covers normal-token attestation.
- The same artifact's PE inventory had nine normal import descriptors
  (duplicate casing of Kernel32 and Winsock), and **zero delay imports**:
  `ntdll.dll` (18 symbols), `ole32.dll` (1), `kernel32.dll` (45),
  `ws2_32.dll` (1), `advapi32.dll` (22), `userenv.dll` (4),
  `api-ms-win-core-synch-l1-2-0.dll` (3), `KERNEL32.dll` (72),
  `WS2_32.dll` (15). These are preliminary, not the final image inventory.

## Working API sequence and attestation

Run [37807020190](https://github.com/cortexkit/basal/actions/runs/37807020190),
source `e905aff22941a7f75940117b78379fc7aa3ea010`, produced reports on both
Windows Server 2025 (26100) and Server 2022:

- `CreateProcessAsUserW`, all six full-policy attributes, `SetThreadToken`
  and resume succeeded. Neither runner needed a privilege workaround.
  The constructed primary token had only NULL restricting SID, Untrusted
  integrity and zero privileges; the initial token was impersonation-type
  with original groups/integrity and only SeChangeNotifyPrivilege.
  The full child exited `0xc00000a5` (`STATUS_BAD_IMPERSONATION_LEVEL`) before
  emitting any JSON. This is a startup failure, not a measured denial.
- Plain completed with 2,071 probes on latest and 1,827 on 2022.
  LPAC-only panicked on Rust's assertion that `WSAStartup` returns zero;
  Windows returned 10107 (`WSASYSCALLFAILURE`). Native Winsock calls now
  report that failure instead of losing the file/IPC evidence.
- `GetAppContainerNamedObjectPath` returned a **relative** path beginning
  `AppContainerNamedObjects\\S-1-15-2-…`. Sending that to
  `NtOpenDirectoryObject` returned `0xc000003b` (`STATUS_OBJECT_PATH_SYNTAX_BAD`).
  It now resolves under the runner's `\\Sessions\\<session>` prefix.
- The real package storage key is keyed by `basal.spike.worker`, not the
  package SID. The initialized lowbox token's registry API resolved it to
  `\\REGISTRY\\USER\\<user SID>_Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppContainer\\Storage\\basal.spike.worker`.
- The plain loader loaded Ole32, Userenv, User32, GDI, Win32u and COM/RPC
  transitively and held 106/107 handles before input. A static parent-only
  import contaminates the child role of the same executable. Userenv and
  Ole32 are now resolved dynamically only by the parent, and the child
  opens the broker-resolved hive via NT instead. This change tests the
  minimal child loader; it does not establish the cause of the earlier
  full child's exit until the next report.

- Run [37814149975](https://github.com/cortexkit/basal/actions/runs/37814149975),
  source `049675cdf42bc5ab26a48bce68d66f70ea2058fd`, confirmed on both images
  that the assigned ordinary loader token's actual level was **1
  (SecurityIdentification)** even though DuplicateTokenEx requested level 2.
  Its integrity remained High, above the Untrusted primary recipe. The
  original child again exited `0xc00000a5`. The Low/lowbox and self-lowering
  sequence is now an explicit alternative, not an assumed fix.
- Both images rejected token information class 46 via Win32 (`87`) and NT
  (`0xc0000003`, STATUS_INVALID_INFO_CLASS). Effective LPAC classification
  now falls back to the token's native security attribute `WIN://NOALLAPPPKG`
  (UINT64), while preserving both class-46 failures in the report. This is
  the classification used by native token inspection tooling; the query
  failure must not erase all other token fields.
- With broker-only DLLs removed, the plain startup inventory fell to 70/72
  handles. LPAC-only exited `0xc0000008` during measurement. Per-handle type
  and name queries can race DLL-owned background threads closing a snapshot
  handle; strict-handle policy can turn that invalid handle into process
  termination. Inventory now resolves the snapshot's stable type indices
  against ObjectTypesInformation without dereferencing transient handles.

- Run [37827072544](https://github.com/cortexkit/basal/actions/runs/37827072544),
  source `389438219c37a2c127c03ddcd8a02cde873b2f8e`, passed seven native tests,
  thirteen Python tests and the live-token mutation witness on both images.
  Native LPAC classification using `WIN://NOALLAPPPKG` worked.
  The actual lowbox primary was **Low (4096)** even when its input primary
  was constructed Untrusted (0): lowbox creation reset the integrity level.
  Both the High ordinary loader and the native Low non-LPAC lowbox loader
  were assigned at Identification level 1 and exited `0xc00000a5`.
  The LPAC-born candidate hit Win32 5 while redundantly setting an already
  Low loader token's integrity through a query/duplicate-only source handle.
  That redundant setter is now skipped. Initial same-access restricting SIDs
  now explicitly include the original user's SID and groups rather than an
  empty list.
- The same run's LPAC control still exited `0xc0000008` with race-free type
  inventory, so a snapshot race is **not established as its cause**. The
  deliberately invalid-handle write now runs in a separate identically
  confined worker and carries stage markers, preserving the main report even
  if strict-handle policy makes that required negative probe fatal.

Pending completed full-policy native reports.

## Measured residual and risk assessment

Pending completed native reports. A missing report means **not measured**, not
that all opens were denied.

## Control comparison

Pending completed native reports.

## Final import and loaded-image inventory

Pending the final measured artifact.

## Recommended Windows design changes

Pending native reachability and launch evidence.
