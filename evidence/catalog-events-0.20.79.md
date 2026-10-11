# Catalog event declarations on daemon 0.20.79

## Source and capability gate

The recorded fixture is `crates/basal-module/tests/data/live-catalog-0.20.79.json`.
The fixture is trimmed from the live catalog captured by ck 0.20.79 at daemon
revision `34336c0e` and plexus revision `2cb37682`. It retains the unchanged daemon
capability array, each module's `module_id` and `control_ops`, and the full,
unchanged `events` arrays. `roles` is intentionally empty for both modules; the
SDK requires that collection but permits it to be empty. Private provider
operations, tool descriptions and schemas are omitted. The daemon capability
array advertises `catalog-events/v1`.

The following versions and build commits identify the live binaries that
supplied the declarations, for reproducibility. They were read through
`ck --version`, `ck --json provenance plexus` and `ck --json daemon`:

- ck: `0.20.79`
- daemon: `34336c0e14feb011cd857ae430e694463b1d5bfb`
- plexus: `2cb37682c9e550ee1124df6fb80a1dac0d6c5176`

The production adapter uses the same daemon-wide capability list supplied by
HELLO_ACK. Its shared state starts false and is updated on every registration,
including reconnects. `BasalHandler::on_hello_ack` updates the capability before
its initialization early return so reusing the open store does not leave the
previous daemon's capability state in place.
No daemon version or publisher role is treated as evidence of event support.
Missing and empty `events` admit no event; malformed declarations fail closed.
The mapping preserves name, version, headers and optional summary, with empty
filter hints when headers are omitted.

Published dependencies checked through crates.io: subc-protocol `0.30.2`,
subc-client-rs `0.27.2`, subc-control `0.29.5`, subc-os `0.1.15`,
subc-transport `0.11.1`. `cargo update` changed only those five lockfile packages,
including subc-os's existing windows-sys dependency and subc-transport's subc-os
edge. `cargo fetch --locked` installed the selected crates locally. Existing
source-match strings in `mutations.toml` were refreshed for the new protocol and
dependency edges, so the dependency-fence and fabricated-event controls still
apply their intended edits without changing the properties they guard.

## Recorded-catalog install check

The isolated daemon test environment managed by `script/flows-rig.sh` has no
plexus repository, build option or configured module. The reviewer explicitly
chose the recorded-catalog fallback instead of building that environment; a live
install will be checked after merge. No production install was attempted.

The test decodes the trimmed entries through the SDK's `CatalogList`, preserving
the real subc-control decoding boundary, then passes its serialized modules to
`SubcCatalog`. It also verifies prefrontal-core's `digest_delivered` v1 declaration.
After decoding, the test supplies a synthetic read-only `events_get` tool using
basal's public test declaration. This avoids copying plexus's private provider
surface while satisfying install validation's requirement for an event-body
resolver. The test checks live event declarations, not live resolver schemas.
Install uses the normal `flow.install` entry point; dry run uses `flow.dry_run`.
A mock replaces the external consent service. The accepted `flow.install` reply
means the event flow passed validation and awaits consent; it does not establish
production consent behavior or activate a production flow.

Command (Linux, 4 vCPUs), with the capability-gate bypass and fabricated-mapping
mutations restored so acceptance is checked against the actual implementation:

```sh
cargo test -p basal-module --test catalog_events --locked -- --nocapture
```

Exact application output:

```text
live catalog plexus.github_pr_changed v1 install: {"attempt":1,"card_id":"card:catalog-event-flow:v1:601b74a4b13372aa","code_hash":"601b74a4b13372aa9c30635ae1ad60799374fba841ce73a70803e85c74a56792","dry_run":{"mode":"capture","partial":false,"runs":[],"window":{"covered":null,"kind":"events","note":"event replay needs the event plane; no events were replayed"}},"flow_id":"catalog-event-flow","new":true,"state":"pending","version":1,"warnings":[]}
live catalog plexus.undeclared_event v1 install: event_not_declared
live catalog plexus.github_pr_changed v99 install: event_not_declared
live catalog plexus.github_pr_changed v1 dry-run: 7
```

All four integration tests passed. Additional cases refuse installs before the
first acknowledgement, without the capability, with missing/empty events, and
with an undeclared name or version. Dry run revalidates stored declarations and
refuses capability removal, missing/empty events, and name/version changes using
its existing `invalid_params` error containing `UnknownEvent`.

## Mutation witnesses

Both new controls declare `platforms = ["linux"]`. The CI partition validator
`.github/workflows/check_mutation_coverage.py` requires exactly one replay
platform per row to assign each control to one platform's CI shards. The normal
workspace suites run on both platforms. Intentional source mutations are marked
with the comment token `NON-VACUITY BREAK` to distinguish temporary broken code
from the implementation; those edits are restored before committing.
For each mutation, the implementation was first staged and `git diff --stat`
was empty. During each mutation, the diff was exactly:

```text
 crates/basal-host/src/subc_catalog.rs | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)
```

Restoration used `git checkout -- crates/basal-host/src/subc_catalog.rs && touch
crates/basal-host/src/subc_catalog.rs`; `git diff --stat` was empty after each,
confirming the temporary source edit was fully restored to the staged implementation.

### catalog-events-capability-gate-bypassed

Replaced the capability guard with `false &&` and marked `NON-VACUITY BREAK`.
`cargo test -p basal-module --test catalog_events --locked` ran all four tests:

```text
test catalog_events_require_an_acknowledged_daemon_capability ... FAILED
test declared_catalog_event_is_accepted_by_install_and_dry_run ... ok
test recorded_live_plexus_catalog_accepts_github_pr_changed_v1 ... ok
test missing_empty_or_unmatched_catalog_events_are_refused ... ok
test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
```

The failure was `unwrap_err()` receiving an accepted pending install without a
HELLO_ACK. No other test failed.

### catalog-event-mapping-fabricated

Replaced the declaration decoder with `EventDeclaration::new("fabricated_event",
99)` and marked `NON-VACUITY BREAK`. `cargo test -p basal-host --lib --locked`:

```text
test subc_catalog::event_tests::catalog_event_mapping_preserves_names_versions_and_filter_headers ... FAILED
test result: FAILED. 73 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
```

All other 73 host unit tests stayed green, including the existing name/version
lookup test. After restoration the four integration tests and both catalog unit
tests passed again.

## Verification

- Rust: cargo `1.99.0 (5f94df478 2026-08-27)`, rustc `1.99.0 (b940084d7 2026-09-28)`;
  clippy `0.1.99 (b940084d7e 2026-09-28)`; rustfmt `1.10.0-stable (b940084d7e 2026-09-28)`.
- `cargo fmt --all --check`: passed, workspace formatting checked.
- `cargo clippy --workspace --all-targets --locked -- -D warnings`: passed on Linux.
- `cargo clippy --workspace --all-targets --locked --features basal-module/rig-kill-hook -- -D warnings`:
  passed on Linux. Each configuration checked all targets in the eight workspace crates.
- `cargo test --workspace --locked`: Linux (8 vCPUs), 1,251 passed, 0 failed, 0 ignored;
  macOS, 1,163 passed, 0 failed, 1 ignored, each across 131 test/doc-test targets.
- `ckdev-mutate check`: passed with ckdev-mutate `0.10.0`; all 1,113 controls' anchors
  and exact test names verified.
- Python `3.14.4` on Linux: `python3.14 script/check-ckdev-names.py` passed (147 files,
  0 violations); `python3.14 script/check-path-deps.py` passed (eight local workspace
  packages); `python3.14 .github/workflows/check_mutation_coverage.py` passed
  (547 Linux, 499 macOS, 67 Windows rows, exactly one platform per row);
  `python3.14 -m unittest script.tests` passed (62 tests).
- Fresh Rust diagnostics for the four changed Rust files: 0 errors/warnings.
  The inspection's call-graph categories were unavailable; clippy and workspace tests
  were the authoritative gates.
- Comment-clarity review examined the implementation and evidence for rationale
  understandable without task context. Flagged provenance and evidence wording
  was clarified; the lockfile and JSON fixture were skipped as data.

## Public fixture minimization

The live fixture was reduced from 3,156 lines (92,742 bytes) to 180 lines
(4,109 bytes). A raw JSON field comparison against the committed capture verified
byte-for-byte preservation of daemon capabilities and both modules' `module_id`,
`control_ops` and complete `events` values. No provider roles, operation lists,
tool descriptions or schemas remain in the fixture. This evidence file contains
only provenance, verification and application replies, not either private catalog.
The four catalog-events tests and both mutation controls were rerun against the
trimmed fixture; each mutation failed only its expected test and was restored.

Follow-up verification used cargo `1.99.0`, rustfmt `1.10.0-stable`, clippy
`0.1.99` and Linux ckdev-mutate `0.9.8`. The focused catalog-events clippy target
passed with warnings denied. Both commands
`ckdev-mutate run --only catalog-events-capability-gate-bypassed --allow-dirty`
and `ckdev-mutate run --only catalog-event-mapping-fabricated --allow-dirty`
reported `CAUGHT`; those runs did not perform a broad mutation audit. Separate
apply/test/restore witnesses against the trimmed fixture reproduced the exact
red test names and green companion-test counts recorded above, with a non-empty
one-line source diff during each mutation and an empty diff after restoration.
The four catalog-events integration tests were also rerun on Linux and macOS.
The unpublished task commit was amended so the removed catalog content is not
part of this task branch's committed history.
