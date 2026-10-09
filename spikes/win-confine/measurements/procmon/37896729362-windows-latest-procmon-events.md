# Process Monitor child startup windows

## chrome-default-dacl-control (PID 5400)
Exit `0xc0000142`; 74 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 26935 | 7:03:46.9264559 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 26940 | 7:03:46.9266465 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 26943 | 7:03:46.9266909 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 26949 | 7:03:46.9267752 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 26950 | 7:03:46.9267915 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 26952 | 7:03:46.9268386 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 26953 | 7:03:46.9268708 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 26954 | 7:03:46.9268928 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 26956 | 7:03:46.9269318 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 26959 | 7:03:46.9269545 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 26960 | 7:03:46.9269734 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 26962 | 7:03:46.9269930 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 26964 | 7:03:46.9270182 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 26965 | 7:03:46.9270345 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 26966 | 7:03:46.9270510 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 26967 | 7:03:46.9270725 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 26968 | 7:03:46.9270896 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 26969 | 7:03:46.9271001 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 26970 | 7:03:46.9271229 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 26971 | 7:03:46.9271782 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 26975 | 7:03:46.9272196 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 26976 | 7:03:46.9272441 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 26977 | 7:03:46.9272626 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 26978 | 7:03:46.9272768 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 26979 | 7:03:46.9272895 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 26981 | 7:03:46.9273285 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 26982 | 7:03:46.9273387 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 26984 | 7:03:46.9274964 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 26987 | 7:03:46.9277002 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 27058 | 7:03:46.9307566 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 27063 | 7:03:46.9308733 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 27078 | 7:03:46.9314896 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## post-load-primary-control (PID 5820)
Exit `0xc0000142`; 39 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 29436 | 7:03:46.9770929 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 29438 | 7:03:46.9771182 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 29444 | 7:03:46.9774223 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 29445 | 7:03:46.9774587 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 29447 | 7:03:46.9775135 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 29449 | 7:03:46.9775586 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 29450 | 7:03:46.9775810 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 29451 | 7:03:46.9776128 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 29459 | 7:03:46.9778205 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 29469 | 7:03:46.9782719 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 29491 | 7:03:46.9798065 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 29495 | 7:03:46.9799135 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 29501 | 7:03:46.9803410 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:46.9782719 AM', 'Process Name': 'win-confine.exe', 'PID': '5820', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 29469}`

## chrome-context-control (PID 4268)
Exit `0xc0000142`; 72 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 29945 | 7:03:47.0058969 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 29957 | 7:03:47.0062058 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 29959 | 7:03:47.0062580 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 29967 | 7:03:47.0063655 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 29968 | 7:03:47.0063839 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 29970 | 7:03:47.0064006 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 29972 | 7:03:47.0064167 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 29974 | 7:03:47.0064329 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 29975 | 7:03:47.0064510 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 29977 | 7:03:47.0064673 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 29978 | 7:03:47.0064842 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 29980 | 7:03:47.0065005 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 29981 | 7:03:47.0065166 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 29983 | 7:03:47.0065340 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 29984 | 7:03:47.0065535 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 29985 | 7:03:47.0065719 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 29986 | 7:03:47.0065898 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 29989 | 7:03:47.0066474 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 29991 | 7:03:47.0066667 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 29992 | 7:03:47.0066843 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 29994 | 7:03:47.0067236 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 29996 | 7:03:47.0067411 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 29997 | 7:03:47.0067657 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 30000 | 7:03:47.0068240 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 30002 | 7:03:47.0068518 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 30006 | 7:03:47.0069215 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 30007 | 7:03:47.0069419 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 30018 | 7:03:47.0071836 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 30020 | 7:03:47.0073182 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 30021 | 7:03:47.0075188 AM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 30062 | 7:03:47.0104566 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 30069 | 7:03:47.0105689 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 30084 | 7:03:47.0113040 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.0075188 AM', 'Process Name': 'win-confine.exe', 'PID': '4268', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 30021}`

## lpac-context-control (PID 5308)
Exit `0xc0000142`; 41 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 30457 | 7:03:47.0232308 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 30458 | 7:03:47.0232572 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 30461 | 7:03:47.0234312 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 30462 | 7:03:47.0234642 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 30465 | 7:03:47.0235137 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 30466 | 7:03:47.0235618 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 30467 | 7:03:47.0235856 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 30468 | 7:03:47.0236164 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 30471 | 7:03:47.0238433 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 30480 | 7:03:47.0257474 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 30484 | 7:03:47.0258147 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 30490 | 7:03:47.0262647 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.0236164 AM', 'Process Name': 'win-confine.exe', 'PID': '5308', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 30468}`

