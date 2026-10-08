# Worker and IPC audit disposition

## Merge coordination

- The parent authorized the minimal update to `crates/basal-testkit/tests/journal_admin.rs::pruning_spares_unfinished_runs_and_open_obligations`, which normally belongs to slice G. It previously asserted immediate script failure with outstanding calls. It now preserves the pruning property while asserting suspension until every outcome is consumed, then script failure and successful pruning. The parent is coordinating ownership with slice G.
- Two existing `mutations.toml` rows needed mechanical re-anchoring: `recorded-clock-and-random-outcomes-are-not-served-from-the-prefix` uses `take` instead of `clone`; `memory-limit-not-applied` targets the capped allocator instead of `Runtime::set_memory_limit`. Their intended properties are unchanged.
- No manifests, lockfiles, engine patches, parser dependencies, or schema were changed.

## Findings

| Finding | State | Evidence |
|---|---|---|
| W1 | FIXED | The auditor's stolen `hostError` repro initially produced `ScriptHostRejection { position: 12345 }`. Non-configurable data properties now replace both context-backed Error setters. The retained lockdown test checks descriptors, caller/arguments routes and the repro; its mutation is CAUGHT. |
| W2 | NEEDS A DECISION | The repro executes top-level code, but retained tests prove that evaluation is locked down, journaled/replayed, and bounded by all three activation budgets. QuickJS's AsyncFunction constructor has the same string-pasting defect, so it is not a fix. The parent requested no parser dependency or behavior change. |
| W3 | FIXED | A sync prefix with orders 5 then 2 initially completed. Planning now rejects it before any call is sent; the corresponding mutation is CAUGHT. Existing replay tests cover mixed asynchronous/synchronous release ordering. |
| W4 | FIXED | Production consumes the owned IPC request, moves its prefix, takes each consumed recorded outcome, and encodes borrowed HostCalls. This removes the full-prefix deep copy and per-call JSON copies; all replay tests pass. |
| W5 | NEEDS A DECISION | The parent retained the stack-label contract pending an engine decision. The independently fixable parts are fixed: memory classification uses native allocator refusal or binding Allocation errors, JS time uses the native interrupt/clock signal, and descriptions read string-valued own data properties without invoking getters or coercion. Both new memory-spoof/getter controls are CAUGHT. |
| W6 | FIXED | The retained in-process test initially left the issued call in the link's waiting queue. Rejected scripts now drain dispatched calls, or suspend on long calls, before returning rejection. The draining mutation is CAUGHT; the authorized pruning fixture preserves its original pruning claim. |
| W7 | FIXED | Counts require each entry's minimum encoded size (8-byte positions and 42-byte recorded calls), rather than one byte. The new test initially accepted two positions backed by only two bytes; the count mutation is CAUGHT. Documentation distinguishes Rust-object overhead from encoded size. |
| W8 | FIXED | Incoming payloads grow only after bytes arrive; a header alone no longer reserves 32 MiB. Raw writes do not copy the payload. Encoders receive capped capacity hints. These are allocation-shape changes, not wire-behavior changes; IPC and protocol round trips pass. |
| W9 | FIXED | Encoders reject oversize scripts, names and lists, and both decoder/native bridge reject empty module/op names. Encoder and raw-bridge regressions initially failed and now pass; the encoder name control is CAUGHT. |
| W10 | FIXED | Shutdown interrupts both blocked and synchronous activations without emitting a refusal or Finished frame. The new test initially returned Broken; its mutation is CAUGHT. |
| W11 | FIXED | The Oversized documentation now names exit code 3. |
| W12 | FIXED | Removed the unused stream_intact method and redundant post-cap error branches. DecodeError::Invalid is now genuinely constructed for empty names and is retained. |
| W13 | FIXED | Descriptor closure occurs once at confinement entry, or once in the explicitly unconfined diagnostic probe. Confinement::None remains the protocol's unconfined value, not a production claim; normal startup still refuses an unsupported sandbox. Inherited-descriptor and confinement tests pass. |
| W14 | FIXED | The frame-limit rationale names the real 10,000-call run cap and independent aggregate bounds. MAX_LIST_ENTRIES remains the protocol-list limit; the fs listing cap is a different, module-qualified limit, not a conflicting contract. |
| W15 | FIXED | One UTF-8 prefix helper serves wire details, debug display and engine error truncation. A retained test executes all 21 JavaScript primitives and compares the actual issued Rust kinds; a changed JS primitive code reddens only that test. |
| W16 | FIXED | The minimum result cap is derived from the four-byte JSON null literal and explained. Absent names in the removed-global denylist remain defensive against future context/engine additions, rather than being removed from the security policy. |

