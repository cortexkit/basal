# Evidence

Benchmark output that backs basal's performance claims. The measurement JSON files are rewritten by the commands below.

## Measurements

| File | What | Command |
|---|---|---|
| `slice-1-measurements.json` | Worker spawn, respawn and warm-pool cost, IPC cost per activation and per host call, prefix replay, RSS, and a payload fuzzing run | `worker-bench --worker <ck-basal-worker> --out <file> --fuzz 2000` (`cargo build --release -p basal-testkit --bin worker-bench`) |
| `slice-2-measurements.json` | Store commit cost with and without `F_FULLFSYNC`, replay through the parent with the store, and a new call end to end | `cargo run --release -p basal-testkit --bin journal-bench -- --worker target/release/ck-basal-worker --out evidence/slice-2-measurements.json --samples 20` |
| `throwaway-durability.md` | One before/after Mac unit-suite run, with build and test time separated | `/usr/bin/time -p cargo test -p basal-core --lib` |

Every timing sample is kept. Times are wall-clock and were taken on a shared, heavily loaded Apple M5 Max (18 cores, 128 GiB); `slice-1-measurements.json` records the machine and its load average. Read the tails as a description of that environment, not of capacity.