## post-load-cwd-grant-control (PID 5620)
Exit `0xc0000142`; 41 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 38510 | 7:03:47.2519236 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 38511 | 7:03:47.2519523 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 38513 | 7:03:47.2521038 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 38515 | 7:03:47.2521307 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 38518 | 7:03:47.2522424 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38519 | 7:03:47.2522940 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 38520 | 7:03:47.2523143 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 38521 | 7:03:47.2523434 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38524 | 7:03:47.2525516 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 38541 | 7:03:47.2546635 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 38545 | 7:03:47.2547322 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 38551 | 7:03:47.2551672 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.2523434 AM', 'Process Name': 'win-confine.exe', 'PID': '5620', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 38521}`

## post-load-detached-control (PID 8376)
Exit `0x00000000`; 486 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 38889 | 7:03:47.2643828 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 38890 | 7:03:47.2644085 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 38891 | 7:03:47.2644965 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 38892 | 7:03:47.2645174 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 38893 | 7:03:47.2645521 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38894 | 7:03:47.2646221 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 38895 | 7:03:47.2646425 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 38896 | 7:03:47.2646689 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38902 | 7:03:47.2648642 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 38917 | 7:03:47.2652906 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 38944 | 7:03:47.2666536 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 38945 | 7:03:47.2666869 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 38959 | 7:03:47.2673229 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38966 | 7:03:47.2676744 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 38967 | 7:03:47.2677092 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 38968 | 7:03:47.2677393 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 38969 | 7:03:47.2677620 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 38971 | 7:03:47.2677855 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 38973 | 7:03:47.2678649 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 38974 | 7:03:47.2679039 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 38976 | 7:03:47.2679243 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 39023 | 7:03:47.2707531 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 39025 | 7:03:47.2707743 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 39030 | 7:03:47.2709866 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 39031 | 7:03:47.2710000 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 39075 | 7:03:47.2729558 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979 |
| 39077 | 7:03:47.2729774 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979, Priority: Low |
| 40012 | 7:03:47.3112965 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 40399 | 7:03:47.3224587 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.2710000 AM', 'Process Name': 'win-confine.exe', 'PID': '8376', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 39031}`

