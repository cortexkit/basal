# Process Monitor child startup windows

## chrome-default-dacl-control (PID 5052)
Exit `0xc0000142`; 71 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 12861 | 7:03:29.9100087 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 12872 | 7:03:29.9100916 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 12874 | 7:03:29.9101067 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 12880 | 7:03:29.9101396 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 12882 | 7:03:29.9101464 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 12884 | 7:03:29.9101525 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 12885 | 7:03:29.9101591 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 12887 | 7:03:29.9101649 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 12888 | 7:03:29.9101705 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 12889 | 7:03:29.9101758 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 12890 | 7:03:29.9101809 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 12891 | 7:03:29.9101861 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 12892 | 7:03:29.9101914 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 12893 | 7:03:29.9101972 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 12896 | 7:03:29.9102297 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 12897 | 7:03:29.9102364 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 12899 | 7:03:29.9102429 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 12901 | 7:03:29.9102484 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 12906 | 7:03:29.9102606 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 12908 | 7:03:29.9102673 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 12910 | 7:03:29.9102779 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 12911 | 7:03:29.9102973 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 12912 | 7:03:29.9103068 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 12928 | 7:03:29.9104027 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 12936 | 7:03:29.9104830 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 12938 | 7:03:29.9104984 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 12950 | 7:03:29.9106291 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 13029 | 7:03:29.9118654 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 13035 | 7:03:29.9118924 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 13060 | 7:03:29.9121336 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## post-load-primary-control (PID 4712)
Exit `0xc0000142`; 38 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 14105 | 7:03:29.9252871 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 14106 | 7:03:29.9252976 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 14111 | 7:03:29.9253586 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 14113 | 7:03:29.9253655 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 14115 | 7:03:29.9253787 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 14118 | 7:03:29.9253941 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 14119 | 7:03:29.9254012 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 14123 | 7:03:29.9255186 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 14125 | 7:03:29.9255251 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 14138 | 7:03:29.9256726 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 14184 | 7:03:29.9263095 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 14189 | 7:03:29.9263364 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 14215 | 7:03:29.9265461 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:29.9256726 AM', 'Process Name': 'win-confine.exe', 'PID': '4712', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\\runneradmin', 'capture_order': 14138}`

## chrome-context-control (PID 6332)
Exit `0xc0000142`; 65 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 14612 | 7:03:29.9327331 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 14619 | 7:03:29.9328033 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 14621 | 7:03:29.9328151 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 14625 | 7:03:29.9328368 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 14626 | 7:03:29.9328414 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 14627 | 7:03:29.9328452 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 14628 | 7:03:29.9328497 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 14629 | 7:03:29.9328535 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 14630 | 7:03:29.9328576 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 14631 | 7:03:29.9328612 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 14632 | 7:03:29.9328650 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 14633 | 7:03:29.9328688 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 14634 | 7:03:29.9328724 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 14635 | 7:03:29.9328760 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 14636 | 7:03:29.9328797 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 14637 | 7:03:29.9328835 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 14638 | 7:03:29.9328872 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 14639 | 7:03:29.9328908 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 14641 | 7:03:29.9328990 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 14642 | 7:03:29.9329032 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 14643 | 7:03:29.9329084 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 14644 | 7:03:29.9329129 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 14645 | 7:03:29.9329186 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 14647 | 7:03:29.9329907 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 14650 | 7:03:29.9330419 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 14651 | 7:03:29.9331205 AM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 14659 | 7:03:29.9341466 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 14663 | 7:03:29.9341725 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 14669 | 7:03:29.9343772 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:29.9331205 AM', 'Process Name': 'win-confine.exe', 'PID': '6332', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\\runneradmin', 'capture_order': 14651}`

## lpac-context-control (PID 7992)
Exit `0xc0000142`; 38 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 14980 | 7:03:29.9386047 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 14982 | 7:03:29.9386143 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 14993 | 7:03:29.9387046 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 14995 | 7:03:29.9387150 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 14996 | 7:03:29.9387342 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 14997 | 7:03:29.9387490 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 14998 | 7:03:29.9387553 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 15036 | 7:03:29.9394907 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 15040 | 7:03:29.9395133 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 15046 | 7:03:29.9396893 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:29.9387342 AM', 'Process Name': 'win-confine.exe', 'PID': '7992', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 14996}`

