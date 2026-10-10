# Windows gap measurements: run 38048658581 / windows-2022

Source `6738c09d0117a665dfcb547319a305956d0bafd1`; recipe `full-gui-close-removable`; PID 4784; exit `0xc0000008`.

## Gap 1: all live threads

The broker enumerates with SystemProcessInformation and Toolhelp at worker-requested barriers. The confined worker's enumeration errors are retained separately. Callbacks remain alive during the second snapshot; these are not permanently suspended threads.

### pre_pool_activity
Complete: True; SPI NumberOfThreads: 1; Toolhelp TIDs: [2100]

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|
| 2100 | `0x00007ff770895cd8` | `!?+? (type None; error 487)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |

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

Loader syscall observations: `[]`

Scheduler-close trial: `{"pid":772,"exit_code":"0x00000000","stderr":"probe-stage: Rust entry\nEXCEPTION: code=0x000006a6 address=0x00007fff192fefdc\nEXCEPTION: code=0x000006a6 address=0x00007fff192fefdc\nprobe-stage: attestation complete\nprobe-stage: force work/wait/timer after closes (debugger observes faults)\npool-stage: SubmitThreadpoolWork\npool-stage: SetThreadpoolWait\npool-stage: SetThreadpoolTimer\npool-stage: QueueUserWorkItem\nprobe-stage: forced pool activity returned {\"callback_inspections\":[{\"kind\":\"wait\",\"nt_open_as_client\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"nt_open_as_self\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"open_as_client\":{\"error\":1008,\"has_token\":false},\"open_as_self\":{\"error\":1008,\"has_token\":false},\"scheduler_slot_query_57_bytes8\":{\"returned_bytes\":0,\"slot\":\"0x0000000000000000\",\"status\":\"0xc0000003\"},\"tid\":1884},{\"kind\":\"work\",\"nt_open_as_client\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"nt_open_as_self\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"open_as_client\":{\"error\":1008,\"has_token\":false},\"open_as_self\":{\"error\":1008,\"has_token\":false},\"scheduler_slot_query_57_bytes8\":{\"returned_bytes\":0,\"slot\":\"0x0000000000000000\",\"status\":\"0xc0000003\"},\"tid\":3112},{\"kind\":\"legacy_work\",\"nt_open_as_client\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"nt_open_as_self\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"open_as_client\":{\"error\":1008,\"has_token\":false},\"open_as_self\":{\"error\":1008,\"has_token\":false},\"scheduler_slot_query_57_bytes8\":{\"returned_bytes\":0,\"slot\":\"0x0000000000000000\",\"status\":\"0xc0000003\"},\"tid\":7644},{\"kind\":\"timer\",\"nt_open_as_client\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"nt_open_as_self\":{\"has_token\":false,\"status\":\"0xc000007c\"},\"open_as_client\":{\"error\":1008,\"has_token\":false},\"open_as_self\":{\"error\":1008,\"has_token\":false},\"scheduler_slot_query_57_bytes8\":{\"returned_bytes\":0,\"slot\":\"0x0000000000000000\",\"status\":\"0xc0000003\"},\"tid\":6680}],\"callbacks_drained\":true,\"callbacks_held_during_snapshot\":true,\"legacy_error\":0,\"legacy_queued\":true,\"local\":{\"all_threads_have_no_impersonation_token\":false,\"any_token_found\":false,\"discovered_toolhelp_tids\":[],\"enumeration_complete\":false,\"nt_enumeration_error\":\"PID 772 missing from SystemProcessInformation\",\"process_reported_thread_count\":null,\"thread_count\":0,\"threads\":[],\"toolhelp_error\":null},\"observer\":null,\"ready_status\":0}\nprobe-stage: preliminary inventory written\nprobe-stage: input complete; leak_only=false\nprobe-progress: 0/1857 (file: C:\\Windows\\System32\\kernel32.dll)\nprobe-progress: 1/1857 (file: C:\\Windows\\System32\\cmd.exe)\nprobe-progress: 2/1857 (directory: C:\\Users\\runneradmin)\nprobe-progress: 3/1857 (directory: C:\\Users\\RUNNER~1\\AppData\\Local\\Temp)\nprobe-progress: 4/1857 (directory: C:\\Users\\runneradmin\\AppData\\Local\\Packages\\basal.spike.worker\\AC)\nprobe-progress: 500/1857 (alpc: \\RPC Control\\lsasspirpc)\nprobe-progress: 1000/1857 (semaphore: \\BaseNamedObjects\\SM0:2932:120:WilError_03_p0h)\nprobe-progress: 1500/1857 (section_read: \\Sessions\\2\\BaseNamedObjects\\278HWNDInterface:2011a)\n","faults":[],"measurement_faults":[]}`

Coverage violations: ["complete-close worker did not exit successfully", "post_pool_activity: incomplete enumeration", "post_release: incomplete enumeration", "callback liveness at snapshot not proved", "work/wait/timer/legacy callbacks did not all run", "callback drain not proved", "callback TID missing from enumeration", "scheduler inventory unknown"]

This validates coverage and token absence, not universal scheduler non-reachability. A successful duplicate is success; its grant and follow-up security operations must be assessed, not labelled denied.
