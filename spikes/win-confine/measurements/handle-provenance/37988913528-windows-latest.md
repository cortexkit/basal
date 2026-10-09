# Pre-input handle campaign

Source `fabfc81507b1bcf1eb71c0facbc04450fb0248d5`; run 37988913528; windows-latest.

| Recipe | PID | Exit | Probes | Successful operations | Pre-input handles |
|---|---:|---|---:|---|---:|
| full-gui-control | 5184 | 0x00000000 | 2146 | thread: CreateThread | 27 |
| full-gui-close-alpc | 9092 | 0x00000000 | 2146 | thread: CreateThread | 26 |
| full-gui-close-file | 4744 | 0x00000000 | 2146 | thread: CreateThread | 27 |
| full-gui-close-pool | 1184 | 0xc0000008 | 0 |  | 6 |
| full-gui-inspect | 4444 | 0x00000000 | 2146 | thread: CreateThread | 27 |
| full-gui-close-combined | 7072 | 0x00000000 | 2146 | thread: CreateThread | 26 |
| full-gui-serial-0 | 8176 | 0x00000000 | 2146 | thread: CreateThread | 27 |
| full-gui-serial-1 | 5408 | 0x00000000 | 2146 | thread: CreateThread | 27 |
| full-gui-close-event | 8132 | 0xc0000008 | 0 |  | 20 |
| full-gui-close-completion | 6456 | 0x00000000 | 2146 | thread: CreateThread | 25 |
| full-gui-close-factory | 5524 | 0xc0000008 | 0 |  | 0 |
| full-gui-close-timer | 6160 | 0x00000000 | 2146 | thread: CreateThread | 23 |
| full-gui-close-packet | 5564 | 0x00000000 | 2146 | thread: CreateThread | 21 |
| full-gui-close-semaphore | 3296 | 0x00000000 | 2146 | thread: CreateThread | 27 |
| full-gui-close-scheduler | 7964 | 0xc0000008 | 0 |  | 26 |