## post-load-cwd-grant-control (PID 2304)
Exit `0xc0000142`; 40 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 24488 | 7:03:30.0222521 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 24495 | 7:03:30.0223554 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 24515 | 7:03:30.0225653 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 24522 | 7:03:30.0226453 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 24534 | 7:03:30.0227592 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 24544 | 7:03:30.0228757 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 24556 | 7:03:30.0229726 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 24572 | 7:03:30.0232501 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 24578 | 7:03:30.0233705 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 24696 | 7:03:30.0254000 AM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 24704 | 7:03:30.0254708 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 26782 | 7:03:30.0431559 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.0233705 AM', 'Process Name': 'win-confine.exe', 'PID': '2304', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 24578}`

## post-load-detached-control (PID 2448)
Exit `0x00000000`; 437 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 27124 | 7:03:30.0474555 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 27126 | 7:03:30.0474666 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 27131 | 7:03:30.0475217 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 27132 | 7:03:30.0475292 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 27135 | 7:03:30.0475445 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 27137 | 7:03:30.0475606 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 27138 | 7:03:30.0475678 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 27155 | 7:03:30.0477617 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 27158 | 7:03:30.0477712 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 27181 | 7:03:30.0480447 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 27588 | 7:03:30.0511272 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 27590 | 7:03:30.0511364 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 27633 | 7:03:30.0513668 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 27634 | 7:03:30.0515193 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 27635 | 7:03:30.0515290 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 27636 | 7:03:30.0515411 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 27637 | 7:03:30.0515489 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 27638 | 7:03:30.0516073 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 27674 | 7:03:30.0519065 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 27710 | 7:03:30.0522294 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 27711 | 7:03:30.0522401 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 30517 | 7:03:30.0744739 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 30518 | 7:03:30.0744857 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 30527 | 7:03:30.0746750 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 30528 | 7:03:30.0746837 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 30545 | 7:03:30.0751292 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 30546 | 7:03:30.0751375 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 30645 | 7:03:30.0768292 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 48,679 |
| 30646 | 7:03:30.0768338 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 48,679, Priority: Low |
| 31093 | 7:03:30.0942489 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 31611 | 7:03:30.0972264 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 31758 | 7:03:30.0981320 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,019 |
| 32025 | 7:03:30.1021885 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.0751375 AM', 'Process Name': 'win-confine.exe', 'PID': '2448', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 30546}`