## chrome-detached-control (PID 3648)
Exit `0x00000000`; 541 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 41073 | 7:03:47.3542024 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 41081 | 7:03:47.3544097 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 41084 | 7:03:47.3544625 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 41092 | 7:03:47.3545592 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 41094 | 7:03:47.3545783 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 41095 | 7:03:47.3545949 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 41097 | 7:03:47.3546119 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 41098 | 7:03:47.3546298 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 41100 | 7:03:47.3546864 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 41101 | 7:03:47.3547239 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 41102 | 7:03:47.3547693 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 41103 | 7:03:47.3547911 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 41104 | 7:03:47.3548071 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 41105 | 7:03:47.3548220 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 41106 | 7:03:47.3548363 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 41108 | 7:03:47.3548520 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 41109 | 7:03:47.3548673 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 41110 | 7:03:47.3548834 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 41111 | 7:03:47.3549006 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 41112 | 7:03:47.3549178 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 41115 | 7:03:47.3549546 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 41116 | 7:03:47.3549896 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 41119 | 7:03:47.3550177 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 41122 | 7:03:47.3550731 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 41125 | 7:03:47.3550992 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 41133 | 7:03:47.3551841 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 41134 | 7:03:47.3552032 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 41138 | 7:03:47.3554457 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 41149 | 7:03:47.3557377 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 41162 | 7:03:47.3577492 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 41163 | 7:03:47.3579189 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b749553b-d950-5e03-6282-3145a61b1002 | NAME NOT FOUND | Length: 528 |
| 41164 | 7:03:47.3579613 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 41165 | 7:03:47.3579782 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 41166 | 7:03:47.3580893 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 41168 | 7:03:47.3581588 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 41169 | 7:03:47.3581701 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 41171 | 7:03:47.3587209 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 41172 | 7:03:47.3587340 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 41173 | 7:03:47.3587487 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 41174 | 7:03:47.3587593 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 41176 | 7:03:47.3587908 AM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 41178 | 7:03:47.3588165 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 41179 | 7:03:47.3588424 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 41182 | 7:03:47.3588777 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 41294 | 7:03:47.3637453 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 41303 | 7:03:47.3640179 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\5fe982db-5db8-4dc7-9814-2b3ceb277f7e | NAME NOT FOUND | Length: 528 |
| 41305 | 7:03:47.3641938 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 41316 | 7:03:47.3646107 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 41317 | 7:03:47.3646746 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 41323 | 7:03:47.3648265 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 41328 | 7:03:47.3648986 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 41331 | 7:03:47.3649486 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 41334 | 7:03:47.3649969 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySleepLoopWindowSize | NAME NOT FOUND | Length: 80 |
| 41335 | 7:03:47.3650170 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySpinCountThreshold | NAME NOT FOUND | Length: 80 |
| 41336 | 7:03:47.3650359 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayBaseYield | NAME NOT FOUND | Length: 80 |
| 41337 | 7:03:47.3650541 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtFactorYield | NAME NOT FOUND | Length: 80 |
| 41338 | 7:03:47.3650718 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayMaxYield | NAME NOT FOUND | Length: 80 |
| 41406 | 7:03:47.3678855 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 19,979 |
| 41408 | 7:03:47.3679061 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979, Priority: Low |
| 41500 | 7:03:47.3737333 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 42209 | 7:03:47.3867179 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 42588 | 7:03:47.3946799 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## lpac-context-detached-control (PID 9408)
Exit `0x00000000`; 490 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 43075 | 7:03:47.4220872 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 43076 | 7:03:47.4221138 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 43079 | 7:03:47.4222439 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 43080 | 7:03:47.4222832 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 43081 | 7:03:47.4223502 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 43084 | 7:03:47.4223947 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 43085 | 7:03:47.4224085 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 43086 | 7:03:47.4224294 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 43087 | 7:03:47.4226107 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 43097 | 7:03:47.4241912 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 43098 | 7:03:47.4242079 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 43099 | 7:03:47.4245730 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 43100 | 7:03:47.4248068 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 43101 | 7:03:47.4248204 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 43102 | 7:03:47.4248351 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 43103 | 7:03:47.4248457 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 43104 | 7:03:47.4248588 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 43105 | 7:03:47.4248900 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 43106 | 7:03:47.4249120 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 43107 | 7:03:47.4249236 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 43144 | 7:03:47.4274258 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 43145 | 7:03:47.4274443 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 43148 | 7:03:47.4276430 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 43149 | 7:03:47.4276558 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 43172 | 7:03:47.4285255 AM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 43216 | 7:03:47.4300791 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 19,979 |
| 43218 | 7:03:47.4300995 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979, Priority: Low |
| 43366 | 7:03:47.4391420 AM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 43367 | 7:03:47.4392372 AM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 43368 | 7:03:47.4393513 AM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 43369 | 7:03:47.4394332 AM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 44529 | 7:03:47.4601069 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.4276558 AM', 'Process Name': 'win-confine.exe', 'PID': '9408', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 43149}`

