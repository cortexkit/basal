# Process Monitor child startup windows

## chrome-default-dacl-control (PID 2588)
Exit `0xc0000142`; 71 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 22841 | 10:27:10.9852976 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 22851 | 10:27:10.9854752 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 22853 | 10:27:10.9855019 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 22857 | 10:27:10.9855556 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 22858 | 10:27:10.9855651 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 22859 | 10:27:10.9855734 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 22860 | 10:27:10.9855821 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 22861 | 10:27:10.9855907 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 22862 | 10:27:10.9855990 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 22863 | 10:27:10.9856070 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 22864 | 10:27:10.9856147 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 22865 | 10:27:10.9856227 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 22866 | 10:27:10.9856303 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 22867 | 10:27:10.9856378 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 22868 | 10:27:10.9856455 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 22869 | 10:27:10.9856530 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 22870 | 10:27:10.9856609 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 22871 | 10:27:10.9856686 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 22873 | 10:27:10.9856863 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 22874 | 10:27:10.9856951 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 22875 | 10:27:10.9857069 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 22876 | 10:27:10.9857174 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 22877 | 10:27:10.9857284 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 22879 | 10:27:10.9858614 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 22880 | 10:27:10.9859565 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 22882 | 10:27:10.9859886 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 22888 | 10:27:10.9862081 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 22901 | 10:27:10.9878930 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 22905 | 10:27:10.9879396 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 22911 | 10:27:10.9882634 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## post-load-primary-control (PID 7940)
Exit `0xc0000142`; 38 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 23696 | 10:27:11.0102091 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 23697 | 10:27:11.0102234 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 23702 | 10:27:11.0103765 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 23703 | 10:27:11.0103942 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 23704 | 10:27:11.0104268 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 23705 | 10:27:11.0104570 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 23706 | 10:27:11.0104696 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 23707 | 10:27:11.0106808 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 23709 | 10:27:11.0107015 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 23712 | 10:27:11.0110628 PM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 23718 | 10:27:11.0120689 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 23722 | 10:27:11.0121154 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 23728 | 10:27:11.0124199 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.0110628 PM', 'Process Name': 'win-confine.exe', 'PID': '7940', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\\runneradmin', 'capture_order': 23712}`

## chrome-context-control (PID 3292)
Exit `0xc0000142`; 65 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 24150 | 10:27:11.0261551 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 24159 | 10:27:11.0263363 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 24161 | 10:27:11.0263594 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 24165 | 10:27:11.0264001 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 24166 | 10:27:11.0264071 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 24167 | 10:27:11.0264123 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 24168 | 10:27:11.0264173 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 24169 | 10:27:11.0264221 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 24170 | 10:27:11.0264267 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 24171 | 10:27:11.0264317 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 24172 | 10:27:11.0264364 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 24173 | 10:27:11.0264415 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 24174 | 10:27:11.0264471 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 24175 | 10:27:11.0264520 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 24176 | 10:27:11.0264568 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 24177 | 10:27:11.0264614 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 24178 | 10:27:11.0264661 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 24179 | 10:27:11.0264710 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 24181 | 10:27:11.0264826 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 24182 | 10:27:11.0264884 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 24183 | 10:27:11.0264964 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 24184 | 10:27:11.0265033 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 24185 | 10:27:11.0265124 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 24186 | 10:27:11.0266208 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 24187 | 10:27:11.0266839 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 24189 | 10:27:11.0268504 PM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 24252 | 10:27:11.0292253 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 24256 | 10:27:11.0292776 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 24263 | 10:27:11.0297157 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.0268504 PM', 'Process Name': 'win-confine.exe', 'PID': '3292', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\\runneradmin', 'capture_order': 24189}`

## lpac-context-control (PID 7744)
Exit `0xc0000142`; 38 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 24593 | 10:27:11.0401290 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 24596 | 10:27:11.0402058 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 24598 | 10:27:11.0402994 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 24600 | 10:27:11.0403154 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 24602 | 10:27:11.0403441 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 24604 | 10:27:11.0403991 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 24606 | 10:27:11.0404091 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 24629 | 10:27:11.0415548 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 24633 | 10:27:11.0416005 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 24647 | 10:27:11.0419366 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.0403441 PM', 'Process Name': 'win-confine.exe', 'PID': '7744', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 24602}`

## post-load-cwd-grant-control (PID 4364)
Exit `0xc0000142`; 40 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 32962 | 10:27:11.2357721 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 32963 | 10:27:11.2357870 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 32966 | 10:27:11.2358685 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 32967 | 10:27:11.2358841 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 32969 | 10:27:11.2359140 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 32970 | 10:27:11.2359422 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 32971 | 10:27:11.2359544 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 32976 | 10:27:11.2362115 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 32977 | 10:27:11.2362224 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 33001 | 10:27:11.2376025 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 33005 | 10:27:11.2376515 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 33016 | 10:27:11.2379487 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.2362224 PM', 'Process Name': 'win-confine.exe', 'PID': '4364', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 32977}`