## chrome-detached-control (PID 1224)
Exit `0x00000000`; 495 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 32540 | 7:03:30.1249310 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 32547 | 7:03:30.1250367 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 32551 | 7:03:30.1250539 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 32555 | 7:03:30.1250887 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 32556 | 7:03:30.1250952 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 32557 | 7:03:30.1251002 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 32558 | 7:03:30.1251052 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 32559 | 7:03:30.1251101 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 32560 | 7:03:30.1251148 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 32561 | 7:03:30.1251364 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 32562 | 7:03:30.1251486 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 32564 | 7:03:30.1251653 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 32565 | 7:03:30.1252043 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 32566 | 7:03:30.1252200 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 32567 | 7:03:30.1252371 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 32569 | 7:03:30.1252425 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 32570 | 7:03:30.1252474 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 32571 | 7:03:30.1252527 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 32573 | 7:03:30.1252667 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 32574 | 7:03:30.1252739 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 32576 | 7:03:30.1252837 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 32578 | 7:03:30.1252924 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 32581 | 7:03:30.1253021 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 32590 | 7:03:30.1254324 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 32594 | 7:03:30.1255195 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 32597 | 7:03:30.1255418 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 32612 | 7:03:30.1257429 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 32660 | 7:03:30.1271352 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 32661 | 7:03:30.1272655 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 32662 | 7:03:30.1272733 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 32663 | 7:03:30.1273087 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 32665 | 7:03:30.1273450 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 32666 | 7:03:30.1273498 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 32668 | 7:03:30.1276115 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 32669 | 7:03:30.1276163 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 32670 | 7:03:30.1276229 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 32671 | 7:03:30.1276272 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 32673 | 7:03:30.1276463 AM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 32675 | 7:03:30.1276626 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 32676 | 7:03:30.1276824 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 32680 | 7:03:30.1277282 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 32683 | 7:03:30.1277424 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 32765 | 7:03:30.1297515 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 32776 | 7:03:30.1299176 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 32779 | 7:03:30.1299403 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 32784 | 7:03:30.1299811 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 32796 | 7:03:30.1301148 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\f3a71a4b-6118-4257-8ccb-39a33ba059d4 | NAME NOT FOUND | Length: 528 |
| 32803 | 7:03:30.1302268 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 32807 | 7:03:30.1302923 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 32812 | 7:03:30.1303766 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 32813 | 7:03:30.1304032 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 32814 | 7:03:30.1304234 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 32816 | 7:03:30.1304409 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\DelayExecutionThreshold | NAME NOT FOUND | Length: 80 |
| 32895 | 7:03:30.1318337 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 48,679 |
| 32896 | 7:03:30.1318383 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 48,679, Priority: Low |
| 32916 | 7:03:30.1351235 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 33652 | 7:03:30.1425515 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,020 |
| 33819 | 7:03:30.1435006 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,011 |
| 34192 | 7:03:30.1477913 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## lpac-context-detached-control (PID 2980)
Exit `0x00000000`; 439 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 34942 | 7:03:30.1678284 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 34943 | 7:03:30.1678413 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 34948 | 7:03:30.1679331 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 34949 | 7:03:30.1679423 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 34951 | 7:03:30.1679581 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 34953 | 7:03:30.1679758 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 34954 | 7:03:30.1679843 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 35037 | 7:03:30.1704398 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 35038 | 7:03:30.1704516 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 35039 | 7:03:30.1705033 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 35040 | 7:03:30.1706414 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 35041 | 7:03:30.1706484 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 35042 | 7:03:30.1706577 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 35043 | 7:03:30.1706633 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 35044 | 7:03:30.1706715 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 35046 | 7:03:30.1706927 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 35047 | 7:03:30.1707101 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 35049 | 7:03:30.1707161 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 35100 | 7:03:30.1716695 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 35101 | 7:03:30.1716812 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 35119 | 7:03:30.1720385 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 35121 | 7:03:30.1720464 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 35124 | 7:03:30.1720958 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 35125 | 7:03:30.1721048 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 35136 | 7:03:30.1723386 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 35137 | 7:03:30.1723468 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 35176 | 7:03:30.1728614 AM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 35248 | 7:03:30.1737554 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 48,679 |
| 35249 | 7:03:30.1737611 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 48,679, Priority: Low |
| 36080 | 7:03:30.1854557 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 960 |
| 36088 | 7:03:30.1855311 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 36531 | 7:03:30.1906549 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.1723468 AM', 'Process Name': 'win-confine.exe', 'PID': '2980', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 35137}`