## chrome-context-detached-control (PID 1032)
Exit `0x00000000`; 537 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 45343 | 7:03:47.4999361 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 45349 | 7:03:47.5001264 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 45351 | 7:03:47.5001692 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 45359 | 7:03:47.5002913 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 45361 | 7:03:47.5003167 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 45362 | 7:03:47.5003349 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 45363 | 7:03:47.5003512 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 45364 | 7:03:47.5003665 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 45365 | 7:03:47.5003844 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 45366 | 7:03:47.5004027 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 45367 | 7:03:47.5004190 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 45369 | 7:03:47.5004355 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 45370 | 7:03:47.5004521 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 45371 | 7:03:47.5004686 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 45372 | 7:03:47.5004849 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 45374 | 7:03:47.5005010 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 45375 | 7:03:47.5005160 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 45377 | 7:03:47.5005322 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 45379 | 7:03:47.5005491 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 45380 | 7:03:47.5005645 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 45383 | 7:03:47.5006015 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 45386 | 7:03:47.5006194 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 45389 | 7:03:47.5006432 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 45390 | 7:03:47.5006663 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 45391 | 7:03:47.5006916 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 45395 | 7:03:47.5007525 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 45396 | 7:03:47.5007706 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 45405 | 7:03:47.5010354 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 45412 | 7:03:47.5011720 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 45418 | 7:03:47.5013730 AM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 45480 | 7:03:47.5039245 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 45481 | 7:03:47.5041541 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b749553b-d950-5e03-6282-3145a61b1002 | NAME NOT FOUND | Length: 528 |
| 45482 | 7:03:47.5042134 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 45483 | 7:03:47.5042384 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 45484 | 7:03:47.5044153 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 45488 | 7:03:47.5045363 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 45489 | 7:03:47.5045632 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 45518 | 7:03:47.5056379 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 45520 | 7:03:47.5056630 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 45521 | 7:03:47.5056923 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 45522 | 7:03:47.5057183 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 45526 | 7:03:47.5057788 AM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 45529 | 7:03:47.5058302 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 45532 | 7:03:47.5058848 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 45540 | 7:03:47.5059636 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 45571 | 7:03:47.5100805 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 45575 | 7:03:47.5103328 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\5fe982db-5db8-4dc7-9814-2b3ceb277f7e | NAME NOT FOUND | Length: 528 |
| 45576 | 7:03:47.5105189 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 45577 | 7:03:47.5108321 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 45578 | 7:03:47.5108952 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 45579 | 7:03:47.5110324 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 45580 | 7:03:47.5110963 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 45581 | 7:03:47.5111450 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 45585 | 7:03:47.5111938 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySleepLoopWindowSize | NAME NOT FOUND | Length: 80 |
| 45586 | 7:03:47.5112143 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySpinCountThreshold | NAME NOT FOUND | Length: 80 |
| 45587 | 7:03:47.5112330 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayBaseYield | NAME NOT FOUND | Length: 80 |
| 45588 | 7:03:47.5112512 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtFactorYield | NAME NOT FOUND | Length: 80 |
| 45589 | 7:03:47.5112690 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayMaxYield | NAME NOT FOUND | Length: 80 |
| 45665 | 7:03:47.5137899 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 19,979 |
| 45667 | 7:03:47.5138071 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979, Priority: Low |
| 45827 | 7:03:47.5219343 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 47137 | 7:03:47.5447870 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.5013730 AM', 'Process Name': 'win-confine.exe', 'PID': '1032', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 45418}`

## full-detached-control (PID 5484)
Exit `0x00000000`; 475 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 47474 | 7:03:47.5678342 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 47477 | 7:03:47.5678633 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 47480 | 7:03:47.5679663 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 47481 | 7:03:47.5679901 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 47482 | 7:03:47.5680281 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 47483 | 7:03:47.5680656 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 47484 | 7:03:47.5680871 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 47486 | 7:03:47.5681558 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 47490 | 7:03:47.5684029 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 47495 | 7:03:47.5687522 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 47500 | 7:03:47.5700723 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 47501 | 7:03:47.5700891 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 47502 | 7:03:47.5704002 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 47503 | 7:03:47.5705943 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 47504 | 7:03:47.5706065 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 47505 | 7:03:47.5706206 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 47506 | 7:03:47.5706312 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 47507 | 7:03:47.5706441 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 47508 | 7:03:47.5707244 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 47509 | 7:03:47.5707563 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 47510 | 7:03:47.5707694 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 47581 | 7:03:47.5736320 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 47582 | 7:03:47.5736652 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 47595 | 7:03:47.5739874 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 47596 | 7:03:47.5740070 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 47653 | 7:03:47.5763885 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 19,979 |
| 47655 | 7:03:47.5764079 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979, Priority: Low |
| 49084 | 7:03:47.6067935 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.5740070 AM', 'Process Name': 'win-confine.exe', 'PID': '5484', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 47596}`

## chrome-untrusted-detached-control (PID 7756)
Exit `0xc00000a5`; 26 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 49442 | 7:03:47.6401705 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 49443 | 7:03:47.6402452 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 49446 | 7:03:47.6403346 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 49447 | 7:03:47.6403605 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 49448 | 7:03:47.6403864 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 49449 | 7:03:47.6404120 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 49450 | 7:03:47.6404319 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 49451 | 7:03:47.6404588 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 49452 | 7:03:47.6406239 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 49460 | 7:03:47.6410272 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.6404588 AM', 'Process Name': 'win-confine.exe', 'PID': '7756', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 49451}`

