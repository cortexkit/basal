# Windows gap measurements: run 38048658581 / windows-latest

Source `6738c09d0117a665dfcb547319a305956d0bafd1`; recipe `full-gui-close-combined`; PID 7396; exit `0x00000000`.

## Gap 1: all live threads

The broker enumerates with SystemProcessInformation and Toolhelp at worker-requested barriers. The confined worker's enumeration errors are retained separately. Callbacks remain alive during the second snapshot; these are not permanently suspended threads.

### pre_pool_activity
Complete: True; SPI NumberOfThreads: 3; Toolhelp TIDs: [1204, 8260, 9096]

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|
| 1204 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 8260 | `0x00007ff6b3bb5cf0` | `!?+? (type None; error 487)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 9096 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |

### post_pool_activity
Complete: True; SPI NumberOfThreads: 7; Toolhelp TIDs: [1204, 1212, 1244, 8260, 9096, 9548, 10184]

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|
| 1204 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 1212 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 1244 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 8260 | `0x00007ff6b3bb5cf0` | `!?+? (type None; error 487)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 9096 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 9548 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 10184 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |

### post_release
Complete: True; SPI NumberOfThreads: 7; Toolhelp TIDs: [1204, 1212, 1244, 8260, 9096, 9548, 10184]

| TID | Start address | PDB symbol | NtOpenThreadTokenEx self=true | self=false | Token details |
|---:|---|---|---|---|---|
| 1204 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 1212 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 1244 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 8260 | `0x00007ff6b3bb5cf0` | `!?+? (type None; error 487)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 9096 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 9548 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |
| 10184 | `0x00007ffeaaa35bc0` | `ntdll!TppWorkerThread+0 (type 3; error None)` | `0xc000007c` | `0xc000007c` | absent only if STATUS_NO_TOKEN |

Callback barrier status: `0`; callback TIDs included: `True`.

| Callback | TID | self=true | self=false |
|---|---:|---|---|
| wait | 1244 | `0xc000007c` | `0xc000007c` |
| work | 1212 | `0xc000007c` | `0xc000007c` |
| legacy_work | 10184 | `0xc000007c` | `0xc000007c` |
| timer | 9548 | `0xc000007c` | `0xc000007c` |

## Gap 2: SchedulerSharedData

Correlation snapshot: `{"foreign_handles":[],"handle":68,"is_shared_across_processes":false,"ldr_analysis":{"note":"creation frame not in trace or already resolved"},"object":"0xffff808f239fde90","object_resolved":true,"present":true,"system_handle_count":50666}`

| Operation | Exact measured result (NTSTATUS unless explicitly Win32) |
|---|---|
| alpc_port_escape | `{"permitted":false,"status":"0xc0000024"}` |
| duplicate_access_trials | `[{"basic_query_status":"0x00000000","empty_dacl_input_valid":true,"granted_access":"0x00020000","make_temporary_status":"0xc0000022","requested_access":"0x80000000","same_dacl_write_status":"0xc0000022","same_owner_write_status":"0xc0000022","security_descriptor_hex":"0100008000000000000000000000000000000000","security_descriptor_valid":true,"security_read_status":"0x00000000","security_returned_bytes":20,"status":"0x00000000","success":true,"valid_current_user_owner_write_status":"0xc0000022","valid_empty_dacl_write_status":"0xc0000022","wait_zero":{"Win32_error": … (truncated; full report in the run artifact)
| duplicate_object | `{"generic_all":{"basic_query_status":"0x00000000","empty_dacl_input_valid":true,"granted_access":"0x000f0001","make_temporary_status":"0x00000000","same_dacl_write_status":"0xc00000d7","same_owner_write_status":"0xc0000079","security_descriptor_hex":"0100008000000000000000000000000000000000","security_descriptor_valid":true,"security_read_status":"0x00000000","security_returned_bytes":20,"status":"0x00000000","success":true,"valid_current_user_owner_write_status":"0xc00000d7","valid_empty_dacl_write_status":"0xc00000d7","wait_zero":{"Win32_error":5,"result":4294967295}}," … (truncated; full report in the run artifact)
| file_io | `{"device_io_control":{"error":6,"success":false},"permitted":false,"read_file":{"error":6,"success":false},"write_file":{"error":6,"success":false}}` |
| native_file_io | `{"NtReadFile":"0xc0000024","NtWriteFile":"0xc0000024"}` |
| process_scheduler_shared_data_query_112 | `{"buffer_bytes":8,"output_handle":68,"status":"0xc0000003"}` |
| process_scheduler_shared_data_set_112 | `{"buffer_bytes":8,"new_handle_closed":true,"output_handle":336,"output_object":{"basic_query_status":"0x00000000","empty_dacl_input_valid":true,"granted_access":"0x00000001","make_temporary_status":"0xc0000022","security_read_status":"0xc0000022","status":"0x00000000","success":true,"valid_current_user_owner_write_status":"0xc0000022","valid_empty_dacl_write_status":"0xc0000022","wait_zero":{"Win32_error":5,"result":4294967295}},"status":"0x00000000"}` |
| query_object_basic | `{"attributes":"0x00000000","granted_access":"0x00000001","handle_count":1,"pointer_count":32769,"status":"0x00000000"}` |
| query_object_name | `{"is_unnamed":true,"name":"","status":"0x00000000"}` |
| query_object_type | `{"generic_mapping":{"all":"0x000f0001","execute":"0x00020001","read":"0x00020000","write":"0x00020001"},"maintain_handle_count":false,"security_required":false,"status":"0x00000000","total_handles":145,"total_objects":145,"type_index":17,"type_name":"SchedulerSharedData","valid_access_mask":"0x001f0001"}` |
| query_section_basic | `{"returned_bytes":0,"status":"0xc0000024"}` |
| query_security_object | `{"permitted":false,"returned_bytes":16,"status":"0xc0000022"}` |
| root_directory_escape | `{"permitted":false,"status":"0xc0000024"}` |
| section_map | `{"permitted":false,"status":"0xc0000024"}` |
| thread_class_57_query_size_trials | `[{"buffer_bytes":8,"output_hex":"0000d38786020000","returned_bytes":8,"status":"0x00000000"},{"buffer_bytes":16,"output_hex":"02000000000000004400000000000000","returned_bytes":0,"status":"0xc0000004"},{"buffer_bytes":24,"output_hex":"020000000000000044000000000000000000000000000000","returned_bytes":0,"status":"0xc0000004"},{"buffer_bytes":32,"output_hex":"0200000000000000440000000000000000000000000000000000000000000000","returned_bytes":0,"status":"0xc0000004"},{"buffer_bytes":48,"output_hex":"020000000000000044000000000000000000000000000000000000000000 … (truncated; full report in the run artifact)
| thread_scheduler_shared_data_slot_class_57 | `{"class":57,"structure_bytes":24,"trials":[{"action":2,"api":"query","output_handle":68,"output_slot":"0x0000000000000000","status":"0xc0000004"},{"action":0,"api":"query","output_handle":68,"output_slot":"0x0000000000000000","status":"0xc0000004"},{"action":1,"api":"query","not_attempted":"no successful assignment to free"},{"action":2,"api":"set","output_handle":68,"output_slot":"0x0000000000000000","status":"0xc00000bb"},{"action":0,"api":"set","output_handle":68,"output_slot":"0x0000000000000000","status":"0xc00000bb"},{"action":1,"api":"set" … (truncated; full report in the run artifact)
| wait_for_single_object | `{"error":5,"permitted":false,"result":4294967295}` |

Loader syscall observations: `[{"api":"NtSetInformationProcess","buffer_bytes":8,"caller":140731761800849,"name":{"buffer_bytes":8,"handle":0},"phase":"entry","tid":876},{"NTSTATUS":"0x00000000","api":"NtSetInformationProcess","handle":{"buffer_bytes":8,"handle":64},"name":{"buffer_bytes":8,"handle":0},"phase":"return","tid":876}]`

Scheduler-close trial: `{"pid":5816,"exit_code":"0xc0000005","stderr":"probe-stage: Rust entry\nEXCEPTION: code=0x000006a6 address=0x00007ffea7c975fa\nEXCEPTION: code=0x000006a6 address=0x00007ffea7c975fa\nambient-close: handle=64 type=Some(\"SchedulerSharedData\") success=true\nprobe-stage: attestation complete\nprobe-stage: force work/wait/timer after closes (debugger observes faults)\npool-stage: SubmitThreadpoolWork\npool-stage: SetThreadpoolWait\npool-stage: SetThreadpoolTimer\npool-stage: QueueUserWorkItem\nprobe-stage: forced pool activity returned {\"callback_inspections\":[{\"kind\":\ … (truncated; full report in the run artifact)

Coverage violations: []

This validates coverage and token absence, not universal scheduler non-reachability. A successful duplicate is success; its grant and follow-up security operations must be assessed, not labelled denied.
