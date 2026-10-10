# Module-event reader verification

## Delivery boundary and upstream prerequisites

- The given ck-bus contract provisions `m_basal` on `CK_<ACCOUNT>_EVENT`, with deliver-all, explicit acknowledgment, a 30-second ack wait and at most 1,000 pending deliveries (`subconscious/crates/ck-bus/src/bootstrap/module_durables.rs`, as supplied in the brief). Basal only binds that consumer; it never provisions or changes it.
- Catalog event propagation is not released in the pinned daemon protocol. Per the parent decision, exactly one adapter remains: `basal_host::subc_catalog::catalog_event_declarations`. It deliberately returns `None`; `declared_event` already accepts `subc_protocol::manifest::EventDeclaration` and is tested. Production event-manifest installation remains fail-closed until that adapter can read the authoritative released catalog field. No wire field or compatibility shim was invented.
- The parent confirmed Plexus's separate Pure query tool `events_get`, with exactly `{event_key, event_name}`. `basal_host::catalog::EVENT_BODY_OP` is the one tool-name constant. Flows never call the Mutating `events` tool to fetch a body. The body is exact JSON text; its SHA-256 digest is bare lowercase hex.
- Pins match the supplied prefrontal references: async-nats 0.50 and the three cortexkit-bus crates at commons `e4fb106f581c4d208a81e924a6329cfb20de616d`. Credential and nonce-sign request shapes follow the supplied `.cortexkit/refs/prefrontal/mod.rs:135-295`, using basal's existing reserved transport. The copied commons snapshot is newer than the pin; the fetched pin was checked for ContentDigest's actual spelling. Its `message.rs:30-36` and codec emit bare hex, contrary to the reported prefixed Display format. Only notice-side `sha256:` is optionally normalized, as explicitly approved; reply digests remain strictly bare hex.
- Migration **22** is reserved for these tables: the parent assigned 21 to codemode model tools after the task began. The task's local main ref still points to the supplied base; no unlanded sibling migration was imported.

## First retained batch and limits

A real nats-server v2.15.0 fixture published 200 retained notices before binding. The first pull began at stream sequence 1, returned 32 notices, admitted 32 runs, and committed/acked in **21 ms** on Linux. Processing all 200 under one default rate window admitted 60, queued 120 and refused 20 with durable flow-health overflow counts. The backlog bound is two minutes at the default 60-runs/minute limit. Backlog draining is oldest-first and uses the existing saturation/auto-disable policy. Receipt history lasts eight days; inbox keys and permanent tombstones still prevent duplicate effects after history expires.

This is a fixture measurement, not a claim about the fleet's retained stream. No live reserved basal credential or daemon account was available for measuring its first production pull. Module health now exposes first-batch age/size/elapsed time and reader reconnect/backoff. If the production stream proves to be a historical flood, an approval-time history cutoff with explicit operator catch-up opt-in is a follow-up policy proposal; no cutoff was silently introduced.

Header filters are also a follow-up: the current manifest only has module/name/version and refuses unknown filter fields.

## Gates

Final restored source, including migration 22:

- Cargo 1.99.0, rustc 1.99.0: `cargo fmt --all --check` passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` passed for all seven workspace crates on Linux.
- The same clippy command with `--features basal-module/rig-kill-hook` passed.
- `cargo test --workspace --locked`, Linux 8 vCPUs: **1,012 passed**, zero ignored, 113 harnesses.
- The same workspace test on macOS: **924 passed**, one existing ignored test, 113 harnesses. Both platforms ran all five real JetStream tests and the real-worker event preamble/capture test.
- `ckdev-mutate check`, runner 0.9.8: all **877** catalogue rows' anchors and exact test names verified. Existing anchors for changed lockfile dependency lists, event lookup, overflow output and retention wake-up were refreshed without changing their guarding assertions.
- Python 3.14.4 on Linux: development naming checked 123 files with zero violations; dependency-boundary check passed; mutation coverage checked 877 rows (390 Linux, 487 macOS); `python3.14 -m unittest script.tests` passed 50 tests.
- actionlint 1.7.12 checked `.github/workflows/ci.yml` successfully. CI installs pinned nats-server v2.15.0 archives with official SHA256SUMS-derived hashes on Linux x86_64/arm64, macOS x86_64/arm64 and Windows x86_64/arm64. Missing nats-server is a loud test failure, never a skip.
- Sidekick reviewed all code/comment/prose changes (27 files, excluding lockfile data); genuinely unclear comments were rewritten. Scoped diagnostics reported no errors/warnings; non-diagnostic inspect categories were unavailable, so cargo gates are authoritative.

Dependency installation used `cargo fetch` in this worktree. Remote outbound-network fetch was refused; local fetch updated Cargo.lock, then all Linux gates used the locked resolution. No path dependency was added outside the workspace.

## Non-vacuity evidence

`runner-linux.json` records **18 CAUGHT** controls from `ckdev-mutate run`, with each exact red name, failure text and green identities. Shared `green_name_groups` plus each row's exclusions preserve all names without repeating the whole library list. Seventeen rows ran their complete target with `only=true`; the no-match ack row selected only its expected test because removing all acknowledgments also breaks the other ack tests. No broad cross-target audit is claimed.

`manual-mutation-evidence.json` records corresponding native macOS staged-index cycles: empty working diff; apply the marked mutation; non-empty diff; execute the exact named test; restore with `git checkout -- <path>` and `touch`; empty diff again. Every final control reddened with no collateral test failure in its selected run.

The initial retention control survived: the original new test derived its clock boundary from the production retention constant and therefore checked a proxy. `manual-mutation-evidence-initial.json` preserves that survivor. The test was corrected to use independent eight-day times, after which the same control failed on both platforms. Other tests already provided reddened controls reaching the same basal-core target. No existing test assertion was rewritten to accept new behavior.
