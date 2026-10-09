# Process Monitor child startup windows

## chrome-default-dacl-control (PID 5816)
Exit `0xc0000142`; 74 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 28564 | 10:27:19.7340375 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 28569 | 10:27:19.7342227 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 28571 | 10:27:19.7342719 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 28576 | 10:27:19.7344810 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 28577 | 10:27:19.7344995 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 28579 | 10:27:19.7345156 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 28580 | 10:27:19.7345300 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 28581 | 10:27:19.7345448 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 28582 | 10:27:19.7345604 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 28583 | 10:27:19.7345750 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 28584 | 10:27:19.7345893 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 28585 | 10:27:19.7346037 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 28586 | 10:27:19.7346181 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 28587 | 10:27:19.7346331 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 28588 | 10:27:19.7346485 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 28590 | 10:27:19.7346638 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 28591 | 10:27:19.7346785 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 28592 | 10:27:19.7346930 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 28593 | 10:27:19.7347070 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 28594 | 10:27:19.7347211 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 28597 | 10:27:19.7347527 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 28598 | 10:27:19.7347701 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 28600 | 10:27:19.7348036 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 28602 | 10:27:19.7348241 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 28604 | 10:27:19.7348431 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 28606 | 10:27:19.7348932 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 28607 | 10:27:19.7349082 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 28617 | 10:27:19.7351308 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 28625 | 10:27:19.7354447 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 28691 | 10:27:19.7385216 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 28699 | 10:27:19.7386299 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 28712 | 10:27:19.7393801 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## post-load-primary-control (PID 8932)
Exit `0xc0000142`; 39 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 30467 | 10:27:19.7880493 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 30469 | 10:27:19.7880727 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 30482 | 10:27:19.7881912 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 30485 | 10:27:19.7882106 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 30487 | 10:27:19.7882433 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 30490 | 10:27:19.7882785 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 30492 | 10:27:19.7882970 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 30495 | 10:27:19.7883332 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 30516 | 10:27:19.7885492 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 30587 | 10:27:19.7895913 PM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 30756 | 10:27:19.7918506 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 30766 | 10:27:19.7919547 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 30841 | 10:27:19.7930171 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:19.7895913 PM', 'Process Name': 'win-confine.exe', 'PID': '8932', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 30587}`

## chrome-context-control (PID 8184)
Exit `0xc0000142`; 73 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 31885 | 10:27:19.8208720 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 31894 | 10:27:19.8210665 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 31896 | 10:27:19.8211087 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 31901 | 10:27:19.8211963 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 31904 | 10:27:19.8212507 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 31905 | 10:27:19.8212664 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 31907 | 10:27:19.8212814 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 31909 | 10:27:19.8212976 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 31911 | 10:27:19.8213136 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 31912 | 10:27:19.8213292 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 31914 | 10:27:19.8213444 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 31915 | 10:27:19.8213594 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 31916 | 10:27:19.8213741 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 31917 | 10:27:19.8213883 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 31919 | 10:27:19.8214029 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 31920 | 10:27:19.8214177 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 31921 | 10:27:19.8214324 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 31922 | 10:27:19.8214475 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 31923 | 10:27:19.8214621 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 31924 | 10:27:19.8214766 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 31926 | 10:27:19.8215077 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 31927 | 10:27:19.8215260 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 31928 | 10:27:19.8215466 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 31929 | 10:27:19.8215658 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 31930 | 10:27:19.8215858 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 31932 | 10:27:19.8217029 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 31933 | 10:27:19.8217187 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 31935 | 10:27:19.8219429 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 31938 | 10:27:19.8220556 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 31940 | 10:27:19.8222431 PM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 32004 | 10:27:19.8254035 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 32011 | 10:27:19.8255204 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 32030 | 10:27:19.8262791 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:19.8222431 PM', 'Process Name': 'win-confine.exe', 'PID': '8184', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 31940}`

## lpac-context-control (PID 9164)
Exit `0xc0000142`; 41 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 32467 | 10:27:19.8427591 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 32468 | 10:27:19.8427847 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 32469 | 10:27:19.8428773 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 32470 | 10:27:19.8428964 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 32471 | 10:27:19.8429285 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 32472 | 10:27:19.8429646 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 32473 | 10:27:19.8429836 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 32474 | 10:27:19.8430190 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 32475 | 10:27:19.8432098 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 32496 | 10:27:19.8450925 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 32503 | 10:27:19.8451968 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 32522 | 10:27:19.8457718 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:19.8430190 PM', 'Process Name': 'win-confine.exe', 'PID': '9164', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 32474}`

