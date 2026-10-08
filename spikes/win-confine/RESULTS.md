# Native Windows confinement results

## Outcome

**The requested lockdown was not reached. Do not enable a Windows worker from
this prototype.** Process creation with all requested attributes succeeds, but
its restricted primary recipes terminate before Rust entry. There is therefore
**no measured full-lockdown residual**. An empty report is not a denial result.

The final round is [37840751756](https://github.com/cortexkit/basal/actions/runs/37840751756),
compiled from `9235748f1b6cd5e15fcbf01b4406cd17d7375b95`. Both Windows images
passed formatting, checking, seven native tests, thirteen Python tests, release
building with `-C target-feature=+crt-static`, import inventory and a live-token
mutation witness. Their runtime full-policy acceptance gates failed, correctly.

The plain and LPAC-only controls **did** produce complete measurements. Those
results, rather than an assumed Chromium residual, are the authority evidence
below. Final source, controls, failed birth attempts, diagnostics, attestation,
all successful/failed probes, imports and mutation proofs are preserved in:

- [measurements/windows-latest.json](measurements/windows-latest.json)
- [measurements/windows-2022.json](measurements/windows-2022.json)
- [measurements/manifest.json](measurements/manifest.json), with provenance and hashes

The retained JSON omits the duplicate input target array; every attempted target
and its exact result remain in each completed child's `probes`. Raw reports,
`dumpbin` output and host information are also in the run's artifacts. No user
file contents were read or printed.

## First design finding: birth tokens are not final tokens

1. The High ordinary initial token was requested at SecurityImpersonation (2),
   but querying the **assigned suspended thread token** returned
   SecurityIdentification (1) on both images. The child then exited
   `0xc00000a5` (`STATUS_BAD_IMPERSONATION_LEVEL`) before entry.
2. Lowbox process creation produced a **Low primary, S-1-16-4096**, even when
   the supplied primary had been explicitly constructed **Untrusted,
   S-1-16-0**. Constructed-token attestation alone would falsely claim the
   desired final integrity. The actual process token must be queried.
3. Low, same-package lowbox initial tokens with explicit same-access user/group
   restricting SIDs eventually assigned at level **2**, with the exact same
   AppContainer SID and no capabilities as the process. This cleared the
   impersonation-level failure but not loader initialization: both images
   terminated with `0xc0000142` (`STATUS_DLL_INIT_FAILED`).

The intended next phase is `RevertToSelf`, lower the actual primary to
Untrusted, close every temporary adjustment handle, verify no Token handles,
then read stdin. The code implements that phase, but **none of the restricted
birth candidates reached it**.

## API sequences and failure ledger

### What works

For the LPAC control the parent:

1. Dynamically loads broker-only Userenv/Ole32 APIs from System32.
2. Calls `CreateAppContainerProfile("basal.spike.worker", zero capabilities)`;
   receives the package SID and obtains the package folder.
3. Copies the same binary to `target/release/placed` and grants its directory
   read/execute for that SID, preserving existing ACLs.
4. Creates a never-resumed LPAC process to materialize the package namespace.
   Its token resolves the **actual** package registry key. The parent immediately
   reverts its temporary impersonation. Named section/event fixtures are created
   before measured children, with NULL DACLs; they are not inherited.
5. Enumerates pipes, RPC Control, global/session BaseNamedObjects and the package
   named-object directory from the parent, recording errors rather than hiding them.
6. Creates three anonymous pipes. Only child stdin/read and stdout+stderr/write
   are inheritable in `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`.
7. Calls `CreateProcessW`, extended startup, suspended, zero-capability security
   capabilities and `ALL_APPLICATION_PACKAGES_POLICY` opt-out, then resumes.
8. The child reverts before attestation/input, verifies `ERROR_NO_TOKEN`, reads
   its actual primary and handle table, and refuses any Token/unknown handle.
9. Deletes the profile at the end; `DeleteAppContainerProfile` returned HRESULT
   zero on both final runners. Successful file-create fixtures are removed after
   measuring. The deliberately leaked file remained empty.

`CreateProcessAsUserW` with the requested original full attributes also succeeds
without a privilege workaround. Its caller-derived restricted token qualifies
for the restricted-own-token exception. The exact owned job's limit readback and
`IsProcessInJob` membership check succeed before resume. What fails is execution
of the newly created child, not the process-creation call.

### Birth recipes tried

The failures listed below were observed on both images unless stated otherwise.
Variants remained separate diagnostics, never accepted as full confinement.

| Recipe / change | Exact observed outcome |
|---|---|
| Initial implementation, querying package-only/reserved information on ordinary tokens | `GetTokenInformation(size): Win32 87`; report was not written. That first message did not identify its class, so its exact class cannot be recovered. |
| Reserved LPAC information class 46 | Win32 `87` and NT `0xc0000003` (`STATUS_INVALID_INFO_CLASS`). See the claim-based attestation below. |
| Untrusted NULL-restricted primary; High ordinary same-access initial | Create succeeds; actual primary Low, assigned initial level 1; exit `0xc00000a5`. |
| Low NULL-restricted primary; Low `NtCreateLowBoxToken` initial with initially empty restricting set | Initial still assigned level 1; exit `0xc00000a5`. |
| Initial LPAC-born source, redundant Low setter through query/duplicate-only handle | `SetTokenInformation(TokenIntegrityLevel): Win32 5`. Skip the unnecessary setter; later source handles request full parent-only access. |
| Low NULL-restricted primary; Low native lowbox initial with user/group restricting SIDs | Assigned level 2, matching SID/capabilities; exit `0xc0000142`. |
| Low NULL-restricted primary; Low LPAC-born initial with user/group restricting SIDs | Assigned level 2, matching SID/capabilities and LPAC claim; exit `0xc0000142`. |
| All mitigations removed; job UI unchanged | `0xc0000142`. |
| Each single drop: dynamic code, Microsoft signature, Win32k, strict handles, extension points, image-load flags | Every variant: `0xc0000142`. |
| UI restrictions removed; all mitigations retained | `0xc0000142`. |
| All mitigations **and** UI restrictions removed | `0xc0000142`. |
| Preserve token-object DACL across filtering/duplication, rather than use the broker's default DACL | No change: both Low loader variants and all policy diagnostics still `0xc0000142`. |
| Bare `CreateProcessAsUserW`, LPAC-born restricted primary, no attributes/job/mitigations/handle list | Win32 `1314` (`ERROR_PRIVILEGE_NOT_HELD`), before loading. Parent has SeIncreaseQuotaPrivilege but no SeAssignPrimaryTokenPrivilege. This is a distinct privilege-gated control, not a DLL failure. |
| Bare caller-derived **non-lowbox** Low NULL primary + same-access Low non-lowbox initial | Create succeeds without 1314, then `0xc0000142`. |
| Full LPAC attributes; primary NULL set replaced by user/group restricting SIDs; deny-only groups retained | `0xc0000142`. NULL alone is **not** isolated as the cause. |
| Final: full LPAC attributes; true same-access primary with enabled groups, user/group restricting SIDs, zero privileges; Low matching LPAC initial | `0xc0000142`, before Rust entry. |

The final post-load route creates a new deny-only/NULL/Untrusted restricted token
and calls `NtSetInformationProcess(ProcessAccessToken)` on the current process.
**That call was not reached.** Neither its success nor an unprivileged replacement
floor is established. It would be incorrect to say that SeAssignPrimaryToken is
proven necessary for post-load replacement merely because the separate bare
LPAC creation control returned 1314.

### Other measured corrections

- Static broker-only Userenv/Ole32 imports loaded GUI/COM libraries into the child
  (including User32, GDI and Win32u) and produced 106/107 startup handles.
  Broker-only dynamic loading removed those child dependencies. Final controls
  have no User32/GDI/Win32u; the final DLL failure is therefore **not** explained
  by the earlier GUI import inventory.
- `GetAppContainerNamedObjectPath` returns a relative path. Passing it directly
  to `NtOpenDirectoryObject` returned `0xc000003b`
  (`STATUS_OBJECT_PATH_SYNTAX_BAD`). Prefixing the real session namespace fixed it.
- The package storage key uses **profile name**, not package SID. Resolving it
  under the initialized lowbox token avoids testing a nonexistent guessed hive.
- Rust `std::net` panicked when `WSAStartup` returned **10107**
  (`WSASYSCALLFAILURE`). Native Winsock probes preserve that result; subsequent
  socket attempts return **10093** (`WSANOTINITIALISED`). No connect was reached.
- LPAC automatically enables strict-handle checks (flags 3). The raw excluded
  inheritable-handle `WriteFile` terminates the isolated LPAC child with
  **`0xc0000008` (`STATUS_INVALID_HANDLE`)** immediately after its write marker.
  The plain isolated child returns Win32 **6**. The parent-owned fixture stays
  empty. Isolating this negative probe preserves the other measurements.
- Handle type/name queries can race transient snapshot handles and are unsafe
  with strict checks. The final inventory uses stable type indices plus global
  ObjectTypesInformation; it does not dereference arbitrary snapshot handles.

## Attestation actually measured

Abbreviations used below:

- `SID = S-1-15-2-523749759-298135514-2295813373-2963266676-3438459952-3726307614-2942093161`
- `PKG = C:\Users\runneradmin\AppData\Local\Packages\basal.spike.worker\AC`
- `BIN = D:\a\basal\basal\spikes\win-confine\target\release\placed`
- `OBJ = \Sessions\2\AppContainerNamedObjects\<SID>`
- `HIVE = \REGISTRY\USER\<runner user SID>_Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppContainer\Storage\basal.spike.worker`

The two runner user SIDs and all exact paths/handle values are retained in JSON.

| Property | LPAC-only control, both images | Full birth candidates |
|---|---|---|
| Primary type | TokenPrimary (1) | Parent queries type 1 before resume |
| AppContainer SID | Exact SID above | Matches assigned Low loader SID |
| Capabilities | Empty | Empty |
| Effective LPAC | `WIN://NOALLAPPPKG`, UINT64 `[1]`, flags 0 | Actual process claim `[1]`; LPAC-born loader also `[1]` |
| Requested class `TokenIsLessPrivilegedAppContainer` | Unsupported: 87 / `0xc0000003`, preserved in `lpac_query` | Same unsupported query; not fabricated as a successful API check |
| Integrity | Low, `S-1-16-4096` | Actual birth Low, including original supplied-Untrusted case |
| Restricting SIDs | Empty | NULL-only, or explicitly named diagnostic user/group set |
| Groups | Normal user/world/interactive/admin/logon groups, not deny-only; raw list in JSON | Original lockdown groups deny-only except integrity; enabled-group diagnostic explicitly different |
| Privileges | LUID 23 SeChangeNotifyPrivilege enabled; LUID 33 SeIncreaseWorkingSetPrivilege disabled | Lockdown birth zero privileges |
| Privilege-name lookup under LPAC | Win32 6; names above resolved from parent LUIDs | Raw LUIDs/attributes retained independently of name lookup |
| After `RevertToSelf` | No thread token, Win32 1008 | Child entry not reached |
| Token handles at attestation | None; unknown types also fail closed | No child attestation available |
| Mitigations | Dynamic 0, signature 0, image-load 0, Win32k 4, strict-handle 3, extension 0, child-process 0 | Required policies passed at creation, but no self-mitigation report |
| Job | None (control) | Owned handle: flags `0x2508`, active limit 1, memory 268435456, UI `0xff`, no breakaway; membership true |
| Initial token handle | No initial in LPAC control | Non-inheritable flag 0, excluded from actual three-handle list; successful CloseHandle before ResumeThread |

**Do not substitute parent birth attestation for the missing worker final
attestation.** The initial object's DACL is preserved across filtering and
impersonation duplication, but that did not make these birth recipes runnable.

## Measured residual: LPAC-only, not lockdown

Both images have **29 successful operations**, on the objects/operations below.
The table combines repeated opens of the same object but explicitly lists each
successful access form. `Q/Z` means both FILE_READ_ATTRIBUTES and Win32 desired
access 0; both actually grant `0x00100080` and return metadata. Desired access 0
is therefore **not** an access-free metadata probe.

Risk columns are **user-file data / state for a later worker / another worker /
network**. They describe granted authority, not a claim that a service protocol
was exercised. “Possible” or “unknown” is not classified harmless.

| Kind / successful object | Requested and actual access | Present | Risk assessment (files / state / peer / network) |
|---|---|---|---|
| `C:\Windows\System32\kernel32.dll` | Data-read `0x00100081`; Q and Z `0x00100080`, metadata succeeds | Both | System image bytes, not tested user-file data / no write / no direct peer / no |
| `C:\Windows\System32\cmd.exe` | Data-read `0x00100081`; Q and Z `0x00100080`, metadata succeeds | Both | System image bytes / no write / no direct peer / no; executable creation separately denied |
| `PKG` directory | List `0x00100081`; Q and Z `0x00100080`, metadata succeeds | Both | Can discover package state / read-only opens, but create below succeeds / shared-profile state visible / no direct network |
| `BIN` directory | List `0x00100081`; Q and Z `0x00100080`, metadata succeeds | Both | Can list placed image directory / no successful create there / no direct peer / no |
| `HIVE` | KEY_QUERY_VALUE `0x1` | Both | Package registry values, not general user files / reads later-worker state / shared-profile channel / no direct network |
| `HIVE`, value `win-confine-native-fixture` | KEY_SET_VALUE + NtSetValueKey(REG_BINARY), successful actual write | Both | No direct user-file read / **persistent registry state** / **cross-worker channel** for same profile / no direct network |
| `\RPC Control\epmapper` | Identification-only ALPC connect; client handle `0x001f0001` | Both | **Unknown brokered file authority** / unknown protocol state / possible brokered peer channel / **unknown brokered network authority**; no requests sent |
| `OBJ` | DIRECTORY_QUERY `0x1` | Both | No filesystem data / can discover shared names / shared namespace visible / no |
| `OBJ\Global` | DIRECTORY_QUERY `0x1` on latest; SYMBOLIC_LINK_QUERY `0x1` on 2022 | Both, different kernel object types | No direct files / namespace discovery / shared namespace visible / no |
| `OBJ\Local` | SYMBOLIC_LINK_QUERY `0x1` | Both | No direct files / namespace mapping discovery / shared namespace visible / no |
| `OBJ\RPC Control` | DIRECTORY_QUERY `0x1` | Both | No direct files / endpoint discovery / shared endpoints visible / network authority depends on services, not directory handle |
| `OBJ\Session` | SYMBOLIC_LINK_QUERY `0x1` | Both | No direct files / namespace mapping discovery / shared namespace visible / no |
| `OBJ\basal.spike.package.event` (NULL-DACL fixture) | EVENT_QUERY_STATE `0x1`; EVENT_MODIFY_STATE `0x2` | Both | No files / event state persists while retained / **bidirectional same-profile channel** / no direct network |
| `OBJ\basal.spike.package.section` (NULL-DACL fixture) | SECTION_QUERY `0x1`; SECTION_MAP_READ `0x4`; SECTION_MAP_WRITE `0x2` | Both | Shared bytes may contain other-worker data, not a direct file handle / state while section retained / **shared-memory channel** / no direct network; map rights opened, bytes not mapped in this test |
| `PKG\win-confine-896-lpac.txt` | CREATE_NEW, FILE_WRITE_DATA; grant `0x00100082` | Latest | No read tested / **created filesystem state** / later same-profile worker can see it / no direct network |
| `PKG\win-confine-8028-lpac.txt` | CREATE_NEW, FILE_WRITE_DATA; grant `0x00100082` | 2022 | Same state/channel assessment; exact PID-specific filename retained in report |
| `VirtualAlloc` | PAGE_EXECUTE_READWRITE succeeds | Both | No direct files / process-local lifetime / no peer by itself / no network; executable-memory guarantee absent in this control |
| `VirtualProtect` | RW to PAGE_EXECUTE_READ succeeds | Both | Same process-local assessment; dynamic-code mitigation absent in control |
| `CreateThread` | In-process thread succeeds | Both | Can run other reachable operations concurrently / lives until process ends / no peer by itself / no network by itself; wall deadline remains necessary |

The two global/session BaseNamedObjects fixtures did **not** open under LPAC;
only the package-namespace fixtures did. Their NULL DACLs deliberately measure
permissive same-profile objects, not a claim that arbitrary system objects have
those ACLs. The parent retains them across the controls to expose sharing.

No enumerated named pipe opened successfully under LPAC. No other enumerated
ALPC port connected successfully. Failed/refused/time-out opens are distinct
results in JSON; they are not all “access denied”. A finite enumeration and one
connection message format do not prove all IPC routes inaccessible.

### Ambient handles after lockdown of the LPAC control

The LPAC-only child had **34 handles on latest, 35 on 2022**, not just three.
Raw handle values/type indices/grants are preserved. Names are not queried on
transient handles, so unidentified endpoint identity remains an explicit gap.
The table covers every observed type/access combination (counts sum to totals).

| Type | Granted access | Latest | 2022 | Authority assessment |
|---|---|---:|---:|---|
| Event | `0x001f0003` | 7 | 7 | Full local event operations; private/unnamed identity not established; peer/state risk unknown if shared |
| IoCompletion | `0x001f0003` | 2 | 2 | Completion queue operations; thread/work scheduling and lifetime state |
| TpWorkerFactory | `0x000f00ff` | 2 | 2 | Thread-pool authority; reinforces that thread creation is not prohibited |
| IRTimer | `0x00100002` | 4 | 4 | Timer/wait authority; process resource behavior |
| WaitCompletionPacket | `0x1` | 6 | 5 | Wait/completion state; unknown identity, not a direct file/network grant |
| SchedulerSharedData | `0x1` | 1 | 0 | Scheduler shared state; exact semantics not exercised |
| Directory | `0x3` | 1 | 1 | Query/traverse namespace authority; identity unknown |
| Directory | `0xf` | 1 | 1 | Includes namespace creation rights; potential peer/state channel depending on identity |
| ALPC Port | `0x001f0001` | 1 | 1 | Existing IPC channel: **unknown file/state/peer/network broker authority**, and potentially cached startup security context |
| Mutant | `0x001f0001` | 1 | 1 | Synchronization state; cross-process use unknown |
| Semaphore | `0x001f0003` | 2 | 2 | Query/modify/wait state; cross-process use unknown |
| Semaphore | `0x00100003` | 0 | 2 | Query/modify/wait state; same uncertainty |
| File | `0x0012019f` | 2 | 2 | Beyond stdio; includes data read/write and attributes: **potential file/state/IPC authority**, object identity not established |
| File | `0x00100003` | 0 | 1 | Read/write data; beyond stdio, identity not established |
| File | `0x00100001` | 1 | 1 | Read data; beyond stdio, identity not established |
| File (stdin) | `0x00120189` | 1 | 1 | Intended parent-to-worker pipe; ordinary stdio contract |
| File (stdout/stderr) | `0x00120196` | 2 | 2 | Intended worker-to-parent pipes; ordinary stdio contract |

It would be unsound to call every unnamed handle harmless or equate absence of
a Token handle with absence of all cached credentials in ALPC/runtime objects.
The prototype establishes the inventory, not all object-specific operations.

## Control-run comparison

| Observation | Plain latest / 2022 | LPAC latest / 2022 | Required full |
|---|---|---|---|
| Attempted main probes | 2108 / 1897 | 2108 / 1897 | No worker entry |
| Successful operations | 1179 / 1027 | 29 / 29 | **Not measured** |
| Parent-observed loopback TCP/UDP | Accepted / received in both | Zero / zero | Not measured |
| Winsock path | Startup/socket/connect/send succeed | Startup 10107; socket 10093, connect/send not reached | Not measured |
| `cmd.exe` CreateProcessW | Succeeds | Win32 5 | Not measured |
| Open parent: query-limited / VM_READ | Both succeed | Both Win32 5 | Not measured |
| File create: TEMP / BIN / PKG | All succeed | TEMP and BIN deny 5; PKG succeeds | Not measured |
| Native package registry write | Succeeds | Succeeds | Not measured |
| Win32 guessed-HKCU package write | Succeeds as normal user | Win32 5, despite actual native hive write succeeding | Not measured |
| Executable allocation and RW-to-X | Both succeed | Both succeed | Not measured |
| CreateThread | Succeeds | Succeeds | Not measured |
| Excluded inheritable handle raw write | Win32 6 | Fatal `0xc0000008`, isolated report | Loader never reaches write |

The known conventional `PKG\SystemAppData\Helium\User.dat` candidate did not
exist for this unpackaged profile. Its actual registry key is under the user
Classes hive; `UsrClass.dat` was also probed. Missing candidate files are not
sandbox-denial evidence. Network results establish only the tested Winsock
path, not a bare NT AFD socket proof.

## Imports, delay imports and actual loaded paths

Final PE inventory is identical on both images (duplicate casing creates
multiple descriptors for Kernel32 and Winsock):

| Normal import descriptor | Imported symbols |
|---|---:|
| `ntdll.dll` | 26 |
| `kernel32.dll` | 54 |
| `ws2_32.dll` | 10 |
| `advapi32.dll` | 25 |
| `api-ms-win-core-synch-l1-2-0.dll` | 3 |
| `KERNEL32.dll` | 71 |
| `WS2_32.dll` | 8 |

**Seven normal descriptors; zero delay imports; zero non-System32 imports.**
All symbol/ordinal inventories are retained in the committed JSON and the
`dumpbin /imports /dependents` artifact. Userenv and Ole32 are broker-only
runtime resolutions, not normal/delay imports in the measured image.

The LPAC startup module list on latest is the executable plus System32:
`ntdll`, `KERNEL32`, `KERNELBASE`, `ws2_32`, `ucrtbase`, `RPCRT4`, `advapi32`,
`msvcrt`, `sechost`. On 2022 it is the executable plus System32:
`ntdll`, `KERNEL32`, `KERNELBASE`, `ws2_32`, `RPCRT4`, `advapi32`, `msvcrt`,
`sechost`, `bcrypt`. The raw full paths are retained. These are **control**
startup inventories; no full worker loaded-path attestation exists.

## Verification and mutation witnesses

Final native run: rustc/cargo **1.99.0**, seven native tests and thirteen Python
tests passed per runner; release builds use static CRT. Fmt/check/build and PE
inventory passed. Runtime acceptance remained red for the unsupported recipes,
not rewritten to accept a missing child.

The token-handle fence was staged, neutralized with `NON-VACUITY BREAK`, checked
against **a real live TOKEN_QUERY handle**, restored with checkout/touch, and
rechecked. Only the targeted test
`native::tests::live_token_handle_is_visible_to_handle_inventory` was executed
in the red run (one failed, six filtered); all seven native tests passed after
restoration. Git diff stat was non-empty during mutation and empty after.
Both raw mutation proofs are committed in the runner JSON.

The NULL restricting-SID validator also had a local mutation witness: only
`test_full_policy_requires_null_restricting_sid` failed; the seven other tests
then present stayed green. Live staged source was restored before committing.

## Recommended Windows design changes

1. **Keep Windows unsupported/fail-closed until a runnable primary-token sequence
   is measured.** Neither LPAC-only nor an impersonation-only lockdown is a
   replacement for the specified final primary. `RevertToSelf` would expose a
   broader primary; it must never become an escape hatch.
2. **Separate birth requirements from final requirements.** Query the actual
   suspended process and assigned thread, not only the input handles. Account
   for the observed Low integrity reset and Identification downgrade. Treat
   post-load Untrusted as a transition requiring an independently attested
   result, not a paper property of CreateRestrictedToken.
3. **Do not yet conclude a privileged service solves this.** The bare LPAC
   control needs a privilege the hosted account lacks, but the final enabled
   same-access birth still failed DLL init, so post-load primary replacement
   never executed. A privileged broker versus a revised unprivileged boundary
   is a design decision requiring another native campaign, not a result here.
4. **Classify LPAC by measured claim when the reserved API is unavailable.**
   Preserve class-46 errors and verify native `WIN://NOALLAPPPKG` UINT64 `[1]`,
   exact package SID and empty capabilities. Do not accept an arbitrary
   AppContainer, or silently label a failed getter as true.
5. **Minimize and inventory the worker's actual imports/startup objects.**
   Static CRT is not proof of no DLL side effects. Broker-only APIs should not
   load GUI/COM into the worker. Unknown ambient File, Directory and ALPC
   handles must be closed safely or explicitly contracted after object-specific
   probes; the LPAC control demonstrably has more authority than its stdio.
6. **Use profile isolation for any weaker floor.** LPAC-only can create a package
   file, write its registry and modify/map shared named fixtures. A reused
   fixed profile is a durable-state and cross-worker channel, not deny-all.
   Per-worker/per-flow profile and cleanup are necessary but do not alone prove
   the stronger no-state invariant or eliminate system IPC.
7. **Keep the initial handle lifecycle exact:** non-inheritable, never listed,
   successful close before resume; after revert/transition reject every Token
   or unidentified handle before input. Existing ALPC channels may still hold
   cached security context, which a handle-type check alone cannot rule out.
8. **Keep resource claims honest:** threads succeed in the measured control,
   and Windows thread creation is not a claimed denial. Whole-process wall
   deadlines and pool retirement must bound all threads, not just main-thread
   CPU. Job membership must use the parent's exact handle, never NULL/outer-job
   queries.
9. **Remaining coverage work:** identify ambient handle objects and exercise
   their operations; inspect all thread security contexts; test bare NT AFD,
   real ALPC protocols and collaborating worker endpoints; inventory modules
   after probes; scrub inherited environment/current-directory authority.
   This finite parent-enumerated sample is evidence, not a universal capability
   proof. The final DLL-init failure requires loader-level tracing before its
   cause can be asserted.
