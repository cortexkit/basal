# Evidence

Benchmark output that backs basal's performance claims. The measurement JSON files are rewritten by the commands below.

## Mutation controls

Safety proofs are checked in as [`mutations.toml`](../mutations.toml), not as stale machine output. The pinned shared `ck-mutate` runner applies each edit, builds the mutant, requires the exact named test to fail, and restores source bytes. Other tests in the target may also fail: the original proofs tested one named guard, not target-wide exclusivity. See the [README](../README.md#mutation-proofs) for installation, replay and `ck-mutate prove` commands.

The catalogue has 383 product controls: worker 32, journal 48, dispatch 47, schedule 35, module 97, Broca 28, hosts 74 and built-ins 22. The sole retired control, `integration: unknown flags become filters`, guarded the deleted private runner rather than product behavior. The dependency-fence proof includes a Cargo-resolved lockfile edit so its mutant can build with `--locked`; the shared runner restores and byte-verifies that file too.

Write local reports under gitignored `target/mutations/` with `--report`. Each JSON row records its ID, outcome, full red and green test names, separate build and test milliseconds, reasons and output tails. All rows must be `CAUGHT`. CI uploads these reports as artifacts for PR diff replays and sharded full replays on main; no generated mutation report is committed here.

## Measurements

| File | What | Command |
|---|---|---|
| `slice-1-measurements.json` | Worker spawn, respawn and warm-pool cost, IPC cost per activation and per host call, prefix replay, RSS, and a payload fuzzing run | `worker-bench --worker <ck-basal-worker> --out <file> --fuzz 2000` (`cargo build --release -p basal-testkit --bin worker-bench`) |
| `slice-2-measurements.json` | Store commit cost with and without `F_FULLFSYNC`, replay through the parent with the store, and a new call end to end | `cargo run --release -p basal-testkit --bin journal-bench -- --worker target/release/ck-basal-worker --out evidence/slice-2-measurements.json --samples 20` |

Every timing sample is kept. Times are wall-clock and were taken on a shared, heavily loaded Apple M5 Max (18 cores, 128 GiB); `slice-1-measurements.json` records the machine and its load average. Read the tails as a description of that environment, not of capacity.
