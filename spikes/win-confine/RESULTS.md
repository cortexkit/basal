# Native Windows confinement results

## Outcome

**The NULL-only, all-deny-only LPAC primary now reaches entry and is attested
Untrusted before input**, both as a detached console image and as a GUI-subsystem
image without `DETACHED_PROCESS`. The earlier `0xc0000142` failures were not proof
that restricted birth tokens cannot load: removing console initialization is
sufficient in the measured comparisons. **Do not enable a Windows worker from
this prototype yet.** The successful recipes retain **27 / 28 ambient handles**,
including an ALPC port and, on 2022, a non-stdio read/write File handle. They do
not establish the requested three-stdio-only boundary.

The final recommendation is the **GUI-subsystem, NULL-only LPAC recipe with
Low birth and an attested post-load Untrusted transition**, not the failed
post-load primary-replacement route or non-AppContainer Untrusted birth. Its
measured residual and the comparison with weaker controls are in
[the final campaign](#final-campaign-original-lockdown-and-gui-subsystem).
Earlier sections below describe their named historical campaigns; their missing
entry reports are not the final outcome.

The earlier control/residual baseline is [37840751756](https://github.com/cortexkit/basal/actions/runs/37840751756),
compiled from `9235748f1b6cd5e15fcbf01b4406cd17d7375b95`. Both Windows images
passed formatting, checking, seven native tests, thirteen Python tests, release
building with `-C target-feature=+crt-static`, import inventory and a live-token
mutation witness. Their runtime full-policy acceptance gates failed, correctly.

The plain and LPAC-only controls **did** produce complete measurements. Those
results, rather than an assumed Chromium residual, are the authority evidence
below. Baseline source, controls, failed birth attempts, diagnostics, attestation,
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
then read stdin. The code implements that phase, but **none of the baseline
console-birth candidates reached it**. Console-free results are reported below.

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

Earlier baseline native run: rustc/cargo **1.99.0**, seven native tests and thirteen Python
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

## Loader trace and Chrome reference

The final follow-up is [37848564991](https://github.com/cortexkit/basal/actions/runs/37848564991),
compiled from `2cc7c91c6118e640a94e717279a206cfc7d06994`. Both images passed
fmt/check, **seven native tests, sixteen Python tests**, static-CRT release build,
PE inventory and the live-token fence mutation witness. Runtime full-policy
acceptance remained red; the diagnostics were not substituted for that gate.

Native observations, field-by-field token comparisons, every renderer's raw
handle table and type/access counts, GPU/network/browser contrasts, startup
parameters/ACL samples, failed attempts and complete control probes are retained:

- [measurements/loader-chrome/final-windows-latest.json](measurements/loader-chrome/final-windows-latest.json)
- [measurements/loader-chrome/final-windows-2022.json](measurements/loader-chrome/final-windows-2022.json)
- [loader-windows-latest.txt](measurements/loader-chrome/loader-windows-latest.txt)
  and [loader-windows-2022.txt](measurements/loader-chrome/loader-windows-2022.txt)
- [manifest.json](measurements/loader-chrome/manifest.json), with all five run IDs,
  compiled-source SHAs, artifact-file hashes and retained-file hashes.

The first-run JSON is retained separately because it caught a different Chrome
startup state. Final JSON omits only the duplicate input `targets` array from the
runtime report; completed probe results retain their exact targets and accesses.

### Exact loader finding

Image-specific IFEO `GlobalFlag=0x2` alone did not produce snaps for the restricted
child. Both DWORD and REG_SZ attempts were silent. In the final REG_SZ run the
**parent** PEB read back `0x2`, but the suspended child read back `0x0`. The parent
therefore wrote only the diagnostic `0x2` bit to that child's x64 PEB and verified
`0x2` before resume. No token, ACL, mitigation or application instruction was
changed in the traced attempt. Both runners removed the image-specific IFEO key;
retained cleanup output says `IFEO exists after cleanup: False`.

`CreateProcessAsUserW(DEBUG_ONLY_THIS_PROCESS)` succeeded on both images. Neither
pre-resume attach nor ETW was needed. The event loop completed without errors,
with **172 events / 165 debug strings** on latest and **154 / 147** on 2022. It
recorded all output strings and DLL load/unload events through process exit.

The decisive sequence is the same on both:

1. Current-directory initialization for `D:\a\basal\basal\spikes\win-confine\`
   fails with **`0xc0000022` (`STATUS_ACCESS_DENIED`)**. The loader continues.
2. Known-DLL lookup and mapping of `KERNEL32.DLL` and `KERNELBASE.dll` return
   **`0x00000000`**. This is not a missing-DLL or failed image-mapping result.
3. `LdrpThreadTokenUnsetMainThreadToken` reports **`Status: 0x0`**.
4. `LdrpInitializeNode` calls the init routine for
   **`C:\Windows\System32\KERNELBASE.dll`**, then reports that the routine
   **failed during `DLL_PROCESS_ATTACH`**.
5. Kernel32 and KernelBase unload. `LdrpLoadDllInternal` first returns
   **`0xc0000142`**, propagated through process initialization and exit.

Thus the exact failing DLL is **KernelBase**, and the last reported NTSTATUS
before the first `0xc0000142` is **success (`0x0`)**, not a disclosed internal
KernelBase failure status. The earlier cwd access denial is real, but the trace
**does not establish that it causes the attach failure**. The latest loader also
logs resolved `0xc0000135` cache misses before successful Known-DLL mapping.

Importantly, outside token queries at the token-unset message **and** the
`Calling init routine` message still find the assigned Low LPAC impersonation
token at level 2. The log line is **not evidence of `RevertToSelf`**. These
snapshots do not reveal every token switch inside KernelBase's routine. Its
internal failing syscall/object remains unmeasured.

### Chrome renderer token: startup is not the settled state

Chrome was already installed, so no `choco` install was needed. The final images
have Chrome **154.0.8037.58** (latest) and **154.0.8037.98** (2022). Each launch used
`--headless=new`, a fresh temporary profile and `about:blank`, with no
sandbox-disabling switches. Descendant command lines identify seven renderers
per image. The final snapshot was taken ten seconds after the first observed
renderer; subsequent outside queries are sequential snapshots, not simultaneous
or a census of every startup process. The selected thread is the earliest
surviving thread by creation time, since Toolhelp has no main-thread flag.

The immediate first-run snapshot caught Low primary tokens and Low level-2
same-access thread tokens. Those initial thread tokens retained 24 privileges,
with only SeChangeNotifyPrivilege enabled. In the later final sample, **all 14
renderers are Untrusted and have no selected-thread token (`ERROR_NO_TOKEN`,
1008)**. A Low/thread-token startup sample must not be described as Chrome's
final sandbox.

| Primary field | Chrome, all final renderers | Basal failing LPAC birth | Basal intended final, not reached |
|---|---|---|---|
| Type / impersonation level | Primary (1) / not applicable | Same | Same |
| User SID | Same runner user as the broker | Same exact per-run user SID | Same |
| Integrity | Untrusted, `S-1-16-0` | Low, `S-1-16-4096` | Untrusted |
| AppContainer / SID | **False / NULL** | True / basal package SID | True / basal package SID |
| LPAC | Not an AppContainer; claim getter not queried | `WIN://NOALLAPPPKG` UINT64 `[1]` | Required, unmeasured |
| Capabilities | Empty | Empty | Empty |
| Restricting SIDs | NULL SID only, `S-1-0-0`, attributes `0x7` | Same | Same |
| Access groups | 12 deny-only; **session logon SID enabled**, `0xc0000007` | 13 deny-only; logon SID `0xc0000010` | All access groups deny-only |
| Integrity group | Untrusted, attributes `0x60` | Low, attributes `0x60` | Untrusted |
| Privileges | Empty | Empty | Empty |
| TokenDefaultDacl | `D:(A;;GA;;;LA)(A;;GA;;;BA)(A;;GA;;;SY)` | BA/SY full access, logon read/execute, package full access; **no user full-access ACE** | Constructed source-derived DACL is in JSON; final DACL unmeasured |
| Selected thread | No token | Assigned Low matching LPAC token, level 2 | No token after revert, unmeasured |

`LA` is the runner's local Administrator user SID, `BA` Administrators and `SY`
SYSTEM. The initial Low Chrome token's enabled logon SID was also observed in the
first sample. Chrome's measured renderer is **not LPAC**: this reference does not
prove that an LPAC plus deny-all primary can load. TokenDefaultDacl is the default
security for newly created objects, **not** the DACL protecting the token object.
The baseline preserved token-object DACLs to avoid dropping the package SID's
query grant; that change did not fix DLL initialization or inspect TokenDefaultDacl.

### Chrome mitigations, jobs and handle inventory

Raw policy words below are identical across each image's seven final renderers.
Failed getters are not zero-policy measurements.

| Mitigation policy | Latest | 2022 |
|---|---|---|
| DEP | `[3, 1]` | `[3, 1]` |
| ASLR | `5` | `5` |
| Dynamic code | **`0`** | **`0`** |
| Strict handle | `3` | `3` |
| Win32k | `5` | `1` |
| Extension points / CFG / font / child process / side channel | Each `1` | Each `1` |
| Signature | `5` | `5` |
| Image load | `3` | `3` |
| System-call filter / payload / redirection trust | Each `0` | Each `0` |
| User shadow stack | `256` | `256` |
| User pointer auth | Getter fails Win32 `50` | Successful `0` |
| SEHOP | Successful `1` | Getter fails Win32 `87` |

Each renderer belongs to an exact Chrome-owned job, read through a duplicated
browser Job handle and checked with `IsProcessInJob`. Both images report flags
**`0x2508`**, UI **`0xff`**, process memory **1,099,511,627,776 bytes**, job memory
0 and active-process limit 0. Basal's owned job uses the same flags/UI but requests
256 MiB and active limit 1. Outer runner job limits are not recovered by this
method. Chrome's dynamic-code policy is zero, unlike the required basal ban;
matching its token is not equivalent to matching all our policies.

| Latest renderer PID | Handles | 2022 renderer PID | Handles |
|---:|---:|---:|---:|
| 8440 | 268 | 3468 | 265 |
| 1104 | 221 | 6932 | 218 |
| 7908 | 225 | 6264 | 222 |
| 2800 | 229 | 8020 | 226 |
| 2384 | 203 | 6380 | 200 |
| 4716 | 229 | 7312 | 226 |
| 6952 | 225 | 7436 | 222 |

The inventory includes ALPC Port, Directory, EtwRegistration, Event, File,
IRTimer, IoCompletion, Key, Mutant, Section, Semaphore, Thread, TpWorkerFactory
and WaitCompletionPacket; latest also has SchedulerSharedData. Every raw handle,
type/access combination and count is retained under `handles` and
`handles_by_type_access`. No Token or unknown type appeared in these renderer
snapshots. Hundreds of ambient handles are **not** a three-stdio-only reference,
nor evidence that unidentified File/ALPC/Section handles are harmless.

GPU and network processes were inspected with the same outside code, as contrast
only. GPU: Low, non-AppContainer, five restricting SIDs (Users, World, Restricted
Code, a unique SID and logon SID), SeChangeNotifyPrivilege enabled, **357 / 301
handles**. Network service: command line explicitly has
`--service-sandbox-type=none`, High (`S-1-16-12288`), non-AppContainer, no restricting
SIDs, 24 privileges, **344 / 380 handles**. Their complete thread, group,
mitigation, job and handle reports are retained, but they are not renderer policy
or evidence for basal confinement.

### Additional startup-context differences

| Field | Chrome renderer | Failing basal child / limits of measurement |
|---|---|---|
| Primary default DACL | User + Administrators + SYSTEM full access | User ACE missing; matching Chrome's exact DACL still leaves the child failing before Rust entry |
| Station / desktop | `Service-0x0-d8957$\sbox_alternate_desktop_0x1570` on latest; `Service-0x0-9e1ae$\sbox_alternate_desktop_0x1170` on 2022 | `winsta0\default`. Station ACLs and basal desktop ACL are retained. **Chrome desktop ACL not read** because the observer belongs to another station and did not switch its own station. Its name is observed, not its ACL. |
| Current directory | Versioned Chrome install directory under `C:\Program Files\Google\Chrome\Application\` | Inherited repository spike directory; its loader initialization returns `0xc0000022`. No tested cwd grant/change or proof of Chrome's final cwd-open authority. |
| Environment block | Seven variable names: CHROME_CRASHPAD_PIPE_NAME, LOCALAPPDATA, PATH, SYSTEMDRIVE, SYSTEMROOT, TEMP, TMP | 147 inherited runner variable names. Actual LPAC birth TEMP/TMP point at package `AC\Temp`. Console parameter is 0 for Chrome and `0xfffffffffffffffd` for basal. Names and selected path values are retained; other values are omitted to avoid publishing credentials. |
| Named attach objects / ACLs | Samples of `\KnownDlls` and `\Sessions\2\BaseNamedObjects` | Same objects sampled against basal primary. **Not an NtOpen* trace:** which objects KernelBase opens, CSR/console endpoint identity and their attach-time security contexts remain unknown. |

For the KnownDlls and session-directory suspects, DACL-only `AccessCheck` used
impersonation-token duplicates of each actual process primary and denied directory
query (`1`), traverse (`2`) and READ_CONTROL (`0x20000`) on those two directories
for Chrome renderers and the failing NULL-restricted basal primary. Those checks
**cannot be called failed loader operations**: Chrome runs and basal's Known-DLL
mapping explicitly succeeds. The plain positive control grants all six checks;
LPAC-only grants the KnownDlls checks but not the session-directory checks. This
also demonstrates the evaluator is not a constant-denial stub. Window-station
checks and all raw ACLs are preserved; neither DACL-only evaluation nor an ACL
sample includes mandatory-integrity enforcement or establishes an object's use
inside KernelBase.

### Applied property changes and final outcome

All variants retain the three-handle list, child-process restriction, full
mitigation mask and exact owned job. Non-AppContainer variants intentionally
omit only lowbox attributes and use a matching non-AppContainer Low initial
source; they remain separate diagnostics, not full LPAC acceptance.

| Additional attempt | Both native images |
|---|---|
| Enable only the session logon SID; LPAC retained | Create succeeds; Low primary attested; initial level 2; `0xc0000142`, no Rust-entry marker |
| Same enabled-logon restricted Low birth, without AppContainer/LPAC | Same pre-entry failure |
| Non-AppContainer, enabled-logon variant; initial impersonation token retains all 24 broker-source privileges (only SeChangeNotify enabled) | Same pre-entry failure; primary privileges remain empty |
| Non-AppContainer, enabled-logon, retained-loader-privilege variant; **match exact Chrome primary TokenDefaultDacl** user/BA/SY full-access grants | Parent readback exactly matches Chrome; same pre-entry `0xc0000142` |

**None of that loader/Chrome campaign's variants reaches Rust entry, so it
measures no new full-policy or non-AppContainer reachability residual.** The
normal complete probe path is wired for each variant,
but never executes. No empty residual is substituted for the LPAC-only table.
These measurements isolate KernelBase attach and falsify logon-only, non-lowbox
plus logon, retained-loader-privilege, and primary-default-DACL fixes as sufficient
in this recipe. They do **not** identify one proven root-cause property. Alternate
desktop/station setup, cwd/environment initialization and KernelBase's internal
object opens remain real differences or coverage gaps requiring another native
investigation; no install-directory grant or desktop fix is claimed tested.

## Process Monitor and startup-context campaign

The first capture run is [37851610568](https://github.com/cortexkit/basal/actions/runs/37851610568),
compiled from `a8b40b77e3e678bd98dc3688401c5dd6631cc50e`. Both administrator runners
downloaded Sysinternals Process Monitor **4.11** from
`https://download.sysinternals.com/files/ProcessMonitor.zip` and accepted its EULA.
The archive SHA-256 was
`80a6442b46af762ed1432f6fec3f7e20366bed62a2522b3486503398a40a1128`; the
`Procmon64.exe` SHA-256 was
`fc3af5317c707e0555ad6e7590ad65ceb5c5085b053b41221944ac3ca3492d9c`.
Capture used `/Quiet /Minimized /BackingFile`, with `/WaitForIdle` before worker
birth, `/Terminate` after the measurements, and `/OpenLog /SaveAs` for CSV export.

The first run retained PML files, but **did not consume a CSV**: PowerShell did
not wait for the GUI exporter and its `LASTEXITCODE` still held the measured
child failure. Neither that stale `1` nor the mere existence of a PML proves
an exporter failure or a denied operation. Subsequent capture orchestration
waits for each exact GUI process and reads its own exit code; it also uses
`/NoFilter`. The first run's birth observations remain valid independently of
that export error.

### Chrome-like context: measured changes, not ACL equivalence

The parent creates a unique `basal_spike_<pid>_<timestamp>\\worker` station and
desktop with broker-only dynamic User32 calls, then restores its original
station. The worker receives the explicit `STARTUPINFO.lpDesktop` value; no
station/desktop handle is inherited. The station grants read/execute to the
logon SID, NULL restricting SID and package SID; the desktop grants
`READ_CONTROL | DESKTOP_READOBJECTS | DESKTOP_WRITEOBJECTS` (`0x20081`) to those
SIDs. SYSTEM, Administrators and the broker user have full access. Both objects
have a Low no-write-up label. Requested SDDL and suspended-process station DACL
readback are retained. These grants are **not claimed to be Chrome's exact
ACLs**: its desktop ACL was not recovered, and Chromium's public broker code
copies existing station/desktop DACLs rather than adding a NULL-SID grant.

The disposable install directory grants NULL/package read/execute with a Low
label; a per-worker TEMP directory grants NULL/package full access with a Low
label. No ancestor directory ACL is changed. The Unicode environment has only
the seven observed Chrome names plus `windir`: `CHROME_CRASHPAD_PIPE_NAME`
(empty, no Crashpad endpoint), `LOCALAPPDATA` (the per-worker directory), `PATH`
(System32), `SYSTEMDRIVE`, `SYSTEMROOT`, `TEMP`, `TMP`, and `windir`. Suspended
process-parameter readback confirms these eight names and the install cwd. LPAC
creation rewrites TEMP/TMP below the supplied directory to
`Packages\\basal.spike.worker\\AC\\Temp`; that rewriting is retained, not hidden.

| Run 37851610568 attempt | windows-latest PID / exit | windows-2022 PID / exit | Rust entry |
|---|---|---|---|
| Enabled-group same-access LPAC; original context | 1440 / `0xc0000142` | 7396 / `0xc0000142` | Neither |
| Chrome-default-DACL non-AppContainer; original context | 6180 / `0xc0000142` | 2824 / `0xc0000142` | Neither |
| Same Chrome-style token plus alternate station/desktop, scrubbed environment, install cwd | 1236 / `0xc0000142` | 7012 / `0xc0000142` | Neither |
| Same enabled-group LPAC token plus the changed context | 9936 / `0xc0000142` | 5376 / `0xc0000142` | Neither |

Both images passed fmt/check, seven native tests, nineteen Python tests,
static-CRT release build, PE inventory and the live-token mutation witness.
The acceptance gate requires a completed LPAC child with an actual Untrusted
primary; it still failed because that child never entered Rust. No empty child
report is treated as a residual.

### Complete Process Monitor startup windows

The synchronized run is [37852312584](https://github.com/cortexkit/basal/actions/runs/37852312584),
compiled from `e4442517771c7fad669d3ee036b57fd2fcb1e04e`. Process Monitor produced
**226 events / 90 non-success rows** on latest and **212 / 82** on 2022 for the
four requested children. Every PID has `Process Start` and `Process Exit`.
`/WaitForIdle`, `/Terminate` and CSV export returned 0. The capture instance
itself returned 1 on latest and is recorded separately; that did not prevent
complete export. Version and hashes match the first run. WPR fallback was not
needed for these windows.

**Every non-SUCCESS operation, including reparses, missing optional values,
buffer-sizing responses and image-mapping responses, is listed in capture order
with exact path, operation, result and detail in these Process Monitor tables:**

- [Latest: all 90 rows](measurements/procmon/37852312584-windows-latest-procmon-events.md)
- [2022: all 82 rows](measurements/procmon/37852312584-windows-2022-procmon-events.md)
- The corresponding `procmon-events.json` files retain those rows and last denial
  per PID. `procmon-child-rows.json` also retains successes; only successful
  `Process Start` environment values are omitted to avoid publishing inherited
  credentials. [manifest.json](measurements/procmon/manifest.json) gives compiled
  SHAs and raw-artifact/retained-file hashes. Raw PML stays in the run artifacts.

| Recipe | Latest PID / events / last ACCESS DENIED | 2022 PID / events / last ACCESS DENIED |
|---|---|---|
| Chrome-default-DACL non-AppContainer | 9860 / 74 / **none** | 5516 / 71 / **none** |
| Enabled-group same-access LPAC | 5296 / 39 / `CreateFile`, spike cwd | 5904 / 38 / `CreateFile`, spike cwd |
| Chrome token plus changed context | 2184 / 72 / `CreateFile`, install cwd | 1996 / 65 / `CreateFile`, install cwd |
| Enabled-group LPAC plus changed context | 2708 / 41 / `RegOpenKey`, IFEO root | 2364 / 38 / `RegOpenKey`, IFEO root |

All eight children exited `0xc0000142` without the Rust-entry marker. There is
**no universal decisive ACCESS DENIED line**: the original-context
non-AppContainer has no access denial at all, yet fails identically. The last
denial for original LPAC is the real open of
`D:\a\basal\basal\spikes\win-confine`, asking **Execute/Traverse + Synchronize**,
directory/synchronous non-alert, share read/write, under impersonation. Its DACL
grants Administrators/SYSTEM full access and Users read/execute, but **no package
SID**. Full SDDL is in [latest object ACL samples](measurements/procmon/37852312584-windows-latest-denied-object-acls.json)
and [2022 samples](measurements/procmon/37852312584-windows-2022-denied-object-acls.json).
That missing package grant is consistent with Low LPAC denial, not proof that
this earlier cwd failure caused KernelBase attach failure.

The context LPAC loader **does open the install cwd successfully** in Procmon,
and a real parent-side open under its duplicate succeeds. The actual suspended
primary's list/traverse open still returns Win32 5. The Chrome-style context
loader and primary duplicates both return 5; its install DACL omits the logon
SID and the trace still denies traverse. These grants did not make every token
able to open cwd; a configured path is not an access witness.

The context LPAC's last denied registry object is
`HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options`,
requesting query-value/enumerate-subkeys. Its DACL has Users and ALL APPLICATION
PACKAGES read grants but no basal package grant. Nls CodePage and Session Manager
also deny LPAC reads earlier. Original non-AppContainer reads those keys and cwd
successfully, so none is established as the common attach cause.

One common final activity is visible: **every failing child successfully opens
`C:\Windows\System32\conhost.exe` for Execute/Traverse + Synchronize**, then records
`CreateFileMapping` / `FILE LOCKED WITH ONLY READERS`, followed by more registry
activity and exit. That result is not ACCESS DENIED and is not by itself proof
of a failed console attach. The suspended console parameter remains
`0xfffffffffffffffd` from `CREATE_NO_WINDOW`, unlike Chrome's 0. A detached-process
diagnostic isolates that console path while preserving the exact job,
child-process ban, mitigations and three stdio pipes.

### Trace-driven changes: console-free entry is measured

Run [37853389246](https://github.com/cortexkit/basal/actions/runs/37853389246),
compiled from `3889668cfd7d0e2fffa27ff20957d2bae0e5aa28`, retains the baseline
console recipes and tests two isolated interventions. A temporary package-SID
ACE grants exactly traverse/synchronize on the original cwd, without inheritance;
the original DACL is restored before the next recipe. Procmon confirms that
cwd open now succeeds, but the child still exits `0xc0000142` with no Rust entry.

**Replacing only `CREATE_NO_WINDOW` with `DETACHED_PROCESS` reaches Rust entry
on both images**, for both original-context token recipes and their alternate
context counterparts. All four detached children exit 0, revert (`ERROR_NO_TOKEN`
1008), close the adjustment handle, attest their actual primary as Untrusted,
and complete 2,049 probes on windows-latest / 1,921 on windows-2022. The job,
mitigations, child-process ban and explicit three-pipe inheritance list remain
unchanged. The detached startup console parameter is `0xffffffffffffffff`, not
Chrome's 0. The failing console
recipes' conhost image opens are not seen on that detached startup path.
This establishes console initialization as a sufficient removable cause of
pre-entry failure in these comparisons, **not an intrinsically unloadable
restricted token**. The exact internal console syscall/failure NTSTATUS was
not measured by Process Monitor.

The enabled-group LPAC detached recipes attempt post-load lockdown, but
`CreateRestrictedToken(post-load lockdown)` returns **Win32 87** on both images,
before `NtSetInformationProcess(ProcessAccessToken)` is called. The actual
Untrusted primary retains enabled groups and user/group restricting SIDs. The
Chrome-style detached primary is non-AppContainer, NULL-restricted, Untrusted
and privilege-free, but retains an enabled logon SID. Those are distinct
measured floors, not full LPAC acceptance.

Every detached recipe has exactly **one successful reachability probe**:
`CreateThread` / in-process thread. All tested file/directory/metadata, registry,
named-object and ALPC opens fail; epmapper connection fails; native Winsock
startup/socket paths fail; process creation, parent opens and executable-memory
allocation/protection fail. This is a finite target sample, not an all-IPC
proof. The retained birth reports include every exact probe result and each
ambient handle/type/access value. The unknown startup ALPC port and other
ambient objects must not be classified harmless from failed new opens.

The attempted same-window ETW adjunct did **not** run: `logman` rejected repeated
`-p` arguments with `0x80070057`, "Argument 'p' has been defined too many times."
No ETW data is claimed. Procmon's complete PID windows remain the authority;
setup output and metadata preserve that coverage gap. A provider-file form is
used for the subsequent GUI confirmation, rather than hiding this failed command.

### Final campaign: original lockdown and GUI subsystem

The last run is [37896729362](https://github.com/cortexkit/basal/actions/runs/37896729362),
compiled from `b9c67e3852e6be16ec096139c93e3fdbd9df1865`. **The original NULL-only,
all-deny-only LPAC primary reaches Rust entry**, both as a detached CUI image and
as a GUI-subsystem image **without `DETACHED_PROCESS`**. Both revert, lower the
actual primary from Low to Untrusted, close the adjustment handle before input,
complete all probes and exit 0 on both images. No post-load primary replacement
is needed. The earlier `0xc0000142` failures were not evidence that these tokens
are intrinsically unloadable; removing console initialization was sufficient.
The internal call or check responsible for console-dependent attach failure
remains unidentified; Procmon supplies no return status for it.

**Untrusted-at-birth non-AppContainer fails, even console-free.** Its suspended
primary is Untrusted, but the assigned Low loader is downgraded to Identification
(1); the detached and GUI attempts both exit `0xc00000a5` before entry on both
images. The successful Chrome-style recipe instead starts Low, then self-lowers
to Untrusted. No GUI non-AppContainer residual was produced; it must not be
inferred from the successful LPAC GUI report or Low-birth detached control.

Each completed diagnostic has **2,134 probes on windows-latest / 1,870 on
windows-2022**, and exactly **one successful new operation: `CreateThread`**.
All retain the exact owned job, full creation mitigations, child-process ban
and three explicit pipe handles. Latest / 2022 values follow:

| Final recipe | PID latest / 2022 | Entry and outcome | Ambient handles |
|---|---|---|---:|
| `full-detached-control`: NULL-only, all access groups deny-only, zero privileges, LPAC; Low birth to Untrusted | 5484 / 6940 | Complete report, exit 0 | 27 / 28 |
| `full-gui-control`: same full LPAC token/transition; GUI image, no detached flag | 7600 / 4836 | Complete report, exit 0 | 27 / 28 |
| `chrome-detached-control`: non-AppContainer NULL-only, deny-only except enabled logon SID; Low birth to Untrusted | 3648 / 1224 | Complete report, exit 0 | 51 / 53 |
| `chrome-context-detached-control`: same Chrome-style token plus alternate station/desktop, minimal environment and install cwd | 1032 / 7904 | Complete report, exit 0 | 50 / 52 |
| `post-load-detached-control`: enabled-group same-access LPAC, self-lowered Untrusted; replacement fails Win32 87 | 8376 / 2448 | Complete report, exit 0; **weaker primary** | 27 / 28 |
| `lpac-context-detached-control`: same weaker LPAC token with changed context | 9408 / 2980 | Complete report, exit 0; replacement fails Win32 87 | 28 / 29 |
| `chrome-untrusted-detached-control`: non-AppContainer, NULL/deny-only except logon, Untrusted at birth, Low loader | 7756 / 1664 | **No entry**, `0xc00000a5`; no residual | Not measured |
| `chrome-untrusted-gui-control`: GUI image, no detached flag, same Untrusted-at-birth token | 8656 / 7284 | **No entry**, `0xc00000a5`; no residual | Not measured |

The full LPAC actual final primary is type 1, Untrusted `S-1-16-0`, with exactly
`S-1-0-0` restricting SID, all thirteen access groups deny-only including logon,
zero privileges/capabilities, the basal package SID and `WIN://NOALLAPPPKG` UINT64
`[1]`. After revert `OpenThreadToken` returns 1008; no Token or unknown-type handle
survives. Birth primary/loader are Low and assigned loader is level 2; its handle
is non-inheritable, excluded from the list and closed before resume. Job flags
`0x2508`, UI `0xff`, process limit 1, 256 MiB memory and no breakaway are read back
through the exact owned job. Policies read back dynamic-code 1, signature 5,
image-load 7, strict-handle 3, extension-points 1, child-process 1, and Win32k
**5 on latest / 1 on 2022**, not normalized to a desired value.

The broker remains CUI. Independent PE inventories confirm CUI subsystem **3**
and GUI subsystem **2**; both have seven normal import descriptors, zero delay
imports and zero non-System32 imports. Explicit stdio works in the GUI image.
It additionally loads **System32 `apphelp.dll`** at startup, but no User32/GDI/
Win32u import is added. GUI creation flags are `0x08080004`, including
`CREATE_NO_WINDOW` but not detached; the CUI detached flags are `0x0008000c`.
The GUI suspended console field still reads `-3`, versus detached CUI's `-1`:
that field alone is not proof of a console dependency.

#### One side-by-side measured residual

This table uses the **same final run** and the same targets per image. `F-D` is
full NULL/deny-only LPAC detached; `F-G` is that primary in the GUI image; `C-D`
is successful non-AppContainer **Low-birth then Untrusted** detached; `E-D` is
enabled-group LPAC detached after failed replacement. The explicit-context
variants have identical probe-kind/result counts to C-D/E-D respectively but
different ambient inventories. Direct Untrusted-birth non-AppContainer columns
would be **not measured**, not an empty residual.

| Authority / operation | LPAC-only control | F-D | F-G | C-D | E-D |
|---|---|---|---|---|---|
| Completed probes / successful new operations (latest / 2022) | 2,134 / 1,870; **29 / 29 successes** | 2,134 / 1,870; **1 / 1** | 2,134 / 1,870; **1 / 1** | 2,134 / 1,870; **1 / 1** | 2,134 / 1,870; **1 / 1** |
| Files, directories and metadata | kernel32.dll/cmd.exe data-read `0x100081`; package/install directory list; Q/Z metadata `0x100080` on all four objects | All 24 opens Win32 5 | All 24 opens Win32 5 | All 24 opens Win32 5 | All 24 opens Win32 5 |
| File creates: TEMP / install / package | TEMP/install deny 5; **package CREATE_NEW succeeds**, `0x100082` | All three Win32 5 | All three Win32 5 | All three Win32 5 | All three Win32 5 |
| Registry reads and actual writes | Native package hive query and binary **write succeed**; three Win32 opens and guessed-HKCU write deny 5 | Four Win32 operations deny 5; native query/write `0xc0000022` | Same | Same | Same |
| Named-object directories / links / fixtures | Package directory/Global/Local/RPC Control/Session queries; event query/modify `1`/`2`; section query/read/write `1`/`4`/`2` succeed; global/session fixtures deny | Every enumerated directory/link/section/event/mutant/semaphore/timer/job open `0xc0000022` | Same | Same | Same |
| Named pipes | All 39 attempted opens Win32 5 on each image | All 39 Win32 5 | All 39 Win32 5 | All 39 Win32 5 | All 39 Win32 5 |
| New ALPC / epmapper | **epmapper succeeds**, grant `0x1f0001`; other 263 / 214 ports deny | All 264 / 215 connections `0xc0000022`, epmapper included | Same | Same | Same |
| Native Winsock TCP/UDP | WSAStartup 10107; sockets 10093; no connect/send | Same | Same | Same | Same |
| Executable allocation / RW-to-X | Both succeed | Both Win32 **1655** | Both Win32 **1655** | Both Win32 **1655** | Both Win32 **1655** |
| In-process thread | **CreateThread succeeds** | **Succeeds** | **Succeeds** | **Succeeds** | **Succeeds** |
| cmd.exe process creation / parent query-limited and VM_READ | All Win32 5 | All Win32 5 | All Win32 5 | All Win32 5 | All Win32 5 |
| Pre-input ambient handles (latest / 2022; includes pipes) | **34 / 35** | **27 / 28** | **27 / 28** | **51 / 53** | **27 / 28** |
| Existing startup ALPC / non-stdio files | One ALPC `0x1f0001`; three / four non-stdio files | One ALPC; **zero / one non-stdio file**, 2022 `0x100003` | Same | One ALPC; **one / two non-stdio files**, `0x100020` and on 2022 `0x100003`; also two Keys and 21 / 20 ETW registrations | Same as F-D |

Q/Z are separate read-attributes and desired-access-zero opens, both returning
metadata in the control. The full-mode parent observes zero TCP connections and
zero UDP datagrams across its diagnostic children. Direct native calls to the
Windows AFD network device, bypassing Winsock initialization, were not tested;
neither were real ALPC protocols or collaborating workers. Each of the six completed
diagnostic recipes has this entire **successful new-operation** residual:

| Kind / successful object | Requested and actual access | Present | Risk assessment (files / state / peer / network) |
|---|---|---|---|
| Thread / `CreateThread` | In-process thread; actual creation succeeds, Win32 0 | Both images, all six recipes | No direct files / process-local lifetime / no direct peer / no direct network; concurrent use of reachable authority remains possible |

**Pre-existing handles are authority too.** Full LPAC detached and GUI inventories
are identical: seven Event `0x1f0003`, two IoCompletion `0x1f0003`, two
TpWorkerFactory `0xf00ff`, four IRTimer `0x100002`, six / five
WaitCompletionPacket `0x1`, one Directory `0x3`, one ALPC Port `0x1f0001`,
one / zero SchedulerSharedData `0x1`, zero / two Semaphore `0x100003`,
zero / one **non-stdio File `0x100003`**, plus stdin `0x120189` and two
stdout/stderr `0x120196`. Counts sum to 27 / 28. The existing ALPC endpoint,
cached credentials and possible brokered authority remain unknown. The 2022
File has read/write rights and unknown identity. Failed new opens do not revoke
those handles; the three-stdio-only requirement is **not established**.

All final measurements are retained in [latest births](measurements/procmon/37896729362-windows-latest-births.json)
and [2022 births](measurements/procmon/37896729362-windows-2022-births.json).
[Per-recipe residual tables](measurements/procmon/37896729362-residuals.md) give
every successful probe, every probe-kind/result count and every ambient
type/access combination for each completed diagnostic separately.
[Detailed findings](measurements/procmon/37896729362-findings.md) include the
full attestation, risk tables, module differences and failed attempts;
[manifest.json](measurements/procmon/manifest.json) records source/artifact/retained
hashes. F-D and F-G use inherited startup parameters and follow earlier
diagnostics that modified the disposable install ACL; they are not proof of
fully scrubbed environment/cwd authority.

Both images passed Rust **1.99.0** fmt/fetch/check, **seven Rust tests**, Python
**3.12.10 / 22 tests**, static-CRT builds, independent import/subsystem checks and
the live-token mutation/restoration witness. The unchanged baseline acceptance
gates remain red because they select the ordinary failing console birth, not
the completed diagnostic. Diagnostic-only inspection of the unchanged validator
also finds exact-word policy mismatches: signature **5 versus expected 1** on
both images, and Win32k **5 versus 1** on latest. No gate was rewritten to accept
a missing child or hide those readbacks.

Process Monitor **4.11** captures all thirteen selected PID start/exit windows:
3,373 events / 375 non-success rows on latest and 3,119 / 373 on 2022. Version and
hashes match prior runs; readiness/termination/export return 0, while the capture
instance's separate exit 1 is retained. All non-success rows remain in the
[latest](measurements/procmon/37896729362-windows-latest-procmon-events.md) and
[2022](measurements/procmon/37896729362-windows-2022-procmon-events.md) tables.
The ETW adjunct again produces **no trace**: provider-file `logman create` returns
`0x80070490`, "Element not found," on both images. No provider or internal
console operation is blamed without data. No further native run was launched.

 No provider or internal
 console operation is blamed without data. No further native run was launched
 in that campaign.

## Named ambient handles before input

The identification runs are
[37979376928](https://github.com/cortexkit/basal/actions/runs/37979376928),
[37980061335](https://github.com/cortexkit/basal/actions/runs/37980061335), and
[37980492496](https://github.com/cortexkit/basal/actions/runs/37980492496), compiled
from `7d7ea8c8a5de233a61abfb382d6faf97a6c2c108`,
`5798847db119dc83d3febc162f55fd795489de0e`, and
`1e6135b5ad1ef0001746fe7438f91ad9885394aa`. Each names a handle by duplicating it
and calling `NtQueryObject` `ObjectNameInformation`. Files also get
`NtQueryInformationFile` `FileNameInformation`. The ALPC port also gets
`NtAlpcQueryInformation` classes 0 through 4. No ALPC message is sent. The
unchanged `full-gui-control` still completes with one successful new operation,
`CreateThread`, and the same 27 / 28 handle totals. Its module list still includes
System32 `apphelp.dll`.

The close recipes use the same GUI full-LPAC token, job, mitigations and three
pipes. They write the named inventory before probes. Their later module list omits
`apphelp.dll` even in the recipe that closes no handle, so that omission is an
inventory-call difference, not evidence that a particular close unloaded apphelp.

| Handle | Latest / 2022 | Name and query | Creator evidence | Close result |
|---|---:|---|---|---|
| Directory `0x3` | 1 / 1 | `\KnownDlls`; object name succeeds | ntdll loader cache. Procmon shows no userspace open of this directory during the measured window, so the handle is already open when the snapshot starts. | Close succeeds, then the process dies `0xc0000008` (`STATUS_INVALID_HANDLE`) after input and before probe results. **Cannot be closed.** |
| ALPC Port `0x1f0001` | 1 / 1 | Object name empty. Class 0 returns 16 bytes, flags `0x30000`, sequence 1. Classes 1, 2 and 4 return `0xc000000d`; class 3 returns `0xc0000078`. No server name or SID was returned. | Not established. It is not a named `\RPC Control` port: a new epmapper connection is separately denied. It is also not identified as the apphelp port. | Close succeeds on both images. The handle is absent from the second snapshot. The worker then completes every probe and exits 0, with the same one success, `CreateThread`. **Close it before input.** |
| File `0x100003` | 0 / 1 | `\Device\KsecDD`. Object name succeeds. `FileNameInformation` returns `0xc0000003` because this device handle does not support that class. | Kernel security device, present only on Server 2022. Procmon records no userspace `CreateFile` for it in the GUI startup window. | Close succeeds. The handle is absent afterward. Probes complete and the process exits 0. **Close it before input on 2022.** |
| File `0x120189`, two File `0x120196` | 3 / 3 | Anonymous pipes. Object name returns `0xc0000039`; file name is empty. Handle attribute `0x2` marks them inheritable, unlike every other handle. | The three handles in `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. | Not closed. These are the intended stdio pipes. |
| Event `0x1f0003` | 7 / 7 | Empty object name | Unnamed. Counts and access match ntdll thread-pool/wait objects, but no creation stack attributes each handle to ntdll, kernelbase, the CRT or Rust. | Not closed individually. |
| IoCompletion `0x1f0003` | 2 / 2 | Empty object name | Same unnamed thread-pool attribution limit. | Same. |
| TpWorkerFactory `0xf00ff` | 2 / 2 | Empty object name | The type itself is the thread-pool worker factory. | Same. |
| IRTimer `0x100002` | 4 / 4 | Empty object name | Same unnamed thread-pool attribution limit. | Same. |
| WaitCompletionPacket `0x1` | 6 / 5 | Empty object name | Same. The one-count image difference is measured, not explained by a creation stack. | Same. |
| Semaphore `0x100003` | 0 / 2 | Empty object name | Server 2022 only. Same attribution limit. | Same. |
| SchedulerSharedData `0x1` | 1 / 0 | Empty object name | Windows 11 only. No userspace open was observed. Its owner is not established beyond the kernel object type. | Not closed. Closing it was not isolated from the fatal thread-pool close. |

Closing the unnamed Event, IoCompletion, TpWorkerFactory, IRTimer,
WaitCompletionPacket and Semaphore handles together dies at `0xc0000008` during
Rust entry, before the inventory write, on both images. That recipe therefore
proves the set is load-owned and unsafe to remove wholesale. It does not prove
that every member is required, because the fatal member was not isolated. A
creation stack was not available: the kernel-object ETW session still returns
`0x80070490` and Process Monitor does not record these object-manager creates.

The thread-pool handles grant process-local wait, timer, completion and worker
control on unnamed objects. They do not name a file, a network endpoint, another
process, or a durable object. That is not a proof that no operation on them can
reach another process; it is the measured identity and access. `CreateThread`
remains the one successful new operation, so a thread-pool handle is not the only
way the process can run concurrent work.

`\KnownDlls` at access `0x3` grants directory query and traverse for the loader's
known-DLL cache. It is not a writable namespace and not the package namespace.
It still must stay: the worker cannot complete input after it is closed. The ALPC
port remains the unresolved IPC object. Its server name and protocol were not
returned by the five published port queries, so its cached context is unknown.
The successful close removes that unknown channel before input; it does not
identify which service had accepted the connection.

After the two successful closes, the pre-input table is:

| Image | Closed | Remaining besides three pipes | Must remain | Unnamed thread-pool residual |
|---|---|---:|---|---|
| windows-latest | ALPC port | 23 | `\KnownDlls`, `SchedulerSharedData` | 7 Event, 2 IoCompletion, 2 TpWorkerFactory, 4 IRTimer, 6 WaitCompletionPacket |
| windows-2022 | ALPC port and `\Device\KsecDD` | 23 | `\KnownDlls` | 7 Event, 2 IoCompletion, 2 TpWorkerFactory, 4 IRTimer, 5 WaitCompletionPacket, 2 Semaphore |

That is the bounded residual. It is not a three-pipe table. The two closes were
measured in separate processes, not together in one process. Production acceptance
still fails closed until a gate explicitly selects the GUI recipe, checks these
names, checks that the ALPC and KsecDD handles are gone, and refuses a `\KnownDlls`
handle that is missing or writable.

## Recommended Windows design changes

1. **Use a GUI-subsystem worker with the original NULL-only LPAC primary as the
   recommended next implementation path.** On both images this image reaches
   entry without `DETACHED_PROCESS`, with every access group deny-only (logon
   included), zero privileges/capabilities, the required job/mitigations, and an
   actual Untrusted primary before input. The detached CUI variant is a measured
   fallback/control, not a reason to keep a console dependency in the image.
    System32 apphelp is observed only in the unchanged GUI startup module list.
    Its shim database opens are closed before the handle snapshot; the startup
    ALPC port was not identified as an apphelp port. The GUI image does not reduce
   handle totals relative to detached CUI; prefer it for its demonstrated
   console-free
   image contract, not for an unmeasured residual improvement. Keep explicit
   stdio pipes and independent PE subsystem/import checks.
2. **Keep production Windows fail-closed, and keep the measured residual explicit.**
   The runnable GUI recipe still holds more than three pipes before input. The
   [named inventory](#named-ambient-handles-before-input) identifies every one.
   Close the unnamed ALPC port and, on Server 2022, `\Device\KsecDD` after load:
   both closes succeed and the worker still completes its probes. Do not close
   `\KnownDlls`; that close is fatal under the strict-handle policy. Keep the
   unnamed thread-pool objects as a named residual: closing them together is also
   fatal, and no creation stack separates a removable subset. Absence of Token
   handles is not the boundary.
3. **Construct the final restrictions before creation; lower only integrity after
   load.** The successful full recipes start with NULL-only restricting SID,
   deny-only groups and no privileges, then revert and lower the actual Low
   primary to Untrusted. No wholesale primary replacement is needed. Do not use
   the enabled-group LPAC fallback whose post-load CreateRestrictedToken fails
   Win32 87, and do not infer a privileged service is required from that error:
   the successful original-primary sequence did not take that route.
4. **Attest birth and input phases separately.** Use the measured matching Low
   LPAC initial token, assigned at SecurityImpersonation (2), and query the actual
   suspended primary/thread. Preserve the non-inheritable initial handle,
   explicit exclusion from the three-handle list and successful close before
   resume. After entry, require revert/ERROR_NO_TOKEN, actual Untrusted primary,
   closed adjustment handle and complete token/handle/policy attestation before
   input. Supplying an Untrusted primary or requesting level 2 is not readback.
   Non-AppContainer Untrusted-at-birth still downgraded its Low loader to level 1
   and failed; the Low-to-Untrusted Chrome-style control is a different recipe.
5. **Keep LPAC and non-AppContainer comparisons distinct.** Full LPAC and the
   successful non-AppContainer Low-to-Untrusted floor have the same one-success
   new-operation sample, but the latter retains logon and 51 / 53 ambient
   handles, including Keys and ETW registrations. It is not the specified token
   or a GUI success reference. LPAC's class-46 getter remains unsupported;
   require the native WIN://NOALLAPPPKG UINT64 `[1]` claim, exact package SID and
   empty capabilities, while retaining query errors.
6. **Reject LPAC-only as a substitute for lockdown.** Its repeated 29 successes
   include package file creation, actual registry writes, shared event/section
   rights and epmapper connection. Profile reuse is a state/peer channel.
   Per-worker profile isolation and cleanup remain necessary hygiene, not an
   independently proven no-state boundary. Keep these positive controls so
   denial results cannot be mistaken for missing targets or constant-denial code.
7. **Make console-free startup explicit and scrub the rest of the context.**
   CREATE_NO_WINDOW on a CUI image did not prevent the measured console path;
   DETACHED_PROCESS or a GUI-subsystem image fixes entry without relaxing policy.
   Do not add broad system-object grants to cure that failure. Alternate
   station/desktop, minimal environment and install cwd alone did not fix it.
   The recommended successful GUI recipe inherited runner parameters and used
   the disposable install directory modified by earlier diagnostics; separately
   minimize environment/cwd/station authority and measure the resulting process.
8. **Preserve object-specific imports and policy evidence.** Keep Userenv/Ole32/
    User32 broker-only, record the actual startup module list, and
    inventory modules after probes as well. The close recipes omit apphelp from
    the post-probe list even when they close no handle, so do not treat that
    omission as a successful apphelp removal. Retain signature 5 versus the
   validator's expected 1 and latest's Win32k 5 versus 1 rather than claiming
   exact-word equivalence. Existing baseline acceptance selects the
   failing console recipe; a future gate must explicitly select and verify the
   intended GUI recipe, never silently replace a missing baseline child.
9. **Bound all threads and resources for the whole process.** CreateThread
   succeeds in every completed recipe, including full LPAC; executable-memory
   operations fail with 1655 under the measured dynamic-code policy. Use wall
   deadlines and pool retirement across all threads, and exact owned-job
   membership/readback with no breakaway. Do not call thread creation denied or
   infer runtime isolation from a job's existence alone.
10. **Treat the residual as finite evidence, not universal non-reachability.**
    All tested new file/registry/named-object/ALPC opens deny under full LPAC, and
    native Winsock never reaches connect/send, but bare NT AFD, real ALPC
    protocols, collaborating workers and ambient-handle operations remain
    unmeasured. The failed ETW adjunct supplies no Kernel-Object trace. Keep
    ordered Procmon non-success rows, real access witnesses and separate
    capture errors; the exact internal console failure and Chrome desktop ACL
    remain unknown. No unmeasured grant, protocol denial or privileged-broker
    requirement should become a production assumption.