## post-load-detached-control (PID 5996)
Exit `0x00000000`; 446 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 33244 | 10:27:11.2431429 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 33245 | 10:27:11.2431583 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 33248 | 10:27:11.2432725 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 33249 | 10:27:11.2432859 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 33250 | 10:27:11.2433123 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 33251 | 10:27:11.2433390 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 33252 | 10:27:11.2433489 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 33267 | 10:27:11.2435304 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 33269 | 10:27:11.2435446 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 33286 | 10:27:11.2438735 PM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 33333 | 10:27:11.2451445 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 33335 | 10:27:11.2451576 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 33337 | 10:27:11.2452085 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 33339 | 10:27:11.2454166 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 33340 | 10:27:11.2454313 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 33341 | 10:27:11.2454480 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 33342 | 10:27:11.2454594 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 33344 | 10:27:11.2454726 PM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 33346 | 10:27:11.2455092 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 33349 | 10:27:11.2455368 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 33350 | 10:27:11.2455490 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 33381 | 10:27:11.2473997 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 33382 | 10:27:11.2474191 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 33383 | 10:27:11.2474882 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 33384 | 10:27:11.2475034 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 33385 | 10:27:11.2479420 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 33386 | 10:27:11.2479857 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 33465 | 10:27:11.2502678 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 43,284 |
| 33466 | 10:27:11.2502781 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 43,284, Priority: Low |
| 34434 | 10:27:11.2790702 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,023 |
| 34777 | 10:27:11.2848857 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.2479857 PM', 'Process Name': 'win-confine.exe', 'PID': '5996', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 33386}`

## chrome-detached-control (PID 1896)
Exit `0x00000000`; 506 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 35467 | 10:27:11.3153712 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 35475 | 10:27:11.3155830 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 35478 | 10:27:11.3156177 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 35483 | 10:27:11.3156810 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 35484 | 10:27:11.3156914 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 35485 | 10:27:11.3157037 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 35487 | 10:27:11.3157127 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 35488 | 10:27:11.3157218 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 35490 | 10:27:11.3157300 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 35491 | 10:27:11.3157386 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 35493 | 10:27:11.3157463 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 35494 | 10:27:11.3157557 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 35496 | 10:27:11.3157646 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 35497 | 10:27:11.3157739 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 35498 | 10:27:11.3157833 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 35499 | 10:27:11.3157916 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 35500 | 10:27:11.3158002 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 35501 | 10:27:11.3158090 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 35503 | 10:27:11.3158292 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 35505 | 10:27:11.3158394 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 35506 | 10:27:11.3158538 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 35507 | 10:27:11.3158683 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 35509 | 10:27:11.3158812 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 35517 | 10:27:11.3160439 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 35521 | 10:27:11.3161673 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 35525 | 10:27:11.3161989 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 35531 | 10:27:11.3164185 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 35589 | 10:27:11.3186327 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 35602 | 10:27:11.3188528 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 35603 | 10:27:11.3188702 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 35608 | 10:27:11.3189352 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 35610 | 10:27:11.3190250 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 35611 | 10:27:11.3190365 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 35643 | 10:27:11.3197293 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 35645 | 10:27:11.3197466 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 35646 | 10:27:11.3197669 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 35648 | 10:27:11.3197798 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 35650 | 10:27:11.3198186 PM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 35654 | 10:27:11.3198493 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 35657 | 10:27:11.3198969 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 35662 | 10:27:11.3199775 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 35664 | 10:27:11.3200143 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 35760 | 10:27:11.3235401 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 35761 | 10:27:11.3236986 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 35764 | 10:27:11.3237255 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 35766 | 10:27:11.3237680 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 35771 | 10:27:11.3239029 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\f3a71a4b-6118-4257-8ccb-39a33ba059d4 | NAME NOT FOUND | Length: 528 |
| 35776 | 10:27:11.3240111 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 35777 | 10:27:11.3240686 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 35779 | 10:27:11.3241510 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 35782 | 10:27:11.3242348 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 35785 | 10:27:11.3242592 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 35788 | 10:27:11.3243318 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\DelayExecutionThreshold | NAME NOT FOUND | Length: 80 |
| 35842 | 10:27:11.3258879 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 43,284 |
| 35843 | 10:27:11.3258975 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 43,284, Priority: Low |
| 36032 | 10:27:11.3341988 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 36823 | 10:27:11.3461396 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,022 |
| 37358 | 10:27:11.3554658 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## lpac-context-detached-control (PID 5620)
Exit `0x00000000`; 447 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 38087 | 10:27:11.3850415 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 38088 | 10:27:11.3850593 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 38090 | 10:27:11.3851514 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 38092 | 10:27:11.3851674 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 38094 | 10:27:11.3851970 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38097 | 10:27:11.3852293 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 38098 | 10:27:11.3852430 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 38118 | 10:27:11.3866310 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 38119 | 10:27:11.3866426 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 38120 | 10:27:11.3866941 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38121 | 10:27:11.3868276 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 38122 | 10:27:11.3868355 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 38123 | 10:27:11.3868450 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 38124 | 10:27:11.3868510 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 38125 | 10:27:11.3868591 PM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 38126 | 10:27:11.3868799 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 38127 | 10:27:11.3868989 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 38128 | 10:27:11.3869069 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 38162 | 10:27:11.3884260 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 38163 | 10:27:11.3884445 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38179 | 10:27:11.3889338 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 38181 | 10:27:11.3889534 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 38184 | 10:27:11.3890507 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 38185 | 10:27:11.3890688 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 38197 | 10:27:11.3894494 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 38198 | 10:27:11.3894650 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 38219 | 10:27:11.3902709 PM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 38268 | 10:27:11.3915322 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 43,284 |
| 38269 | 10:27:11.3915435 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 43,284, Priority: Low |
| 39374 | 10:27:11.4104610 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,023 |
| 39813 | 10:27:11.4184095 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.3894650 PM', 'Process Name': 'win-confine.exe', 'PID': '5620', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 38198}`

