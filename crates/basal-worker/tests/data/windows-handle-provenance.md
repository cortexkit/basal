# Production Windows handle provenance

Measured by CI run [38075560568](https://github.com/cortexkit/basal/actions/runs/38075560568).

Both images used the production release `ck-basal-worker.exe` (no worker
deviation feature). The tool copied its bytes unchanged to a `ckdev-` name,
launched the full confinement, enabled ProcessHandleTracing class 32 with
16,384 slots before resume, and queried the process at its pre-input
checkpoint after all startup checks. User-space symbols below are PDB-resolved,
not nearest-export guesses. Kernel prefixes are retained verbatim and labelled
kernel (unsymbolized). The three pipes were created by the parent and were
the only entries in its explicit inherited-handle list.

This is a one-off measurement, not the handle allowlist. Live startup checks
continue to use the fixed per-image ceiling constants. An image change
requires remeasurement. Creator chains identify initialization authority;
they are not an exhaustive proof of every operation on the residual objects.

## windows-latest

Runner image: `win25-vs2026`; version: `20260925.250.1`.
[Unedited raw report](windows-handle-provenance-windows-latest.json).
Production image SHA-256: `94efcb0cd139c921dd63e12ed5a3fe3a048f149a5a49f3b71e5999615d52620e`.
Accepted ceiling profile: `windows-latest`. Total: **25**.

| Type | Granted access | Count | Creator chains |
|---|---|---:|---|
| Directory | `0x3` | 1 | C1 |
| Event | `0x1f0003` | 6 | C2 |
| File | `0x120189` | 1 | C3 |
| File | `0x120196` | 2 | C4 |
| IRTimer | `0x100002` | 4 | C5 |
| IoCompletion | `0x1f0003` | 2 | C6 |
| SchedulerSharedData | `0x1` | 1 | C7 |
| TpWorkerFactory | `0xf00ff` | 2 | C8 |
| WaitCompletionPacket | `0x1` | 6 | C9 |

### C1: Directory `0x3`

- ×1: `kernel (unsymbolized) @0xfffff804c4fd0359 → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtOpenDirectoryObject+0x14 → ntdll!LdrpInitializeProcess+0xd4d → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`

### C2: Event `0x1f0003`

- ×1: `kernel (unsymbolized) @0xfffff804c4e4bb2b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateEvent+0x14 → ntdll!EtwpRegisterTpNotificationOnce+0x37 → ntdll!EtwpRegisterProvider+0xb4 → ntdll!EtwNotificationRegister+0x161 → ntdll!EtwEventRegister+0x23 → ntdll!TraceLoggingRegisterEx_EtwEventRegister_EtwEventSetInformation+0x5a → ntdll!LdrpInitializeProcess+0xb7d → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e4bb2b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpCreateLoaderEvents+0x23 → ntdll!LdrpInitializeProcess+0x127b → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e4bb2b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpCreateLoaderEvents+0x46 → ntdll!LdrpInitializeProcess+0x127b → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e4bb2b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpInitialize+0xea → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e4bb2b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpInitializeInternal+0x95 → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e4bb2b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateEvent+0x14 → ntdll!RtlpWnfRegisterTpNotification+0x2d → ntdll!RtlpInitializeWnf+0x9d → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`

### C3: File `0x120189`

- ×1: `parent CreatePipe / explicit HANDLE_LIST`

### C4: File `0x120196`

- ×2: `parent CreatePipe / explicit HANDLE_LIST`

### C5: IRTimer `0x100002`

- ×1: `kernel (unsymbolized) @0xfffff804c500b047 → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x52 → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x393 → ntdll!LdrpEnableParallelLoading+0x49 → ntdll!LdrpInitializeProcess+0x19db → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c500b047 → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x52 → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x393 → ntdll!TppPoolpReferenceGlobalPool+0xc7 → ntdll!TppCleanupGroupMemberInitialize+0x15f → ntdll!TppWorkInitialize+0x31 → ntdll!TpAllocTimer+0xc2 → ntdll!RtlpInitializeWnf+0x59 → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a`
- ×1: `kernel (unsymbolized) @0xfffff804c500b047 → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x52 → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x393 → ntdll!LdrpEnableParallelLoading+0x49 → ntdll!LdrpInitializeProcess+0x19db → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c500b047 → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x52 → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x393 → ntdll!TppPoolpReferenceGlobalPool+0xc7 → ntdll!TppCleanupGroupMemberInitialize+0x15f → ntdll!TppWorkInitialize+0x31 → ntdll!TpAllocTimer+0xc2 → ntdll!RtlpInitializeWnf+0x59 → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a`

### C6: IoCompletion `0x1f0003`

- ×1: `kernel (unsymbolized) @0xfffff804c500c28b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateIoCompletion+0x14 → ntdll!TpAllocPoolInternal+0x297 → ntdll!LdrpEnableParallelLoading+0x49 → ntdll!LdrpInitializeProcess+0x19db → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c500c28b → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateIoCompletion+0x14 → ntdll!TpAllocPoolInternal+0x297 → ntdll!TppPoolpReferenceGlobalPool+0xc7 → ntdll!TppCleanupGroupMemberInitialize+0x15f → ntdll!TppWorkInitialize+0x31 → ntdll!TpAllocTimer+0xc2 → ntdll!RtlpInitializeWnf+0x59 → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`

### C7: SchedulerSharedData `0x1`

- ×1: `kernel (unsymbolized) @0xfffff804c4f4f7c7 → kernel (unsymbolized) @0xfffff804c4f50c0a → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtSetInformationProcess+0x14 → ntdll!LdrpAllocateSchedulerSharedData+0x31 → ntdll!LdrpInitializeProcess+0x17c7 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`

### C8: TpWorkerFactory `0xf00ff`

- ×1: `kernel (unsymbolized) @0xfffff804c4fa26fd → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWorkerFactory+0x14 → ntdll!TpAllocPoolInternal+0x308 → ntdll!LdrpEnableParallelLoading+0x49 → ntdll!LdrpInitializeProcess+0x19db → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4fa26fd → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWorkerFactory+0x14 → ntdll!TpAllocPoolInternal+0x308 → ntdll!TppPoolpReferenceGlobalPool+0xc7 → ntdll!TppCleanupGroupMemberInitialize+0x15f → ntdll!TppWorkInitialize+0x31 → ntdll!TpAllocTimer+0xc2 → ntdll!RtlpInitializeWnf+0x59 → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`

### C9: WaitCompletionPacket `0x1`

- ×1: `kernel (unsymbolized) @0xfffff804c4e3361e → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TpAllocWait+0xbf → ntdll!EtwpRegisterTpNotificationOnce+0x52 → ntdll!EtwpRegisterProvider+0xb4 → ntdll!EtwNotificationRegister+0x161 → ntdll!EtwEventRegister+0x23 → ntdll!TraceLoggingRegisterEx_EtwEventRegister_EtwEventSetInformation+0x5a → ntdll!LdrpInitializeProcess+0xb7d → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e3361e → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TpAllocWait+0xbf → ntdll!RtlpWnfRegisterTpNotification+0x4c → ntdll!RtlpInitializeWnf+0x9d → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e3361e → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x6a → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x393 → ntdll!LdrpEnableParallelLoading+0x49 → ntdll!LdrpInitializeProcess+0x19db → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e3361e → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x6a → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x393 → ntdll!TppPoolpReferenceGlobalPool+0xc7 → ntdll!TppCleanupGroupMemberInitialize+0x15f → ntdll!TppWorkInitialize+0x31 → ntdll!TpAllocTimer+0xc2 → ntdll!RtlpInitializeWnf+0x59 → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a`
- ×1: `kernel (unsymbolized) @0xfffff804c4e3361e → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x6a → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x393 → ntdll!LdrpEnableParallelLoading+0x49 → ntdll!LdrpInitializeProcess+0x19db → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff804c4e3361e → kernel (unsymbolized) @0xfffff804c4cb5c55 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x6a → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x393 → ntdll!TppPoolpReferenceGlobalPool+0xc7 → ntdll!TppCleanupGroupMemberInitialize+0x15f → ntdll!TppWorkInitialize+0x31 → ntdll!TpAllocTimer+0xc2 → ntdll!RtlpInitializeWnf+0x59 → ntdll!RtlpSubscribeWnfStateChangeNotificationInternal+0x18e → ntdll!LdrpEnableUMGLTracingStateSync+0x7f → ntdll!LdrpInitializeProcess+0xb71 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0x5a`

## windows-2022

Runner image: `win22`; version: `20261004.326.1`.
[Unedited raw report](windows-handle-provenance-windows-2022.json).
Production image SHA-256: `82661cd814af5f43db545c0a21cd98b4a7b9509058ff6459ca87611ba00d4612`.
Accepted ceiling profile: `windows-2022`. Total: **25**.

| Type | Granted access | Count | Creator chains |
|---|---|---:|---|
| Directory | `0x3` | 1 | C1 |
| Event | `0x1f0003` | 6 | C2 |
| File | `0x120189` | 1 | C3 |
| File | `0x120196` | 2 | C4 |
| IRTimer | `0x100002` | 4 | C5 |
| IoCompletion | `0x1f0003` | 2 | C6 |
| Semaphore | `0x100003` | 2 | C7 |
| TpWorkerFactory | `0xf00ff` | 2 | C8 |
| WaitCompletionPacket | `0x1` | 5 | C9 |

### C1: Directory `0x3`

- ×1: `kernel (unsymbolized) @0xfffff8008115fca2 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtOpenDirectoryObject+0x14 → ntdll!LdrpInitializeProcess+0xc66 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`

### C2: Event `0x1f0003`

- ×1: `kernel (unsymbolized) @0xfffff80081109db1 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateEvent+0x14 → kernelbase!CreateEventW+0x92 → bcrypt!BcpRegisterConfigChangeNotifyNoLogging+0x58 → bcrypt!InitializeSystemPreferredCache+0xe9 → bcrypt!InitializeCNG+0x7a → bcrypt!DllMain+0x87 → ntdll!LdrpCallInitRoutine+0x6b → ntdll!LdrpInitializeNode+0x1ca → ntdll!LdrpInitializeGraphRecurse+0x42 → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeProcess+0x1caa → ntdll!LdrpInitialize+0x16c`
- ×1: `kernel (unsymbolized) @0xfffff80081109db1 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateEvent+0x14 → ntdll!EtwpRegisterTpNotificationOnce+0x2f → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20 → ntdll!LdrpInitializeProcess+0xa5d → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff80081109db1 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpCreateLoaderEvents+0x23 → ntdll!LdrpInitializeProcess+0x11ec → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff80081109db1 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpCreateLoaderEvents+0x46 → ntdll!LdrpInitializeProcess+0x11ec → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff80081109db1 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpInitialize+0x7a → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff80081109db1 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateEvent+0x14 → ntdll!LdrpInitializeInternal+0x59 → ntdll!LdrInitializeThunk+0xe`

### C3: File `0x120189`

- ×1: `parent CreatePipe / explicit HANDLE_LIST`

### C4: File `0x120196`

- ×2: `parent CreatePipe / explicit HANDLE_LIST`

### C5: IRTimer `0x100002`

- ×1: `kernel (unsymbolized) @0xfffff8008110232d → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x4e → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x36e → ntdll!LdrpEnableParallelLoading+0x39 → ntdll!LdrpInitializeProcess+0x1b06 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff8008110232d → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x4e → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x36e → ntdll!TppPoolpReferenceGlobalPool+0xa8 → ntdll!TppCleanupGroupMemberInitialize+0x218 → ntdll!TppWorkInitialize+0x21 → ntdll!TppInitializeTimer+0x43 → ntdll!TpAllocWait+0xec → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20`
- ×1: `kernel (unsymbolized) @0xfffff8008110232d → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x4e → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x36e → ntdll!LdrpEnableParallelLoading+0x39 → ntdll!LdrpInitializeProcess+0x1b06 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff8008110232d → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateTimer2+0x14 → ntdll!TppInitializeTimerSubQueue+0x4e → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x36e → ntdll!TppPoolpReferenceGlobalPool+0xa8 → ntdll!TppCleanupGroupMemberInitialize+0x218 → ntdll!TppWorkInitialize+0x21 → ntdll!TppInitializeTimer+0x43 → ntdll!TpAllocWait+0xec → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20`

### C6: IoCompletion `0x1f0003`

- ×1: `kernel (unsymbolized) @0xfffff80081174055 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateIoCompletion+0x14 → ntdll!TpAllocPoolInternal+0x275 → ntdll!LdrpEnableParallelLoading+0x39 → ntdll!LdrpInitializeProcess+0x1b06 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff80081174055 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateIoCompletion+0x14 → ntdll!TpAllocPoolInternal+0x275 → ntdll!TppPoolpReferenceGlobalPool+0xa8 → ntdll!TppCleanupGroupMemberInitialize+0x218 → ntdll!TppWorkInitialize+0x21 → ntdll!TppInitializeTimer+0x43 → ntdll!TpAllocWait+0xec → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20 → ntdll!LdrpInitializeProcess+0xa5d → ntdll!LdrpInitialize+0x16c`

### C7: Semaphore `0x100003`

- ×1: `kernel (unsymbolized) @0xfffff800810a7875 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateSemaphore+0x14 → ntdll!RtlInitializeResource+0xbe → bcrypt!InitializeSystemPreferredCache+0x3f → bcrypt!InitializeCNG+0x7a → bcrypt!DllMain+0x87 → ntdll!LdrpCallInitRoutine+0x6b → ntdll!LdrpInitializeNode+0x1ca → ntdll!LdrpInitializeGraphRecurse+0x42 → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeProcess+0x1caa → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed`
- ×1: `kernel (unsymbolized) @0xfffff800810a7875 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateSemaphore+0x14 → ntdll!RtlInitializeResource+0xed → bcrypt!InitializeSystemPreferredCache+0x3f → bcrypt!InitializeCNG+0x7a → bcrypt!DllMain+0x87 → ntdll!LdrpCallInitRoutine+0x6b → ntdll!LdrpInitializeNode+0x1ca → ntdll!LdrpInitializeGraphRecurse+0x42 → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeGraphRecurse+0xac → ntdll!LdrpInitializeProcess+0x1caa → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed`

### C8: TpWorkerFactory `0xf00ff`

- ×1: `kernel (unsymbolized) @0xfffff8008110216c → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWorkerFactory+0x14 → ntdll!TpAllocPoolInternal+0x2e6 → ntdll!LdrpEnableParallelLoading+0x39 → ntdll!LdrpInitializeProcess+0x1b06 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff8008110216c → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWorkerFactory+0x14 → ntdll!TpAllocPoolInternal+0x2e6 → ntdll!TppPoolpReferenceGlobalPool+0xa8 → ntdll!TppCleanupGroupMemberInitialize+0x218 → ntdll!TppWorkInitialize+0x21 → ntdll!TppInitializeTimer+0x43 → ntdll!TpAllocWait+0xec → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20 → ntdll!LdrpInitializeProcess+0xa5d → ntdll!LdrpInitialize+0x16c`

### C9: WaitCompletionPacket `0x1`

- ×1: `kernel (unsymbolized) @0xfffff800811664d8 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TpAllocWait+0xba → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20 → ntdll!LdrpInitializeProcess+0xa5d → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff800811664d8 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x66 → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x36e → ntdll!LdrpEnableParallelLoading+0x39 → ntdll!LdrpInitializeProcess+0x1b06 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff800811664d8 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x66 → ntdll!TppInitializeTimerQueue+0x31 → ntdll!TpAllocPoolInternal+0x36e → ntdll!TppPoolpReferenceGlobalPool+0xa8 → ntdll!TppCleanupGroupMemberInitialize+0x218 → ntdll!TppWorkInitialize+0x21 → ntdll!TppInitializeTimer+0x43 → ntdll!TpAllocWait+0xec → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20`
- ×1: `kernel (unsymbolized) @0xfffff800811664d8 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x66 → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x36e → ntdll!LdrpEnableParallelLoading+0x39 → ntdll!LdrpInitializeProcess+0x1b06 → ntdll!LdrpInitialize+0x16c → ntdll!LdrpInitializeInternal+0xed → ntdll!LdrInitializeThunk+0xe`
- ×1: `kernel (unsymbolized) @0xfffff800811664d8 → kernel (unsymbolized) @0xfffff80080e451e5 → ntdll!NtCreateWaitCompletionPacket+0x14 → ntdll!TppInitializeTimerSubQueue+0x66 → ntdll!TppInitializeTimerQueue+0x49 → ntdll!TpAllocPoolInternal+0x36e → ntdll!TppPoolpReferenceGlobalPool+0xa8 → ntdll!TppCleanupGroupMemberInitialize+0x218 → ntdll!TppWorkInitialize+0x21 → ntdll!TppInitializeTimer+0x43 → ntdll!TpAllocWait+0xec → ntdll!EtwpRegisterTpNotificationOnce+0x4e → ntdll!RtlRunOnceExecuteOnce+0x9a → ntdll!EtwpRegisterProvider+0x78 → ntdll!EtwNotificationRegister+0xd7 → ntdll!EtwEventRegister+0x20`
