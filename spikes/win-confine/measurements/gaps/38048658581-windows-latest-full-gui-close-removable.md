# Windows gap measurements: run 38048658581 / windows-latest

Source `6738c09d0117a665dfcb547319a305956d0bafd1`; recipe `full-gui-close-removable`; PID 752; exit `0xc0000008`.

## Gap 1: all live threads

The broker enumerates with SystemProcessInformation and Toolhelp at worker-requested barriers. The confined worker's enumeration errors are retained separately. Callbacks remain alive during the second snapshot; these are not permanently suspended threads.

### pre_pool_activity
Complete: True; SPI NumberOfThreads: 1; Toolhelp TIDs: [516]

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|
| 516 | `0x00007ff6b3bb5cf0` | `!?+? (type None; error 487)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |

### post_pool_activity
Complete: None; SPI NumberOfThreads: None; Toolhelp TIDs: None

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|

### post_release
Complete: None; SPI NumberOfThreads: None; Toolhelp TIDs: None

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|

Callback barrier status: `None`; callback TIDs included: `None`.

| Callback | TID | self=true | self=false |
|---|---:|---|---|

## Gap 2: SchedulerSharedData

Correlation snapshot: `null`

| Operation | Exact measured result (NTSTATUS unless explicitly Win32) |
|---|---|
| inventory | `{}` |

Loader syscall observations: `[{"api":"NtSetInformationProcess","buffer_bytes":8,"caller":140731761800849,"name":{"buffer_bytes":8,"handle":0},"phase":"entry","tid":876},{"NTSTATUS":"0x00000000","api":"NtSetInformationProcess","handle":{"buffer_bytes":8,"handle":64},"name":{"buffer_bytes":8,"handle":0},"phase":"return","tid":876}]`

Scheduler-close trial: `{"pid":5816,"exit_code":"0xc0000005","stderr":"probe-stage: Rust entry\nEXCEPTION: code=0x000006a6 address=0x00007ffea7c975fa\nEXCEPTION: code=0x000006a6 address=0x00007ffea7c975fa\nambient-close: handle=64 type=Some(\"SchedulerSharedData\") success=true\nprobe-stage: attestation complete\nprobe-stage: force work/wait/timer after closes (debugger observes faults)\npool-stage: SubmitThreadpoolWork\npool-stage: SetThreadpoolWait\npool-stage: SetThreadpoolTimer\npool-stage: QueueUserWorkItem\nprobe-stage: forced pool activity returned {\"callback_inspections\":[{\"kind\":\ … (truncated; full report in the run artifact)

Coverage violations: ["complete-close worker did not exit successfully", "post_pool_activity: incomplete enumeration", "post_release: incomplete enumeration", "callback liveness at snapshot not proved", "work/wait/timer/legacy callbacks did not all run", "callback drain not proved", "callback TID missing from enumeration", "scheduler inventory unknown"]

This validates coverage and token absence, not universal scheduler non-reachability. A successful duplicate is success; its grant and follow-up security operations must be assessed, not labelled denied.