## full-gui-control (PID 7600)
Exit `0x00000000`; 526 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 49829 | 7:03:47.6528434 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 49830 | 7:03:47.6528763 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 49833 | 7:03:47.6529749 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 49834 | 7:03:47.6529956 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 49835 | 7:03:47.6530703 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 49836 | 7:03:47.6530904 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 49837 | 7:03:47.6531184 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 49839 | 7:03:47.6532940 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 49843 | 7:03:47.6536239 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 49848 | 7:03:47.6550739 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 49849 | 7:03:47.6551037 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 49863 | 7:03:47.6559122 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 49864 | 7:03:47.6559351 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 49866 | 7:03:47.6559599 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 49867 | 7:03:47.6559825 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 49868 | 7:03:47.6560075 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 49869 | 7:03:47.6560624 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 49871 | 7:03:47.6561019 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 49873 | 7:03:47.6561227 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 49879 | 7:03:47.6562848 AM | QueryOpen | C:\Windows\System32\apphelp.dll | FAST IO DISALLOWED |  |
| 49893 | 7:03:47.6567149 AM | CreateFileMapping | C:\Windows\System32\apphelp.dll | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 49905 | 7:03:47.6575350 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags | ACCESS DENIED | Desired Access: Query Value |
| 49907 | 7:03:47.6575961 AM | RegOpenKey | HKLM\OSDATA\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags | NAME NOT FOUND | Desired Access: Query Value |
| 49910 | 7:03:47.6577473 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags | ACCESS DENIED | Desired Access: Query Value |
| 49914 | 7:03:47.6579229 AM | CreateFile | \Device\NamedPipe\GmdAslLogger | NAME NOT FOUND | Desired Access: Generic Write, Read Attributes, Disposition: Open, Options: Synchronous IO Non-Alert, Non-Directory File, Attributes: n/a, ShareMode: None, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 49944 | 7:03:47.6588753 AM | QuerySecurityFile | D:\a\basal\basal\spikes\win-confine\target\release\placed\win-confine-gui.exe | BUFFER OVERFLOW | Information: Owner |
| 49957 | 7:03:47.6591793 AM | QuerySecurityFile | C:\Windows\System32\ntdll.dll | BUFFER OVERFLOW | Information: Owner |
| 49970 | 7:03:47.6594561 AM | QuerySecurityFile | C:\Windows\System32\kernel32.dll | BUFFER OVERFLOW | Information: Owner |
| 49975 | 7:03:47.6596095 AM | QuerySecurityFile | C:\Windows\System32\KernelBase.dll | BUFFER OVERFLOW | Information: Owner |
| 49979 | 7:03:47.6596980 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\SdbUpdates | ACCESS DENIED | Desired Access: Read |
| 49983 | 7:03:47.6598860 AM | CreateFileMapping | C:\Windows\apppatch\sysmain.sdb | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_READONLY |
| 49990 | 7:03:47.6600034 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\SdbUpdates | ACCESS DENIED | Desired Access: Read |
| 50007 | 7:03:47.6625933 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 50008 | 7:03:47.6626569 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 50009 | 7:03:47.6629924 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 50010 | 7:03:47.6630159 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 50038 | 7:03:47.6655697 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 19,979 |
| 50040 | 7:03:47.6655876 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 19,979, Priority: Low |
| 50228 | 7:03:47.6762340 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 50408 | 7:03:47.6778731 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,019 |
| 50858 | 7:03:47.6849767 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine-gui.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.6630159 AM', 'Process Name': 'win-confine-gui.exe', 'PID': '7600', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 50010}`

## chrome-untrusted-gui-control (PID 8656)
Exit `0xc00000a5`; 25 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 51465 | 7:03:47.7206666 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 51466 | 7:03:47.7206905 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 51467 | 7:03:47.7207629 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 51468 | 7:03:47.7207815 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 51469 | 7:03:47.7208041 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 51470 | 7:03:47.7208219 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 51471 | 7:03:47.7208480 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 51473 | 7:03:47.7209987 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 51490 | 7:03:47.7220399 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine-gui.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:47.7208480 AM', 'Process Name': 'win-confine-gui.exe', 'PID': '8656', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 51471}`