## chrome-context-detached-control (PID 3768)
Exit `0x00000000`; 503 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 40588 | 10:27:11.4536296 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 40597 | 10:27:11.4537841 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 40599 | 10:27:11.4538137 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 40603 | 10:27:11.4538677 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 40604 | 10:27:11.4538769 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 40605 | 10:27:11.4538851 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 40606 | 10:27:11.4538929 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 40607 | 10:27:11.4539004 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 40608 | 10:27:11.4539079 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 40609 | 10:27:11.4539154 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 40610 | 10:27:11.4539227 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 40611 | 10:27:11.4539306 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 40612 | 10:27:11.4539877 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 40613 | 10:27:11.4539975 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 40614 | 10:27:11.4540069 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 40615 | 10:27:11.4540154 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 40616 | 10:27:11.4540979 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 40618 | 10:27:11.4541098 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 40621 | 10:27:11.4541650 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 40622 | 10:27:11.4541769 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 40623 | 10:27:11.4541919 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 40624 | 10:27:11.4542073 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 40625 | 10:27:11.4542217 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 40631 | 10:27:11.4544531 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 40639 | 10:27:11.4545467 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 40643 | 10:27:11.4546862 PM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 40701 | 10:27:11.4568941 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 40704 | 10:27:11.4570542 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 40705 | 10:27:11.4570662 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 40709 | 10:27:11.4571114 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 40711 | 10:27:11.4571681 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 40712 | 10:27:11.4571749 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 40720 | 10:27:11.4575459 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 40721 | 10:27:11.4575539 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 40722 | 10:27:11.4575658 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 40723 | 10:27:11.4575721 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 40728 | 10:27:11.4576412 PM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 40731 | 10:27:11.4576680 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 40734 | 10:27:11.4577024 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 40739 | 10:27:11.4577612 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 40741 | 10:27:11.4577836 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 40804 | 10:27:11.4599717 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 40807 | 10:27:11.4600231 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 40848 | 10:27:11.4614185 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 40856 | 10:27:11.4616386 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 40867 | 10:27:11.4617711 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 40869 | 10:27:11.4617957 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 40872 | 10:27:11.4618849 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\f3a71a4b-6118-4257-8ccb-39a33ba059d4 | NAME NOT FOUND | Length: 528 |
| 40876 | 10:27:11.4619971 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 40878 | 10:27:11.4620593 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 40880 | 10:27:11.4621396 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 40882 | 10:27:11.4621698 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 40884 | 10:27:11.4621919 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 40888 | 10:27:11.4622096 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\DelayExecutionThreshold | NAME NOT FOUND | Length: 80 |
| 40953 | 10:27:11.4640879 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 43,284 |
| 40954 | 10:27:11.4640992 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 43,284, Priority: Low |
| 41155 | 10:27:11.4710333 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 41999 | 10:27:11.4841818 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,017 |
| 42360 | 10:27:11.4905290 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:11.4546862 PM', 'Process Name': 'win-confine.exe', 'PID': '3768', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\\runneradmin', 'capture_order': 40643}`