## chrome-context-detached-control (PID 7904)
Exit `0x00000000`; 491 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 37244 | 7:03:30.2129065 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 37250 | 7:03:30.2130265 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 37253 | 7:03:30.2130633 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 37259 | 7:03:30.2131066 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 37260 | 7:03:30.2131135 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 37261 | 7:03:30.2131185 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 37262 | 7:03:30.2131457 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 37263 | 7:03:30.2131507 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 37264 | 7:03:30.2131607 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 37265 | 7:03:30.2131655 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 37266 | 7:03:30.2131730 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 37267 | 7:03:30.2131834 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 37268 | 7:03:30.2131912 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 37269 | 7:03:30.2131962 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 37270 | 7:03:30.2132011 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 37271 | 7:03:30.2132060 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 37272 | 7:03:30.2132115 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 37274 | 7:03:30.2132164 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 37277 | 7:03:30.2132471 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 37279 | 7:03:30.2132548 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 37280 | 7:03:30.2132654 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 37282 | 7:03:30.2132750 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 37283 | 7:03:30.2132845 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 37285 | 7:03:30.2134162 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\57a6ed1a-79f7-5011-b242-4784e5620cf7 | NAME NOT FOUND | Length: 528 |
| 37288 | 7:03:30.2134860 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 37289 | 7:03:30.2136039 AM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 37306 | 7:03:30.2147899 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 37313 | 7:03:30.2149143 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 37316 | 7:03:30.2149293 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 37319 | 7:03:30.2149728 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 37322 | 7:03:30.2150234 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 37324 | 7:03:30.2150312 AM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 37352 | 7:03:30.2154571 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 37354 | 7:03:30.2154654 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 37355 | 7:03:30.2154746 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 37356 | 7:03:30.2154790 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 37361 | 7:03:30.2155004 AM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 37363 | 7:03:30.2155134 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 37364 | 7:03:30.2155315 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 37369 | 7:03:30.2155610 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 37371 | 7:03:30.2155708 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 37427 | 7:03:30.2167944 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 37429 | 7:03:30.2168225 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 37459 | 7:03:30.2177312 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 37468 | 7:03:30.2178691 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 37482 | 7:03:30.2179557 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 37484 | 7:03:30.2179684 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\ResourcePolicies | NAME NOT FOUND | Length: 24 |
| 37488 | 7:03:30.2180510 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\f3a71a4b-6118-4257-8ccb-39a33ba059d4 | NAME NOT FOUND | Length: 528 |
| 37492 | 7:03:30.2181476 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 37495 | 7:03:30.2182012 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 37503 | 7:03:30.2182780 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 37506 | 7:03:30.2183034 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 37509 | 7:03:30.2183222 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 37513 | 7:03:30.2183395 AM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\DelayExecutionThreshold | NAME NOT FOUND | Length: 80 |
| 37585 | 7:03:30.2194805 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 48,679 |
| 37587 | 7:03:30.2195151 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 48,679, Priority: Low |
| 37669 | 7:03:30.2228661 AM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 38827 | 7:03:30.2365263 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.2136039 AM', 'Process Name': 'win-confine.exe', 'PID': '7904', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\\runneradmin', 'capture_order': 37289}`

## full-detached-control (PID 6940)
Exit `0x00000000`; 433 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 39220 | 7:03:30.2524261 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 39221 | 7:03:30.2524367 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 39222 | 7:03:30.2524892 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 39223 | 7:03:30.2524941 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 39224 | 7:03:30.2525059 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 39225 | 7:03:30.2525246 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 39226 | 7:03:30.2525308 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 39231 | 7:03:30.2526920 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 39233 | 7:03:30.2527005 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 39241 | 7:03:30.2528972 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 39271 | 7:03:30.2536532 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 39272 | 7:03:30.2536618 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 39274 | 7:03:30.2536999 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 39282 | 7:03:30.2538303 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 39283 | 7:03:30.2538372 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 39284 | 7:03:30.2538463 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 39285 | 7:03:30.2538518 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 39286 | 7:03:30.2538606 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 39289 | 7:03:30.2538838 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 39290 | 7:03:30.2539020 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 39292 | 7:03:30.2539082 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 39367 | 7:03:30.2552982 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 39368 | 7:03:30.2553099 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 39375 | 7:03:30.2553533 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 39376 | 7:03:30.2553606 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 39390 | 7:03:30.2556178 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 39391 | 7:03:30.2556250 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 39426 | 7:03:30.2567897 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 48,679 |
| 39427 | 7:03:30.2567947 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 48,679, Priority: Low |
| 40697 | 7:03:30.2719573 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.2556250 AM', 'Process Name': 'win-confine.exe', 'PID': '6940', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 39391}`

## chrome-untrusted-detached-control (PID 1664)
Exit `0xc00000a5`; 25 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 41338 | 7:03:30.2891126 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 41340 | 7:03:30.2891237 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 41344 | 7:03:30.2891708 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 41345 | 7:03:30.2891777 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 41346 | 7:03:30.2891862 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 41347 | 7:03:30.2891972 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 41348 | 7:03:30.2892018 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 41349 | 7:03:30.2893079 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 41350 | 7:03:30.2893147 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 41365 | 7:03:30.2896069 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.2893147 AM', 'Process Name': 'win-confine.exe', 'PID': '1664', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 41350}`

