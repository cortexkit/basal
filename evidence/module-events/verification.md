# Module-event reader verification

## Scope

The reader binds the existing JetStream pull consumer `m_basal` on the account's event stream. It does not provision or change streams or named consumers. Event admission commits each batch's per-flow fan-out before acknowledging messages, with in-progress acknowledgments while a commit is waiting.

The eligibility cutoff is the flow's first approval time. It is stored durably and retained across later version approvals, including package-instance version changes. Only the broker's `Message::info().published` timestamp is compared with that cutoff; publisher headers cannot change eligibility. Older matching deliveries produce no run or receipt and increment the flow's `skipped_before_install` counter. Eligible backlog is admitted oldest-first under the normal rate limit.

Before script startup, the read-only `events_get` tool supplies exact body text on the flow's scoped route. An independently computed SHA-256 hash and the reply digest must both match the notice digest. Verified bytes are journaled and replayed without another live read. Reply digests are bare lowercase hex; only notice digests accept a `sha256:` prefix.

The daemon catalog's event-field adapter remains fail-closed until the catalog carries authoritative `EventDeclaration` values. The declaration conversion is tested independently. Header filtering and explicit historical catch-up opt-in are not implemented.

## Real-server measurements

Tests start an actual **nats-server v2.15.0** with JetStream in an isolated temporary directory. Missing nats-server is a test failure, not a skip. Tests use shorter acknowledgment waits to exercise redelivery without waiting the production consumer's 30 seconds.

On **Linux x86_64 with four vCPUs**, using Cargo/rustc **1.99.0**, an already-approved flow had 200 eligible notices retained before its consumer was bound. The first pull began at stream sequence 1, returned 32 notices, and committed/acknowledged 32 admissions in **11 ms**. Processing all 200 in one logical default rate window admitted 60 runs, queued 120 notices and refused 20, with durable overflow counts. This shows bounded work for eligible history; it is not a throughput benchmark or a measurement of production stream age.

The 120-notice per-flow backlog represents two minutes at the default 60-runs/minute rate. Receipt history lasts eight days; inbox keys and permanent tombstones remain deduplication authorities after receipt history expires.

The real-server suite also checks:

- commit followed by simulated crash before acknowledgment, with redelivery after reopening the store and no second run;
- unmatched notices acknowledged without a receipt;
- fan-out rollback leaving messages unacknowledged when the second flow's insertion fails;
- progress acknowledgments preventing redelivery during a blocked commit;
- notices published before first approval acknowledged without a run, even with a forged future-time header;
- notices published after first approval admitted;
- a newer approval preserving eligibility of notices published between the two approvals.

Core tests cover equality at the approval boundary, per-flow cutoffs, package-instance upgrades, migration backfill from superseded install approval times, backlog filtering, independent body hashing, refusal mapping, retry/deadline handling and replay. A real-worker test checks verified JSON delivery and synthetic capture without live calls.

## Gate results

With migration 21 followed by migration 22, Cargo/rustc 1.99.0:

- `cargo fmt --all --check` passed.
- Workspace clippy with `-D warnings` passed with and without `basal-module/rig-kill-hook`.
- `cargo test --workspace --locked` passed on Linux with eight vCPUs: 1,036 tests, zero ignored, 113 harnesses.
- The same workspace test passed on macOS: 948 tests, one existing ignored test, 113 harnesses.
- `ckdev-mutate check` verified all 898 catalogue rows with runner 0.9.8.
- Python 3.14.4 checks passed: development naming (123 files, zero violations), path-dependency boundary, mutation platform coverage (411 Linux and 487 macOS rows), and 50 script unit tests.

## Reproduction and mutation evidence

Run `cargo test -p basal-core --lib events::tests --locked` and `cargo test -p basal-module --test module_events --test event_preamble --locked`. The CI workflow installs nats-server v2.15.0 using pinned official archive hashes on Linux x86_64/arm64, macOS x86_64/arm64 and Windows x86_64/arm64.

`runner-linux.json` contains **21 CAUGHT** mutation controls, exact red names and per-test failure output, including cutoff removal, approval-time replacement and forged-header timestamp controls. One replay-control run also hit an unrelated codemode model-budget test timeout; the complete affected control was rerun successfully, and the initial attempt remains recorded in `prior_attempts`. Shared `green_name_groups` and per-row exclusions retain every green test identity without repeating whole library lists. Controls with `only=true` require exactly the named failure within their target; the no-match acknowledgment control selects its expected test explicitly. These rows were replayed against their own test target only. The nightly `--broad` run, which replays each mutant against every test target in its package, hasn't covered them yet.

The retention test uses independent eight-day clock boundaries rather than deriving its expected time from the production retention constant.
