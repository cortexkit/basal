# Evidence

Benchmark output that backs basal's performance claims. The measurement JSON files are rewritten by the commands below.

## Mutation controls

Safety proofs are checked in as [`mutations.toml`](../mutations.toml), not as stale machine output. The pinned shared `ckdev-mutate` runner applies each edit, builds the mutant, requires the exact named test to fail, and restores source bytes. Other tests in the target may also fail: the original proofs tested one named guard, not target-wide exclusivity. See the [README](../README.md#mutation-proofs) for installation, replay and `ckdev-mutate prove` commands.

The catalogue is the source of truth for control counts; `ckdev-mutate check` validates and counts the current rows. One control changes dependencies. `worker-depends-on-a-subc-crate` adds an older subc-protocol, the daemon's wire-protocol crate, to the worker's manifest, to prove the worker's dependency fence catches any subc crate linked into the worker. CI builds with `--locked`, so the control also carries the exact `Cargo.lock` edit Cargo resolves for its mutant, and the runner restores and byte-verifies that file too.

The retired `unknown-channel-zero-control-field-ends-basal-serving` control downgraded subc-client-rs, the Rust SDK basal uses to talk to the daemon, to 0.26.0. That release depends on protocol 0.29 and cannot build with basal's protocol 0.30 route types. The SDK owns the guard that refuses an undecodable request on control channel 0 without closing the serving connection; basal delegates serving to the SDK, and the SDK's own control-request tests cover the refusal and following health response. Basal still runs `unknown_control_field_is_refused_and_the_following_health_check_is_answered` in `crates/basal-module/tests/unknown_control.rs` as a normal regression test of its pinned SDK.

Pool tests bound worker handshakes, acquisition, activation and quiescence waits. A timeout names the stalled operation, stops the pool, and kills and reaps recorded child workers, including workers without a run binding; pool shutdown reaps idle workers. The deadline's own proof deliberately stalls a fixture with a real worker and uses an independent outer bound to detect removal of the inner deadline without leaking its waiter or worker. The original startup-order control keeps its full `pool` target and named guard: failures of other pool tests are allowed, but cannot prevent that guard from running indefinitely.

Write local reports under gitignored `target/mutations/` with `--report`. Each JSON row records its ID, outcome, full red and green test names, separate build and test milliseconds, reasons and output tails. All rows must be `CAUGHT`. CI uploads these reports as artifacts for PR diff replays and sharded full replays on main; no generated mutation report is committed here.

On main, the full catalogue runs as five shards, because GitHub runs at most five macOS jobs at once for this account, so five shards finish in one wave. Each shard starts from the build the checks job saved for the same commit, so its first mutant recompiles only the edited crate rather than the whole workspace. Size the shard count from the run times GitHub reports, not from local runs: a shared development machine queues compiles and inflates every figure.

## Measurements

| File | What | Command |
|---|---|---|
| `slice-1-measurements.json` | Worker spawn, respawn and warm-pool cost, IPC cost per activation and per host call, prefix replay, RSS, and a payload fuzzing run | `worker-bench --worker <ck-basal-worker> --out <file> --fuzz 2000` (`cargo build --release -p basal-testkit --bin worker-bench`) |
| `slice-2-measurements.json` | Store commit cost with and without `F_FULLFSYNC`, replay through the parent with the store, and a new call end to end | `cargo run --release -p basal-testkit --bin journal-bench -- --worker target/release/ck-basal-worker --out evidence/slice-2-measurements.json --samples 20` |

Every timing sample is kept. Times are wall-clock and were taken on a shared, heavily loaded Apple M5 Max (18 cores, 128 GiB); `slice-1-measurements.json` records the machine and its load average. Read the tails as a description of that environment, not of capacity.