## Decisions: downstream effects and recommendation

### W2: function-body shape

The original wrapper can be closed early. However, evaluation occurs inside `Activation::enter`, after `harden`, with the same runtime memory/stack limits and JS clock/interrupt as the body. `top_level_wrapper_code_has_activation_budgets` proves the CPU, memory and stack cases; `top_level_wrapper_code_is_locked_down_journaled_and_replayed` proves Date/random calls are journaled, replay consumes them without new host calls, and Function/eval/raw bridge remain absent. No extra host capability was found.

The remaining effect is structural/cosmetic: the accepted source is not necessarily a single function body as readers assume, and may introduce activation-local global lexical bindings. A private AsyncFunction constructor was tried and the same repro still returned 5; quickjs-ng 0.16.2's `js_function_constructor` itself pastes and evals a wrapper, with an explicit TODO about accepting `Function("}), ({")`. Recommendation: do not add a parser to the confined worker merely for this shape property. If install/dry-run consistency warrants structural validation, decide that at the parent's install-time validation boundary, against the exact approved bytes.

### W5: stack provenance

QuickJS's public API has no stack-overflow origin flag or callback: `JS_ThrowStackOverflow` only constructs a normal RangeError. Its message and prototype are reproducible by a script. The parent explicitly asked to preserve this stack classification and not vendor/patch the engine.

The downstream difference is a reported failure label and detail, not control flow:

- `basal-core/src/driver.rs:1331-1333,1364-1365`: script rejection calls `fail("script", message, true)`; stack exhaustion calls `fail("budget_exhausted", "Stack", true)`.
- `basal-core/src/driver.rs:303-314` and `runs.rs:335-343`: both use the same terminal `Exit::Failed`, clear ownership, and persist `error_kind/error_detail`; neither retries or requeues the run.
- `basal-core/src/ops.rs:109-140`: flow health exposes the differing last-run `error_kind`, but both increment the identical consecutive-failure counter. Only readiness/retirement exceptions are exempt.
- `basal-core/src/rate.rs:168-208`: auto-disable and re-enable cards depend on saturated run/dispatch windows, not script-versus-budget labels. Both paths have the same consequences here.
- `basal-module/src/engine.rs:314-316`: the same failure logging branch prints differing kind/detail. The failure path records no separate label-dependent audit row or decision card; `driver.rs:294-314` writes only the common run exit (call-audit writes are separate dispatch paths).

Thus a script can mislabel its own terminal failure and health/log detail, but cannot obtain different retries, failure-streak treatment, auto-disable, audit decisions or card authority. Recommendation: defer the engine dependency/ABI decision rather than vendor QuickJS solely for this label; memory/time provenance and getter execution are fixed without that dependency.

## Verification

- Cargo 1.99.0 / rustc 1.99.0; clippy 0.1.99 (b940084d7e 2026-09-28); ckdev-mutate 0.8.0; Python 3.9.6.
- `cargo fmt --all --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed; 7 workspace packages / 77 declared targets.
- `cargo clippy --workspace --all-targets --features basal-module/rig-kill-hook -- -D warnings`: passed; same scope.
- `env -u BASAL_WORKER_BIN -u BASAL_CUT_EXHAUSTIVE cargo test --workspace --locked`: passed, 454 tests. The earlier failure of the pruning fixture was an intentional W6 contract conflict, resolved by the parent-authorized update, not a baseline failure.
- `cargo test -p basal-testkit --locked --test journal_admin`: passed, 5 tests.
- `ckdev-mutate prove ... --catalogue mutations.toml`: all 9 new controls CAUGHT with exactly one named red test each; other tests in each target stayed green. Source application was independently observed with an indexed empty/non-empty/empty diff-stat sequence, followed by checkout and touch restoration.
- `python3 script/check-path-deps.py`: passed, checked the 7 local workspace packages; this guard is silent on success. `python3 -m unittest script.tests.check_path_deps`: passed, 5 tests.
- `python3 script/check-ckdev-names.py`: passed, 88 files / 0 violations.
- `aft_inspect`: partial, analyzer indexing/call graph unavailable. Cargo clippy/test builds provide the authoritative Rust diagnostics.
- Catalogue check and replay of the two mechanically re-anchored existing controls are recorded in the final delivery once complete. Reports remain in gitignored `target/mutations/`, not committed as stale machine output.