## full-gui-control (PID 4836)
Exit `0x00000000`; 523 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 41597 | 7:03:30.2944917 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 41598 | 7:03:30.2945017 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 41603 | 7:03:30.2945508 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 41604 | 7:03:30.2945573 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 41605 | 7:03:30.2945715 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 41606 | 7:03:30.2945776 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 41607 | 7:03:30.2946990 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 41608 | 7:03:30.2947061 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 41612 | 7:03:30.2948914 AM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmtn0fv\runneradmin |
| 41617 | 7:03:30.2956063 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 41618 | 7:03:30.2956143 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 41619 | 7:03:30.2957398 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 41620 | 7:03:30.2957442 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 41621 | 7:03:30.2957512 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 41622 | 7:03:30.2957552 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 41623 | 7:03:30.2957612 AM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 41624 | 7:03:30.2957807 AM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 41625 | 7:03:30.2957934 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 41626 | 7:03:30.2957985 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 41627 | 7:03:30.2958644 AM | QueryOpen | C:\Windows\System32\apphelp.dll | FAST IO DISALLOWED |  |
| 41633 | 7:03:30.2959734 AM | CreateFileMapping | C:\Windows\System32\apphelp.dll | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 41641 | 7:03:30.2962101 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags | ACCESS DENIED | Desired Access: Query Value |
| 41642 | 7:03:30.2962250 AM | RegOpenKey | HKLM\OSDATA\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags | NAME NOT FOUND | Desired Access: Query Value |
| 41643 | 7:03:30.2962734 AM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags | ACCESS DENIED | Desired Access: Query Value |
| 41645 | 7:03:30.2963496 AM | QuerySecurityFile | D:\a\basal\basal\spikes\win-confine\target\release\placed\win-confine-gui.exe | BUFFER OVERFLOW | Information: Owner |
| 41650 | 7:03:30.2964155 AM | QuerySecurityFile | C:\Windows\System32\ntdll.dll | BUFFER OVERFLOW | Information: Owner |
| 41655 | 7:03:30.2964730 AM | QuerySecurityFile | C:\Windows\System32\kernel32.dll | BUFFER OVERFLOW | Information: Owner |
| 41660 | 7:03:30.2965219 AM | QuerySecurityFile | C:\Windows\System32\KernelBase.dll | BUFFER OVERFLOW | Information: Owner |
| 41667 | 7:03:30.2965911 AM | CreateFileMapping | C:\Windows\apppatch\sysmain.sdb | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_READONLY |
| 41673 | 7:03:30.2967055 AM | RegOpenKey | HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders | ACCESS DENIED | Desired Access: Query Value |
| 41683 | 7:03:30.2968329 AM | RegOpenKey | HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders | ACCESS DENIED | Desired Access: Query Value |
| 41797 | 7:03:30.2984775 AM | QuerySecurityFile | C:\Windows\System32\rpcrt4.dll | BUFFER OVERFLOW | Information: Owner |
| 41806 | 7:03:30.2986167 AM | QuerySecurityFile | C:\Windows\System32\ws2_32.dll | BUFFER OVERFLOW | Information: Owner |
| 41822 | 7:03:30.2987683 AM | QuerySecurityFile | C:\Windows\System32\msvcrt.dll | BUFFER OVERFLOW | Information: Owner |
| 41833 | 7:03:30.2988654 AM | QuerySecurityFile | C:\Windows\System32\bcrypt.dll | BUFFER OVERFLOW | Information: Owner |
| 41841 | 7:03:30.2990280 AM | QuerySecurityFile | C:\Windows\System32\sechost.dll | BUFFER OVERFLOW | Information: Owner |
| 41858 | 7:03:30.2992368 AM | QuerySecurityFile | C:\Windows\System32\advapi32.dll | BUFFER OVERFLOW | Information: Owner |
| 41890 | 7:03:30.2995038 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 41891 | 7:03:30.2995172 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 41893 | 7:03:30.2995629 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 41894 | 7:03:30.2995706 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 41916 | 7:03:30.2998374 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 41917 | 7:03:30.2998455 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 42015 | 7:03:30.3014113 AM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 48,679 |
| 42017 | 7:03:30.3014161 AM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 48,679, Priority: Low |
| 42280 | 7:03:30.3102818 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,022 |
| 42902 | 7:03:30.3145078 AM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 956 |
| 43151 | 7:03:30.3191320 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine-gui.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.2998455 AM', 'Process Name': 'win-confine-gui.exe', 'PID': '4836', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 41917}`

## chrome-untrusted-gui-control (PID 7284)
Exit `0xc00000a5`; 24 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 43371 | 7:03:30.3381751 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 43373 | 7:03:30.3381861 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 43376 | 7:03:30.3382335 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 43377 | 7:03:30.3382416 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 43378 | 7:03:30.3382528 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 43379 | 7:03:30.3382588 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 43385 | 7:03:30.3383798 AM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value, Enumerate Sub Keys |
| 43386 | 7:03:30.3383880 AM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 43400 | 7:03:30.3387285 AM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1000861503-3182281758-1041194900-500\\Device\HarddiskVolume6\a\basal\basal\spikes\win-confine\target\release\placed\win-confine-gui.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '7:03:30.3383880 AM', 'Process Name': 'win-confine-gui.exe', 'PID': '7284', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 43386}`
