# Pre-input handle campaign

Source `f8c0b3427dd34d2dfe460530a73b63c43be0fccf`; run 37992223089; windows-latest.

Each row is a separately launched GUI worker. Counts include only reported probe results and the reported pre-input inventory (including stdio). A missing report is not an empty inventory.

| Recipe | PID | Exit | Reported probes | Successful operations | Reported pre-input handles |
|---|---:|---|---:|---|---:|
| full-gui-control | 3824 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-close-alpc | 7220 | 0x00000000 | 2132 | thread: CreateThread | 26 |
| full-gui-close-file | 3820 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-close-pool | 6252 | 0xc0000008 | not reported |  | 6 |
| full-gui-inspect | 3316 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-connect-trace | 6776 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-parameter-1 | 6148 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-close-combined | 1044 | 0x00000000 | 2132 | thread: CreateThread | 26 |
| full-gui-close-removable | 9396 | 0x00000000 | 2132 | thread: CreateThread | 14 |
| full-gui-serial-0 | 9384 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-serial-1 | 10032 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-close-event | 7832 | 0xc0000008 | not reported |  | 20 |
| full-gui-close-completion | 6356 | 0x00000000 | 2132 | thread: CreateThread | 25 |
| full-gui-close-factory | 6640 | 0xc0000008 | not reported |  | not reported |
| full-gui-close-timer | 4704 | 0x00000000 | 2132 | thread: CreateThread | 23 |
| full-gui-close-packet | 6856 | 0x00000000 | 2132 | thread: CreateThread | 21 |
| full-gui-close-semaphore | 8212 | 0x00000000 | 2132 | thread: CreateThread | 27 |
| full-gui-close-scheduler | 9044 | 0xc0000008 | not reported |  | 26 |
