# Windows confinement verification

CI run [38075560568](https://github.com/cortexkit/basal/actions/runs/38075560568),
source commit `794a02070702c8230559ef3c3610412cb7ec1f7f`.

Tool versions from the REST job logs: `rustc 1.99.0 (b940084d7 2026-09-28)`;
`cargo 1.99.0 (5f94df478 2026-08-27)`.

The excerpts below come from the uploaded Windows baseline artifacts, not
from the continue-on-error job conclusions. Default builds passed 77 worker
tests and deviations builds passed 79 per image. Each includes 14 new probe
tests, with unchanged production startup checks.

## windows-latest

Artifact ID: `11678725643`.

### Production worker

```text
test a_real_named_pipe_is_denied_with_plain_control ... ok
test create_thread_succeeds_inside_full_confinement ... ok
test excluded_inheritable_handle_is_fatal_only_after_readiness_and_plain_dropped_list_writes ... ok
test child_creation_and_parent_process_opens_are_denied_with_plain_controls ... ok
test executable_allocation_and_rw_to_x_are_denied_with_lpac_controls ... ok
test fatal_exit_before_readiness_is_not_a_denial ... ok
test forced_pool_work_wait_timer_and_legacy_work_complete ... ok
test native_package_hive_write_is_denied_with_lpac_control ... ok
test native_winsock_tcp_udp_and_loopback_are_denied_and_parent_observes_controls ... ok
test new_alpc_epmapper_and_package_section_are_denied_with_lpac_controls ... ok
test package_folder_create_is_denied_with_lpac_control ... ok
test planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture ... ok
test system_image_data_read_and_stat_are_denied_with_lpac_controls ... ok
test thread_barrier_checks_every_thread_before_and_during_held_pool_callbacks ... ok
windows-pool-complete:work,wait,timer,legacy
tcp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
loopback: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
udp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
before-pool: TID 2480 open_as_self=0 0xc000007c
before-pool: TID 2480 open_as_self=1 0xc000007c
before-pool: TID 3984 open_as_self=0 0xc000007c
before-pool: TID 3984 open_as_self=1 0xc000007c
before-pool: TID 10044 open_as_self=0 0xc000007c
before-pool: TID 10044 open_as_self=1 0xc000007c
before-pool: SystemProcessInformation == Toolhelp (3 threads)
callbacks-held: TID 976 open_as_self=0 0xc000007c
callbacks-held: TID 976 open_as_self=1 0xc000007c
callbacks-held: TID 2480 open_as_self=0 0xc000007c
callbacks-held: TID 2480 open_as_self=1 0xc000007c
callbacks-held: TID 3984 open_as_self=0 0xc000007c
callbacks-held: TID 3984 open_as_self=1 0xc000007c
callbacks-held: TID 5564 open_as_self=0 0xc000007c
callbacks-held: TID 5564 open_as_self=1 0xc000007c
callbacks-held: TID 6220 open_as_self=0 0xc000007c
callbacks-held: TID 6220 open_as_self=1 0xc000007c
callbacks-held: TID 6504 open_as_self=0 0xc000007c
callbacks-held: TID 6504 open_as_self=1 0xc000007c
callbacks-held: TID 10044 open_as_self=0 0xc000007c
callbacks-held: TID 10044 open_as_self=1 0xc000007c
callbacks-held: SystemProcessInformation == Toolhelp (7 threads)
every callback TID is in the snapshot: {"legacy": 976, "timer": 6504, "wait": 5564, "work": 6220}
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.96s
```

### Worker with deviations

```text
test a_real_named_pipe_is_denied_with_plain_control ... ok
test child_creation_and_parent_process_opens_are_denied_with_plain_controls ... ok
test excluded_inheritable_handle_is_fatal_only_after_readiness_and_plain_dropped_list_writes ... ok
test create_thread_succeeds_inside_full_confinement ... ok
test executable_allocation_and_rw_to_x_are_denied_with_lpac_controls ... ok
test fatal_exit_before_readiness_is_not_a_denial ... ok
test forced_pool_work_wait_timer_and_legacy_work_complete ... ok
test native_package_hive_write_is_denied_with_lpac_control ... ok
test native_winsock_tcp_udp_and_loopback_are_denied_and_parent_observes_controls ... ok
test new_alpc_epmapper_and_package_section_are_denied_with_lpac_controls ... ok
test package_folder_create_is_denied_with_lpac_control ... ok
test planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture ... ok
test system_image_data_read_and_stat_are_denied_with_lpac_controls ... ok
test thread_barrier_checks_every_thread_before_and_during_held_pool_callbacks ... ok
windows-pool-complete:work,wait,timer,legacy
tcp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
loopback: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
udp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
before-pool: TID 6940 open_as_self=0 0xc000007c
before-pool: TID 6940 open_as_self=1 0xc000007c
before-pool: TID 7612 open_as_self=0 0xc000007c
before-pool: TID 7612 open_as_self=1 0xc000007c
before-pool: TID 8524 open_as_self=0 0xc000007c
before-pool: TID 8524 open_as_self=1 0xc000007c
before-pool: TID 8612 open_as_self=0 0xc000007c
before-pool: TID 8612 open_as_self=1 0xc000007c
before-pool: SystemProcessInformation == Toolhelp (4 threads)
callbacks-held: TID 3116 open_as_self=0 0xc000007c
callbacks-held: TID 3116 open_as_self=1 0xc000007c
callbacks-held: TID 5208 open_as_self=0 0xc000007c
callbacks-held: TID 5208 open_as_self=1 0xc000007c
callbacks-held: TID 6940 open_as_self=0 0xc000007c
callbacks-held: TID 6940 open_as_self=1 0xc000007c
callbacks-held: TID 7612 open_as_self=0 0xc000007c
callbacks-held: TID 7612 open_as_self=1 0xc000007c
callbacks-held: TID 8264 open_as_self=0 0xc000007c
callbacks-held: TID 8264 open_as_self=1 0xc000007c
callbacks-held: TID 8524 open_as_self=0 0xc000007c
callbacks-held: TID 8524 open_as_self=1 0xc000007c
callbacks-held: TID 8612 open_as_self=0 0xc000007c
callbacks-held: TID 8612 open_as_self=1 0xc000007c
callbacks-held: TID 10108 open_as_self=0 0xc000007c
callbacks-held: TID 10108 open_as_self=1 0xc000007c
callbacks-held: SystemProcessInformation == Toolhelp (8 threads)
every callback TID is in the snapshot: {"legacy": 10108, "timer": 5208, "wait": 3116, "work": 8264}
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.96s
```

### Production image and mutation witnesses

```text
production inventory "windows-latest": 25 handles; saved D:\a\_temp/windows-baseline/windows-handle-provenance.json
subsystem: 2
imports: advapi32.dll, api-ms-win-core-synch-l1-2-0.dll, bcryptprimitives.dll, kernel32.dll, ntdll.dll
delay imports: []
```

The inventory success line is emitted only after the traced checkpoint child
exits 0. Diagnostic handle tracing was disabled after snapshot; permanent
strict-handle checks remained enabled. The production image has no static
Winsock import. Only socket probes preload Winsock before their unchanged
startup checks; they attempt startup and sockets after attestation/readiness.

Unedited mutation records: [JSON](windows-mutation-evidence-windows-latest.json).

- Startup allowlist removed: only
  `planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture`
  fails (13 tests filtered). It reports exit 0 instead of 70 and the parent
  fixture contains `[101, 115, 99, 97, 112, 101, 100]` (`escaped`).
- Kernel32 removed from the committed import inventory: only
  `the_worker_is_a_gui_image_without_gui_com_or_c_runtime_imports` fails
  (the C-runtime-name test is filtered). The assertion says
  `kernel32.dll is not in` the committed inventory.
- Each mutation records a non-empty applied Git diff and an empty restored
  diff; the restored named test passes. No other tests ran in the red checks.

## windows-2022

Artifact ID: `11678860399`.

### Production worker

```text
test a_real_named_pipe_is_denied_with_plain_control ... ok
test create_thread_succeeds_inside_full_confinement ... ok
test child_creation_and_parent_process_opens_are_denied_with_plain_controls ... ok
test excluded_inheritable_handle_is_fatal_only_after_readiness_and_plain_dropped_list_writes ... ok
test executable_allocation_and_rw_to_x_are_denied_with_lpac_controls ... ok
test fatal_exit_before_readiness_is_not_a_denial ... ok
test forced_pool_work_wait_timer_and_legacy_work_complete ... ok
test native_package_hive_write_is_denied_with_lpac_control ... ok
test native_winsock_tcp_udp_and_loopback_are_denied_and_parent_observes_controls ... ok
test new_alpc_epmapper_and_package_section_are_denied_with_lpac_controls ... ok
test package_folder_create_is_denied_with_lpac_control ... ok
test planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture ... ok
test system_image_data_read_and_stat_are_denied_with_lpac_controls ... ok
test thread_barrier_checks_every_thread_before_and_during_held_pool_callbacks ... ok
windows-pool-complete:work,wait,timer,legacy
tcp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
loopback: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
udp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
before-pool: TID 3096 open_as_self=0 0xc000007c
before-pool: TID 3096 open_as_self=1 0xc000007c
before-pool: TID 3968 open_as_self=0 0xc000007c
before-pool: TID 3968 open_as_self=1 0xc000007c
before-pool: TID 5384 open_as_self=0 0xc000007c
before-pool: TID 5384 open_as_self=1 0xc000007c
before-pool: TID 7188 open_as_self=0 0xc000007c
before-pool: TID 7188 open_as_self=1 0xc000007c
before-pool: SystemProcessInformation == Toolhelp (4 threads)
callbacks-held: TID 2024 open_as_self=0 0xc000007c
callbacks-held: TID 2024 open_as_self=1 0xc000007c
callbacks-held: TID 2088 open_as_self=0 0xc000007c
callbacks-held: TID 2088 open_as_self=1 0xc000007c
callbacks-held: TID 3096 open_as_self=0 0xc000007c
callbacks-held: TID 3096 open_as_self=1 0xc000007c
callbacks-held: TID 3968 open_as_self=0 0xc000007c
callbacks-held: TID 3968 open_as_self=1 0xc000007c
callbacks-held: TID 5384 open_as_self=0 0xc000007c
callbacks-held: TID 5384 open_as_self=1 0xc000007c
callbacks-held: TID 6112 open_as_self=0 0xc000007c
callbacks-held: TID 6112 open_as_self=1 0xc000007c
callbacks-held: TID 7188 open_as_self=0 0xc000007c
callbacks-held: TID 7188 open_as_self=1 0xc000007c
callbacks-held: TID 7976 open_as_self=0 0xc000007c
callbacks-held: TID 7976 open_as_self=1 0xc000007c
callbacks-held: SystemProcessInformation == Toolhelp (8 threads)
every callback TID is in the snapshot: {"legacy": 7976, "timer": 2024, "wait": 2088, "work": 6112}
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.78s
```

### Worker with deviations

```text
test child_creation_and_parent_process_opens_are_denied_with_plain_controls ... ok
test a_real_named_pipe_is_denied_with_plain_control ... ok
test create_thread_succeeds_inside_full_confinement ... ok
test excluded_inheritable_handle_is_fatal_only_after_readiness_and_plain_dropped_list_writes ... ok
test executable_allocation_and_rw_to_x_are_denied_with_lpac_controls ... ok
test fatal_exit_before_readiness_is_not_a_denial ... ok
test forced_pool_work_wait_timer_and_legacy_work_complete ... ok
test native_package_hive_write_is_denied_with_lpac_control ... ok
test native_winsock_tcp_udp_and_loopback_are_denied_and_parent_observes_controls ... ok
test new_alpc_epmapper_and_package_section_are_denied_with_lpac_controls ... ok
test package_folder_create_is_denied_with_lpac_control ... ok
test planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture ... ok
test system_image_data_read_and_stat_are_denied_with_lpac_controls ... ok
test thread_barrier_checks_every_thread_before_and_during_held_pool_callbacks ... ok
windows-pool-complete:work,wait,timer,legacy
tcp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
loopback: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
udp: WSAStartup 10107, socket 10093, no parent traffic; plain traffic observed
before-pool: TID 5152 open_as_self=0 0xc000007c
before-pool: TID 5152 open_as_self=1 0xc000007c
before-pool: TID 5608 open_as_self=0 0xc000007c
before-pool: TID 5608 open_as_self=1 0xc000007c
before-pool: TID 5988 open_as_self=0 0xc000007c
before-pool: TID 5988 open_as_self=1 0xc000007c
before-pool: TID 7496 open_as_self=0 0xc000007c
before-pool: TID 7496 open_as_self=1 0xc000007c
before-pool: SystemProcessInformation == Toolhelp (4 threads)
callbacks-held: TID 2244 open_as_self=0 0xc000007c
callbacks-held: TID 2244 open_as_self=1 0xc000007c
callbacks-held: TID 2480 open_as_self=0 0xc000007c
callbacks-held: TID 2480 open_as_self=1 0xc000007c
callbacks-held: TID 5152 open_as_self=0 0xc000007c
callbacks-held: TID 5152 open_as_self=1 0xc000007c
callbacks-held: TID 5608 open_as_self=0 0xc000007c
callbacks-held: TID 5608 open_as_self=1 0xc000007c
callbacks-held: TID 5988 open_as_self=0 0xc000007c
callbacks-held: TID 5988 open_as_self=1 0xc000007c
callbacks-held: TID 6324 open_as_self=0 0xc000007c
callbacks-held: TID 6324 open_as_self=1 0xc000007c
callbacks-held: TID 7496 open_as_self=0 0xc000007c
callbacks-held: TID 7496 open_as_self=1 0xc000007c
callbacks-held: TID 7780 open_as_self=0 0xc000007c
callbacks-held: TID 7780 open_as_self=1 0xc000007c
callbacks-held: SystemProcessInformation == Toolhelp (8 threads)
every callback TID is in the snapshot: {"legacy": 6324, "timer": 7780, "wait": 2244, "work": 2480}
test result: ok. 14 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.77s
```

### Production image and mutation witnesses

```text
production inventory "windows-2022": 25 handles; saved D:\a\_temp/windows-baseline/windows-handle-provenance.json
subsystem: 2
imports: advapi32.dll, api-ms-win-core-synch-l1-2-0.dll, bcryptprimitives.dll, kernel32.dll, ntdll.dll
delay imports: []
```

The inventory success line is emitted only after the traced checkpoint child
exits 0. Diagnostic handle tracing was disabled after snapshot; permanent
strict-handle checks remained enabled. The production image has no static
Winsock import. Only socket probes preload Winsock before their unchanged
startup checks; they attempt startup and sockets after attestation/readiness.

Unedited mutation records: [JSON](windows-mutation-evidence-windows-2022.json).

- Startup allowlist removed: only
  `planted_handle_startup_refuses_and_the_allowlist_mutant_writes_the_fixture`
  fails (13 tests filtered). It reports exit 0 instead of 70 and the parent
  fixture contains `[101, 115, 99, 97, 112, 101, 100]` (`escaped`).
- Kernel32 removed from the committed import inventory: only
  `the_worker_is_a_gui_image_without_gui_com_or_c_runtime_imports` fails
  (the C-runtime-name test is filtered). The assertion says
  `kernel32.dll is not in` the committed inventory.
- Each mutation records a non-empty applied Git diff and an empty restored
  diff; the restored named test passes. No other tests ran in the red checks.

## Unrelated workspace baseline failure

Both Windows workspace check/test commands still fail in
`crates/basal-core/src/retention.rs`: Unix `OsStrExt` / `OsStringExt` APIs
(`as_bytes`, `from_vec`, and `std::os::unix`) do not compile on Windows.
The package-scoped worker and launcher gates above are green. No file in
basal-core was changed by these worker probes.