## post-load-cwd-grant-control (PID 4012)
Exit `0xc0000142`; 41 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 40208 | 10:27:20.0604193 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 40209 | 10:27:20.0604448 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 40210 | 10:27:20.0605408 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 40211 | 10:27:20.0605593 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 40212 | 10:27:20.0605909 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 40213 | 10:27:20.0606243 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 40214 | 10:27:20.0606416 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 40215 | 10:27:20.0606753 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 40216 | 10:27:20.0608893 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 40231 | 10:27:20.0632921 PM | CreateFileMapping | C:\Windows\System32\conhost.exe | FILE LOCKED WITH ONLY READERS | SyncType: SyncTypeCreateSection, PageProtection: PAGE_EXECUTE |
| 40235 | 10:27:20.0634048 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\Conhost.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 40241 | 10:27:20.0639762 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:20.0606753 PM', 'Process Name': 'win-confine.exe', 'PID': '4012', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\Software\\Microsoft\\Windows NT\\CurrentVersion\\Image File Execution Options', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value, Enumerate Sub Keys', 'capture_order': 40215}`

## post-load-detached-control (PID 2664)
Exit `0x00000000`; 471 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 40557 | 10:27:20.0765372 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 40559 | 10:27:20.0765633 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 40562 | 10:27:20.0766544 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 40563 | 10:27:20.0766731 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 40564 | 10:27:20.0767066 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 40566 | 10:27:20.0767759 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 40567 | 10:27:20.0767948 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 40570 | 10:27:20.0768235 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 40576 | 10:27:20.0770139 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 40586 | 10:27:20.0773997 PM | CreateFile | D:\a\basal\basal\spikes\win-confine | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 40636 | 10:27:20.0798649 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 40638 | 10:27:20.0798931 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 40650 | 10:27:20.0803869 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 40658 | 10:27:20.0806677 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 40659 | 10:27:20.0806882 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 40661 | 10:27:20.0807107 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 40662 | 10:27:20.0807281 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 40663 | 10:27:20.0807499 PM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 40664 | 10:27:20.0808273 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 40665 | 10:27:20.0808620 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 40666 | 10:27:20.0808788 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 40790 | 10:27:20.0842051 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 40791 | 10:27:20.0842355 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 40792 | 10:27:20.0846040 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 40793 | 10:27:20.0846295 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 40815 | 10:27:20.0872799 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 29,047 |
| 40817 | 10:27:20.0872962 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 29,047, Priority: Low |
| 42064 | 10:27:20.1303745 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,021 |
| 42480 | 10:27:20.1392942 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:20.0846295 PM', 'Process Name': 'win-confine.exe', 'PID': '2664', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 40793}`

## chrome-detached-control (PID 8332)
Exit `0x00000000`; 525 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 43164 | 10:27:20.1786653 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 43169 | 10:27:20.1788397 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 43171 | 10:27:20.1788788 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 43175 | 10:27:20.1789585 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 43176 | 10:27:20.1789751 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 43177 | 10:27:20.1789900 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 43178 | 10:27:20.1790053 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 43179 | 10:27:20.1790200 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 43180 | 10:27:20.1790348 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 43181 | 10:27:20.1790497 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 43182 | 10:27:20.1790641 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 43183 | 10:27:20.1790789 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 43184 | 10:27:20.1790935 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 43185 | 10:27:20.1791079 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 43186 | 10:27:20.1791232 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 43187 | 10:27:20.1791382 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 43188 | 10:27:20.1791531 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 43189 | 10:27:20.1791682 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 43191 | 10:27:20.1791833 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 43192 | 10:27:20.1791966 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 43194 | 10:27:20.1792279 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 43195 | 10:27:20.1792453 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 43196 | 10:27:20.1792643 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 43197 | 10:27:20.1792828 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 43198 | 10:27:20.1793018 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 43200 | 10:27:20.1793525 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 43201 | 10:27:20.1793675 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 43205 | 10:27:20.1796943 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 43211 | 10:27:20.1800275 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 43219 | 10:27:20.1828065 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 43225 | 10:27:20.1830551 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b749553b-d950-5e03-6282-3145a61b1002 | NAME NOT FOUND | Length: 528 |
| 43228 | 10:27:20.1831150 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 43230 | 10:27:20.1831402 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 43235 | 10:27:20.1833055 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 43238 | 10:27:20.1834046 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 43240 | 10:27:20.1834241 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 43256 | 10:27:20.1843935 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 43258 | 10:27:20.1844207 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 43259 | 10:27:20.1844463 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 43260 | 10:27:20.1844653 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 43262 | 10:27:20.1845185 PM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 43265 | 10:27:20.1845676 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 43266 | 10:27:20.1846169 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 43270 | 10:27:20.1846797 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 43364 | 10:27:20.1892661 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 43368 | 10:27:20.1894923 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\5fe982db-5db8-4dc7-9814-2b3ceb277f7e | NAME NOT FOUND | Length: 528 |
| 43369 | 10:27:20.1896141 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 43370 | 10:27:20.1899122 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 43371 | 10:27:20.1899525 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 43372 | 10:27:20.1900453 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 43373 | 10:27:20.1900814 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 43374 | 10:27:20.1901119 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 43376 | 10:27:20.1901417 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySleepLoopWindowSize | NAME NOT FOUND | Length: 80 |
| 43377 | 10:27:20.1901533 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySpinCountThreshold | NAME NOT FOUND | Length: 80 |
| 43378 | 10:27:20.1901633 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayBaseYield | NAME NOT FOUND | Length: 80 |
| 43379 | 10:27:20.1901732 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtFactorYield | NAME NOT FOUND | Length: 80 |
| 43380 | 10:27:20.1901832 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayMaxYield | NAME NOT FOUND | Length: 80 |
| 43452 | 10:27:20.1928722 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 29,047 |
| 43454 | 10:27:20.1928912 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 29,047, Priority: Low |
| 43663 | 10:27:20.2030886 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 44824 | 10:27:20.2224228 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `None`

