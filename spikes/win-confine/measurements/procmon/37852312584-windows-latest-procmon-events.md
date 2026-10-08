# Process Monitor child startup windows

## chrome-default-dacl-control (PID 9860)
Exit `0xc0000142`; 74 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 25362 | 10:17:01.2766135 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 25369 | 10:17:01.2768574 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 25372 | 10:17:01.2769132 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 25376 | 10:17:01.2769707 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 25377 | 10:17:01.2769811 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 25378 | 10:17:01.2769902 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 25379 | 10:17:01.2769993 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 25380 | 10:17:01.2770083 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 25381 | 10:17:01.2770171 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 25382 | 10:17:01.2770260 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 25383 | 10:17:01.2770348 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 25384 | 10:17:01.2770439 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 25385 | 10:17:01.2770530 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 25386 | 10:17:01.2770619 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 25387 | 10:17:01.2770708 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 25388 | 10:17:01.2770798 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 25389 | 10:17:01.2770887 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 25390 | 10:17:01.2770976 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 25391 | 10:17:01.2771070 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 25392 | 10:17:01.2771159 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 25394 | 10:17:01.2771361 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 25395 | 10:17:01.2771470 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 25396 | 10:17:01.2771598 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 25398 | 10:17:01.2771717 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 25399 | 10:17:01.2771900 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 25401 | 10:17:01.2772910 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 25402 | 10:17:01.2773233 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 25406 | 10:17:01.2775782 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 25409 | 10:17:01.2778160 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 25445 | 10:17:01.2807944 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 25449 | 10:17:01.2809226 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 25463 | 10:17:01.2816168 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## post-load-primary-control (PID 5296)
Exit `0xc0000142`; 39 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 26670 | 10:17:01.3187812 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 26671 | 10:17:01.3188103 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 26673 | 10:17:01.3189489 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 26674 | 10:17:01.3189716 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 26676 | 10:17:01.3190146 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 26678 | 10:17:01.3190582 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 26680 | 10:17:01.3190799 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 26682 | 10:17:01.3191114 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 26685 | 10:17:01.3193547 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 26695 | 10:17:01.3197267 PM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 26704 | 10:17:01.3213674 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 26708 | 10:17:01.3214887 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 26717 | 10:17:01.3220681 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:17:01.3197267 PM', 'Process Name': 'win-confine.exe', 'PID': '5296', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 26695}`

## chrome-context-control (PID 2184)
Exit `0xc0000142`; 72 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 27656 | 10:17:01.3495566 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 27661 | 10:17:01.3497711 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 27663 | 10:17:01.3498240 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 27667 | 10:17:01.3499362 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 27668 | 10:17:01.3499570 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 27669 | 10:17:01.3499741 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 27670 | 10:17:01.3499918 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 27671 | 10:17:01.3500106 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 27672 | 10:17:01.3500282 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 27673 | 10:17:01.3500454 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 27674 | 10:17:01.3500626 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 27675 | 10:17:01.3500813 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 27676 | 10:17:01.3500992 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 27677 | 10:17:01.3501173 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 27678 | 10:17:01.3501354 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 27679 | 10:17:01.3501534 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 27680 | 10:17:01.3501715 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 27681 | 10:17:01.3501879 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 27682 | 10:17:01.3502049 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 27684 | 10:17:01.3502206 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 27686 | 10:17:01.3502549 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 27687 | 10:17:01.3502742 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 27688 | 10:17:01.3502982 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 27689 | 10:17:01.3503218 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 27690 | 10:17:01.3503423 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 27692 | 10:17:01.3503998 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 27693 | 10:17:01.3504166 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 27697 | 10:17:01.3507300 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 27701 | 10:17:01.3508592 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 27704 | 10:17:01.3510542 PM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 27710 | 10:17:01.3529254 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 27714 | 10:17:01.3529937 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 27720 | 10:17:01.3535075 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:17:01.3510542 PM', 'Process Name': 'win-confine.exe', 'PID': '2184', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 27704}`

## lpac-context-control (PID 2708)
Exit `0xc0000142`; 41 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 28154 | 10:17:01.3685087 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 28155 | 10:17:01.3685314 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 28156 | 10:17:01.3686182 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 28157 | 10:17:01.3686380 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 28158 | 10:17:01.3686713 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 28159 | 10:17:01.3687102 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 28160 | 10:17:01.3687279 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 28161 | 10:17:01.3687534 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 28163 | 10:17:01.3689302 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 28186 | 10:17:01.3713863 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 28193 | 10:17:01.3714954 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 28199 | 10:17:01.3719998 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:17:01.3687534 PM', 'Process Name': 'win-confine.exe', 'PID': '2708', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 28161}`
