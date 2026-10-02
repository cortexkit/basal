# Evidence

Machine output that backs basal's tests and performance claims. Nothing here is written by hand: every file is rewritten by the command named below, and the file names are fixed because the runner and the scripts name them.

## Mutation controls

A mutation control disables one safety mechanism with a temporary source edit, runs the one test named for that mechanism, records whether the test went red, and restores the source. A test that stays green with its mechanism disabled is not testing it. Each file is a JSON array with one entry per control:

- `control`: the mechanism that was disabled;
- `expected_red`: the test that must fail;
- `captured_output`: the failing test output;
- `applied_evidence`: the files changed and the `git diff --stat` during the edit and after the restore;
- `outcome`: `reddened` when the named test failed.

| File | Controls for | Command |
|---|---|---|
| `slice-1-mutations.json` | the worker engine and its IPC (`basal-worker`, `basal-proto`) | `cargo run -p basal-testkit --bin mutation-controls` |
| `slice-2-mutations.json` | the journal, the run state machine and the activation driver (`basal-core`) | `... --bin mutation-controls -- --journal` |
| `slice-3-mutations.json` | manifests, authorization, audit, tokens, `kv`, slots, deadlines, rate limits and disables (`basal-core`) | `... --bin mutation-controls -- --dispatch` |
| `slice-4-mutations.json` | the scheduler (`basal-core`) | `... --bin mutation-controls -- --schedule` |
| `slice-5-mutations.json` | the module shell: worker pool, engine, ops, dry run, consent cards, manifest (`basal-module`) | `... --bin mutation-controls -- --module` |
| `i1a-host-mutations.json` | the consumer adapters to the fleet's modules and their journal integration | `... --bin mutation-controls -- --hosts` |
| `i1a-broca-mutations.json` | the model host: Broca contracts, recovery, token metadata and restarts | `... --bin mutation-controls -- --broca` |

`script/verify-controls.sh` runs every set in turn, checks that each run rewrote its file and that every control reddened, then restores the committed files. To refresh a file, run its command on a clean tree and commit the result. The runner refuses to start while the tree has unstaged changes.

## Measurements

| File | What | Command |
|---|---|---|
| `slice-1-measurements.json` | Worker spawn, respawn and warm-pool cost, IPC cost per activation and per host call, prefix replay, RSS, and a payload fuzzing run | `worker-bench --worker <ck-basal-worker> --out <file> --fuzz 2000` (`cargo build --release -p basal-testkit --bin worker-bench`) |
| `slice-2-measurements.json` | Store commit cost with and without `F_FULLFSYNC`, replay through the parent with the store, and a new call end to end | `cargo run --release -p basal-testkit --bin journal-bench -- --worker target/release/ck-basal-worker --out evidence/slice-2-measurements.json --samples 20` |

Every timing sample is kept. Times are wall-clock and were taken on a shared, heavily loaded Apple M5 Max (18 cores, 128 GiB); `slice-1-measurements.json` records the machine and its load average. Read the tails as a description of that environment, not of capacity.