## lpac-context-detached-control (PID 8992)
Exit `0x00000000`; 477 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 45498 | 10:27:20.2583965 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 45499 | 10:27:20.2584300 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | ACCESS DENIED | Desired Access: Read |
| 45500 | 10:27:20.2585307 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 45501 | 10:27:20.2585527 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 45502 | 10:27:20.2585905 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 45503 | 10:27:20.2586254 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 45504 | 10:27:20.2586459 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 45505 | 10:27:20.2586743 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 45506 | 10:27:20.2588966 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 45520 | 10:27:20.2611072 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 45521 | 10:27:20.2611334 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 45525 | 10:27:20.2616666 PM | RegOpenKey | HKLM\Software\Microsoft\Windows NT\CurrentVersion\Image File Execution Options | ACCESS DENIED | Desired Access: Query Value, Enumerate Sub Keys |
| 45529 | 10:27:20.2619447 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 45531 | 10:27:20.2619659 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 45533 | 10:27:20.2619885 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 45534 | 10:27:20.2620061 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 45536 | 10:27:20.2620272 PM | RegOpenKey | HKLM\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | ACCESS DENIED | Desired Access: Query Value |
| 45537 | 10:27:20.2620667 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 45538 | 10:27:20.2620991 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 45539 | 10:27:20.2621168 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem | ACCESS DENIED | Desired Access: Read |
| 45554 | 10:27:20.2647606 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 45555 | 10:27:20.2647897 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | ACCESS DENIED | Desired Access: Read |
| 45556 | 10:27:20.2650839 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 45557 | 10:27:20.2651055 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | ACCESS DENIED | Desired Access: Query Value |
| 45562 | 10:27:20.2661044 PM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 45590 | 10:27:20.2676301 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 29,047 |
| 45593 | 10:27:20.2676469 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 29,047, Priority: Low |
| 45747 | 10:27:20.2793685 PM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 45748 | 10:27:20.2794893 PM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 45749 | 10:27:20.2796264 PM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 45750 | 10:27:20.2797249 PM | QueryOpen | D:\a\basal\basal\spikes\win-confine\target\release\placed\netmsg.dll | NAME NOT FOUND |  |
| 46633 | 10:27:20.2948664 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,019 |
| 46650 | 10:27:20.2950664 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 975 |
| 47027 | 10:27:20.3039568 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:20.2651055 PM', 'Process Name': 'win-confine.exe', 'PID': '8992', 'Operation': 'RegOpenKey', 'Path': 'HKLM\\System\\CurrentControlSet\\Control\\Session Manager', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Query Value', 'capture_order': 45557}`

## chrome-context-detached-control (PID 4516)
Exit `0x00000000`; 526 events; start seen: True; exit seen: True.

| Capture order | Time | Operation | Path | Result | Detail |
|---:|---|---|---|---|---|
| 47800 | 10:27:20.3523557 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\CodePage | REPARSE | Desired Access: Read |
| 47806 | 10:27:20.3525601 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 47809 | 10:27:20.3526090 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 80 |
| 47816 | 10:27:20.3527025 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 47817 | 10:27:20.3527192 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DisableHeapLookaside | NAME NOT FOUND | Length: 1,024 |
| 47818 | 10:27:20.3527347 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\FrontEndHeapDebugOptions | NAME NOT FOUND | Length: 1,024 |
| 47819 | 10:27:20.3527503 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ShutdownFlags | NAME NOT FOUND | Length: 1,024 |
| 47820 | 10:27:20.3527665 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UnloadEventTraceDepth | NAME NOT FOUND | Length: 1,024 |
| 47822 | 10:27:20.3527823 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxLoaderThreads | NAME NOT FOUND | Length: 1,024 |
| 47823 | 10:27:20.3527986 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseImpersonatedDeviceMap | NAME NOT FOUND | Length: 1,024 |
| 47825 | 10:27:20.3528148 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TracingFlags | NAME NOT FOUND | Length: 1,024 |
| 47827 | 10:27:20.3528318 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\RaiseExceptionOnPossibleDeadlock | NAME NOT FOUND | Length: 1,024 |
| 47829 | 10:27:20.3528487 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\LegacyDosDevicePaths | NAME NOT FOUND | Length: 1,024 |
| 47831 | 10:27:20.3528653 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CFGOptions | NAME NOT FOUND | Length: 1,024 |
| 47832 | 10:27:20.3528820 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MinimumStackCommitInBytes | NAME NOT FOUND | Length: 1,024 |
| 47834 | 10:27:20.3528987 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\BreakOnInitializeProcessFailure | NAME NOT FOUND | Length: 1,024 |
| 47836 | 10:27:20.3529165 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\KeepActivationContextsAlive | NAME NOT FOUND | Length: 1,024 |
| 47837 | 10:27:20.3529333 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\TrackActivationContextReleases | NAME NOT FOUND | Length: 1,024 |
| 47838 | 10:27:20.3529500 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\MaxDeadActivationContexts | NAME NOT FOUND | Length: 1,024 |
| 47840 | 10:27:20.3529654 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\ImageExpansionMitigation | NAME NOT FOUND | Length: 1,024 |
| 47842 | 10:27:20.3529993 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GlobalFlag2 | NAME NOT FOUND | Length: 1,024 |
| 47843 | 10:27:20.3530180 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\QueryProcessModuleInformationLoopDetectorCount | NAME NOT FOUND | Length: 1,024 |
| 47844 | 10:27:20.3530422 PM | RegOpenKey | HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Segment Heap | REPARSE | Desired Access: Query Value |
| 47846 | 10:27:20.3530650 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager\Segment Heap | NAME NOT FOUND | Desired Access: Query Value |
| 47847 | 10:27:20.3530859 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\CWDIllegalInDLLSearch | NAME NOT FOUND | Length: 1,024 |
| 47852 | 10:27:20.3531473 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 47854 | 10:27:20.3531639 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\GCInterval | NAME NOT FOUND | Length: 1,024 |
| 47860 | 10:27:20.3533946 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Notifications\41911A28A3BC10B5 | BUFFER TOO SMALL | Length: 0 |
| 47866 | 10:27:20.3535342 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\DebugProcessHeapOnly | NAME NOT FOUND | Length: 1,024 |
| 47874 | 10:27:20.3537304 PM | CreateFile | D:\a\basal\basal\spikes\win-confine\target\release\placed | ACCESS DENIED | Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\runneradmin |
| 47924 | 10:27:20.3576337 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\3c74afb9-8d82-44e3-b52c-365dbf48382a | NAME NOT FOUND | Length: 528 |
| 47925 | 10:27:20.3579225 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b749553b-d950-5e03-6282-3145a61b1002 | NAME NOT FOUND | Length: 528 |
| 47926 | 10:27:20.3579896 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | REPARSE | Desired Access: Read |
| 47927 | 10:27:20.3580340 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\StateSeparation\RedirectionMap\Keys | NAME NOT FOUND | Desired Access: Read |
| 47929 | 10:27:20.3582302 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\05f95efe-7f75-49c7-a994-60a55cc09571 | NAME NOT FOUND | Length: 528 |
| 47933 | 10:27:20.3583465 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\UseFilter | NAME NOT FOUND | Length: 544 |
| 47935 | 10:27:20.3583665 PM | RegQueryValue | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\win-confine.exe\SearchPathMode | NAME NOT FOUND | Length: 1,024 |
| 47951 | 10:27:20.3592975 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | REPARSE | Desired Access: Query Value, Set Value |
| 47952 | 10:27:20.3593207 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\SafeBoot\Option | NAME NOT FOUND | Desired Access: Query Value, Set Value |
| 47953 | 10:27:20.3593518 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | REPARSE | Desired Access: Read |
| 47954 | 10:27:20.3593709 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Srp\GP\DLL | NAME NOT FOUND | Desired Access: Read |
| 47956 | 10:27:20.3594234 PM | RegQueryValue | HKLM\SOFTWARE\Policies\Microsoft\Windows\safer\codeidentifiers\TransparentEnabled | NAME NOT FOUND | Length: 80 |
| 47959 | 10:27:20.3594688 PM | RegOpenKey | HKCU\Software\Policies\Microsoft\Windows\Safer\CodeIdentifiers | NAME NOT FOUND | Desired Access: Query Value |
| 47960 | 10:27:20.3595202 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\FileSystem\ | REPARSE | Desired Access: Read |
| 47965 | 10:27:20.3596564 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\FileSystem\LPGO | NAME NOT FOUND | Length: 20 |
| 48046 | 10:27:20.3643688 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Nls\Sorting\Versions | REPARSE | Desired Access: Read |
| 48057 | 10:27:20.3646222 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\5fe982db-5db8-4dc7-9814-2b3ceb277f7e | NAME NOT FOUND | Length: 528 |
| 48063 | 10:27:20.3648731 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\d0f1a5c6-fc43-48ae-99bf-efb1c38be9d1 | NAME NOT FOUND | Length: 528 |
| 48068 | 10:27:20.3652129 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\ca967c75-04bf-40b5-9a16-98b5f9332a92 | NAME NOT FOUND | Length: 528 |
| 48070 | 10:27:20.3652719 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\b6fd710b-f783-4b1c-ab9c-c68099dcc0c7 | NAME NOT FOUND | Length: 528 |
| 48074 | 10:27:20.3653993 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\bf30465b-e93c-46fd-9cdf-f41c8904a01f | NAME NOT FOUND | Length: 528 |
| 48076 | 10:27:20.3654505 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\WMI\Security\c1376338-0984-48b8-b933-9c7d779fd84d | NAME NOT FOUND | Length: 528 |
| 48077 | 10:27:20.3654930 PM | RegOpenKey | HKLM\System\CurrentControlSet\Control\Session Manager | REPARSE | Desired Access: Query Value |
| 48080 | 10:27:20.3655387 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySleepLoopWindowSize | NAME NOT FOUND | Length: 80 |
| 48084 | 10:27:20.3656455 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelaySpinCountThreshold | NAME NOT FOUND | Length: 80 |
| 48085 | 10:27:20.3656726 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayBaseYield | NAME NOT FOUND | Length: 80 |
| 48086 | 10:27:20.3656889 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtFactorYield | NAME NOT FOUND | Length: 80 |
| 48087 | 10:27:20.3657046 PM | RegQueryValue | HKLM\System\CurrentControlSet\Control\Session Manager\SmtDelayMaxYield | NAME NOT FOUND | Length: 80 |
| 48164 | 10:27:20.3683270 PM | ReadFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 29,047 |
| 48166 | 10:27:20.3683448 PM | ReadFile | \Device\NamedPipe | PIPE BROKEN | Offset: 0, Length: 29,047, Priority: Low |
| 48306 | 10:27:20.3766399 PM | RegOpenKey | HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\cmd.exe | NAME NOT FOUND | Desired Access: Query Value, Enumerate Sub Keys |
| 48327 | 10:27:20.3828234 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,022 |
| 48350 | 10:27:20.3832316 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,018 |
| 48525 | 10:27:20.3852391 PM | WriteFile | \Device\NamedPipe | FAST IO DISALLOWED | Offset: 0, Length: 1,024 |
| 49619 | 10:27:20.4023884 PM | RegQueryValue | HKLM\System\CurrentControlSet\Services\bam\State\UserSettings\S-1-5-21-1643835476-1616584234-1346609752-500\\Device\HarddiskVolume5\a\basal\basal\spikes\win-confine\target\release\placed\win-confine.exe | NAME NOT FOUND | Length: 40 |

Last denied event: `{'Time of Day': '10:27:20.3537304 PM', 'Process Name': 'win-confine.exe', 'PID': '4516', 'Operation': 'CreateFile', 'Path': 'D:\\a\\basal\\basal\\spikes\\win-confine\\target\\release\\placed', 'Result': 'ACCESS DENIED', 'Detail': 'Desired Access: Execute/Traverse, Synchronize, Disposition: Open, Options: Directory, Synchronous IO Non-Alert, Attributes: n/a, ShareMode: Read, Write, AllocationSize: n/a, Impersonating: runnervmfi6oq\\runneradmin', 'capture_order': 47874}`
