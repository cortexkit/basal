# Evidence

Benchmark output that backs basal's performance claims. The measurement JSON files are rewritten by the commands below.

## Mutation controls

Safety proofs are checked in as [`mutations.toml`](../mutations.toml), not as stale machine output. The pinned shared `ckdev-mutate` runner applies each edit, builds the mutant, requires the exact named test to fail, and restores source bytes. Other tests in the target may also fail: the original proofs tested one named guard, not target-wide exclusivity. See the [README](../README.md#mutation-proofs) for installation, replay and `ckdev-mutate prove` commands.

The catalogue is the source of truth for control counts; `ckdev-mutate check` validates and counts the current rows. Some controls change dependencies: for example, one adds an older subc-protocol to prove that only one version is ever linked, and one downgrades subc-client-rs to prove the serve loop survives an unknown control field. CI builds with `--locked`, so each also carries the exact `Cargo.lock` edit Cargo resolves for its mutant, and the runner restores and byte-verifies that file too.

Pool tests bound worker handshakes, acquisition, activation and quiescence waits. A timeout names the stalled operation, stops the pool, and kills and reaps recorded child workers, including workers without a run binding; pool shutdown reaps idle workers. The deadline's own proof deliberately stalls a fixture with a real worker and uses an independent outer bound to detect removal of the inner deadline without leaking its waiter or worker. The original startup-order control keeps its full `pool` target and named guard: failures of other pool tests are allowed, but cannot prevent that guard from running indefinitely.

Write local reports under gitignored `target/mutations/` with `--report`. Each JSON row records its ID, outcome, full red and green test names, separate build and test milliseconds, reasons and output tails. All rows must be `CAUGHT`. CI uploads these reports as artifacts for PR diff replays and sharded full replays on main; no generated mutation report is committed here.

On main, the full catalogue runs as five shards, because GitHub runs at most five macOS jobs at once for this account, so five shards finish in one wave. Each shard starts from the build the checks job saved for the same commit, so its first mutant recompiles only the edited crate rather than the whole workspace. Size the shard count from the run times GitHub reports, not from local runs: a shared development machine queues compiles and inflates every figure.

## Measurements

| File | What | Command |
|---|---|---|
| `slice-1-measurements.json` | Worker spawn, respawn and warm-pool cost, IPC cost per activation and per host call, prefix replay, RSS, and a payload fuzzing run | `worker-bench --worker <ck-basal-worker> --out <file> --fuzz 2000` (`cargo build --release -p basal-testkit --bin worker-bench`) |
| `slice-2-measurements.json` | Store commit cost with and without `F_FULLFSYNC`, replay through the parent with the store, and a new call end to end | `cargo run --release -p basal-testkit --bin journal-bench -- --worker target/release/ck-basal-worker --out evidence/slice-2-measurements.json --samples 20` |

Every timing sample is kept. Times are wall-clock and were taken on a shared, heavily loaded Apple M5 Max (18 cores, 128 GiB); `slice-1-measurements.json` records the machine and its load average. Read the tails as a description of that environment, not of capacity.
