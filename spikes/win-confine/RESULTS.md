# Windows confinement measurements

## Evidence status

Native measurement is in progress. No residual is inferred from a failed
launcher, and cross-compilation is not runtime evidence.

## Iteration ledger

- Run [37802630936](https://github.com/cortexkit/basal/actions/runs/37802630936),
  image source `137dd60d91baa0c8f37317691bbddfc9073b3b77`:
  Windows-latest passed formatting, checking, three native tests and a release
  build with `-C target-feature=+crt-static` (rustc 1.99.0). The measurement
  command exited 1 with `GetTokenInformation(size): Win32 87` (invalid
  parameter), before writing its report. That diagnostic lacked the token
  information class, so it cannot identify the failed class retrospectively.
  Attestation now skips package-only information for non-AppContainer tokens,
  records the exact class on errors, and can query the LPAC flag via NT when
  the Win32 dispatcher rejects it. A native test covers normal-token attestation.
- The same artifact's PE inventory had nine normal import descriptors
  (duplicate casing of Kernel32 and Winsock), and **zero delay imports**:
  `ntdll.dll` (18 symbols), `ole32.dll` (1), `kernel32.dll` (45),
  `ws2_32.dll` (1), `advapi32.dll` (22), `userenv.dll` (4),
  `api-ms-win-core-synch-l1-2-0.dll` (3), `KERNEL32.dll` (72),
  `WS2_32.dll` (15). These are preliminary, not the final image inventory.

## Working API sequence and attestation

Pending completed native reports.

## Measured residual and risk assessment

Pending completed native reports. A missing report means **not measured**, not
that all opens were denied.

## Control comparison

Pending completed native reports.

## Final import and loaded-image inventory

Pending the final measured artifact.

## Recommended Windows design changes

Pending native reachability and launch evidence.
