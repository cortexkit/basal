# macOS responsibility calibration needs a live originator

## Finding and limits

macOS can use a **responsible process** to choose whose privacy (TCC) grants
apply to a child. A normal child generally shares that process identity; a
**disclaimed** launch deliberately gives the child its own identity instead.
Here, an **originator** is the ancestor whose responsible identity was inherited.

`responsibility_get_pid_responsible_for_pid(parent) == parent` does not, by
itself, establish an identity that a plain child will inherit. When an
explicitly responsible ancestor exits and is reaped, its plain descendant and
that descendant's plain `/bin/cat` can each report themselves as responsible.
`privacy::production_worker_is_its_own_responsible_process` in
`crates/basal-module/tests/e2e.rs` originally compared those two different PIDs
and failed, even after cat's echo and device/inode exec barrier.

GitHub run **38659849713** reproduced that assertion in **300/300**
exited-responsible-originator iterations, including **200/200 uninstrumented
full-e2e iterations**. All **300/300 live-originator iterations** passed under
the same load. No sampled identity changed over time. Waiting for inheritance
to appear would not resolve the reproduced state.

This establishes an originator-lifetime mechanism, not the private SPI's
internal implementation. The two historical local failures did not capture
their entire responsible ancestry. An ordinary parent exiting while its own
responsible ancestor remains alive did **not** reproduce the problem in the
CI comparison. Reparenting alone, CPU pressure alone, asynchronous attribution,
and a random Rust spawn-path switch are not established explanations for the
historical events.

## Experiments

All stress ran on GitHub-hosted macOS runners, not the shared development Mac.
Each stress run used one `yes` process per core and twice as many parallel
subprocess loops as cores. Each loop ran 100 iterations, alternating complete
e2e binaries and the exact privacy test. A concurrent
`cargo test --locked --workspace` added workspace-test activity.

| GitHub run | Setup and result | Loaded elapsed |
| --- | --- | --- |
| [38652403364](https://github.com/cortexkit/basal/actions/runs/38652403364) | Invalid diagnostic run: 600 iterations failed before calibration. Injecting an arm64 interposer into arm64e `/bin/cat` aborted dyld. No attribution conclusion drawn. | 461.084 s |
| [38654123849](https://github.com/cortexkit/basal/actions/runs/38654123849) | 600/600 passed: 300 uninstrumented full-e2e, 300 observer-instrumented exact tests. All observers inherited the runner agent's identity. No changes across 3,000 control-echo and 5,590 worker-image samples. | 1,445.088 s |
| [38657149669](https://github.com/cortexkit/basal/actions/runs/38657149669) | 1,000/1,000 passed with explicitly self-responsible live observers. Another 5,000 rapid controls passed: 2,500 default Command spawns and 2,500 forced fork/exec spawns. No spawn/echo mismatch or sampled transition. | 1,322.055 s |
| [38659849713](https://github.com/cortexkit/basal/actions/runs/38659849713) | 300/300 exited-responsible-originator cases failed; 300/300 live-originator cases passed. Primary uninstrumented full-e2e split: 200 exited cases failed, 100 live cases passed. | 1,241.287 s |
| [38662884865](https://github.com/cortexkit/basal/actions/runs/38662884865) | Live-anchor fix: 600/600 passed, 200 each for live-self, exited-responsible-originator, and ordinary inherited-parent-exit launch modes. Workspace passed. Both disclaimed-cat and plain-worker mutation controls failed only the expected outer test; five sibling tests passed. | 2,344.792 s |
| [38667184845](https://github.com/cortexkit/basal/actions/runs/38667184845) | Final diagnostic-free fixture: 600/600 uninstrumented iterations passed (300 full-e2e, 300 exact tests), 200 per launch mode. All three mutation controls caught only the outer test; five siblings passed for each. Workspace: 1,336 passed, zero failed, two ignored across 136 libtest summaries. | 2,636.390 s |

The final run used source commit `16eceb11`, macOS 26.6.2 (25G83), Rust 1.99.0,
Cargo 1.99.0 and Python 3.14.7. It also passed native e2e clippy and a clean
`cargo test --locked -p basal-module --test e2e`: six passed, zero failed, one
ignored (the inner helper, run by the outer test).

The first valid run recorded 300 actual `/bin/cat` calls through libc
`posix_spawn`; the later forced-fork comparison registered an empty `pre_exec`
hook deliberately. Neither path failed with a live explicit originator.
The DYLD spawn-entry-point interposer was removed from every measured cat, and
later from the measured probe subprocess itself. The final test contains no
instrumentation, sampling waits, environment switches, or pressure helper.

### Quoted reproduction

Uninstrumented `orphan-full-2-35.log` from run 38659849713 records an explicitly
self-responsible ancestor launching the observer that outlives it:

```text
ORIGINATOR_PROBE pid=29081 responsible=29081
ORIGINATOR_PROBE child=29082 result=0
ORPHAN_PROBE observer=29082 responsible=29082 originator=29081
```

The observer had waited for `kill(29081, 0)` to report `ESRCH`. The cat it
launched, PID 29583, remained self-responsible at all measured times:

| Observation | Elapsed from spawn | Observer responsible PID | Cat responsible PID |
| --- | ---: | ---: | ---: |
| Immediately after spawn | 1.350 ms | 29082 | 29583 |
| After echo and image barrier | 3.765 ms | 29082 | 29583 |
| Later samples | 6.403 / 19.606 / 122.040 / 1,125.490 ms | 29082 throughout | 29583 throughout |

```text
assertion `left == right` failed: plain control inheritance: parent=29082 child=29583
  left: 29583
 right: 29082
```

## Deterministic fixture

The outer test now uses the production `ck-basal` trampoline to launch an
explicitly disclaimed copy of its own e2e executable. It selects exactly one
ignored probe test, confirms exec, and retains ownership until the probe exits.
The probe thus has a known, live responsible identity (the **anchor**)
throughout calibration, module startup, and worker measurement, independent of
the caller's ancestry.

All original calibration and worker assertions remain: the plain cat must
inherit the live identity and must not report itself; the actual worker must
report itself, not the module or the module's responsible process. A plain
worker therefore has a distinct live identity to inherit and cannot pass by
individually reporting itself after a missing ancestor. No assertion retry or
responsibility-settling sleep was added.

The inner helper is ignored only to prevent ordinary libtest discovery from
running it without the required launch. The outer test always runs it, and the
mutation controls exercise both its calibration and worker assertions. Nested
output is prefixed so the mutation runner can distinguish the inner helper's
diagnostics from the outer target's test results.

## Retained controls and raw evidence

The macOS mutation catalogue in `mutations.d/module.toml` retains these faults
that the privacy test must detect:

- `plain-privacy-calibration-disclaims-child`: actually give the calibration cat
  its own responsible identity instead of an inherited one;
- `production-worker-inherits-parent-privacy-identity`: launch the worker without
  disclaiming, so it borrows the module's responsible identity;
- `privacy-probe-borrows-inherited-originator`: replace the outer disclaimed
  launch with an ordinary Command and remove its exec confirmation.

CI mutation witnesses stage each live source path, require an empty unstaged
diff, apply the catalogue edit, capture a nonempty diff, run the e2e target,
restore with `git checkout -- <path>` and `touch`, and capture an empty diff.
Only `privacy::production_worker_is_its_own_responsible_process` may fail; all
five other active tests must pass. The control that removes the explicit live
identity from the observer additionally requires the actual
`plain control inheritance` assertion under an
  exited-responsible-originator launch.

The final run recorded these source changes, their restoration, and the
assertions each change triggered:

| Catalogue control | Applied diff | Restored diff | Failing assertion inside the selected helper |
| --- | --- | --- | --- |
| Disclaimed calibration cat | `e2e.rs`: 7 insertions, 1 deletion | empty | `plain control inheritance: parent=10610 child=10627`; left 10627, right 10610 |
| Plain production worker | `pool.rs`: 1 insertion, 3 deletions | empty | `worker must be its own responsible process`; left 11187, right 11204 |
| Missing explicit anchor | `e2e.rs`: 5 insertions, 8 deletions | empty | `plain control inheritance: parent=11803 child=12014`; left 12014, right 11803 |

For each of the three faults, the `--test e2e` target reported:

```text
test privacy::production_worker_is_its_own_responsible_process ... FAILED
test result: FAILED. 5 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out
```

The nested ignored helper failed the corresponding assertion, as intended;
its prefixed diagnostic output is preserved in the logs. The five independent
active siblings all passed. In this final witness an external launcher spawned
Cargo suspended, exited, and was reaped before Cargo was resumed, so the
anchor-removal control exercised a genuinely missing responsible ancestor
without any test-only environment handling.

The pinned mutation runner's cargo parser trims each line and recognizes a
test event only when the line starts with `test `; the nested prefix therefore
does not create a second target-level failure. See
[`parse_test_results` at the pinned revision](https://github.com/cortexkit/commons/blob/be120e9254c4972191b39b2e6814987918206afc/crates/cortexkit-mutate/src/lib.rs#L3037-L3106).

The final raw helper check without the outer launcher reproduced the plain
control failure after responsible-originator exit. The ordinary inherited-parent
case instead failed the helper's explicit-self precondition (responsible PID
979 versus helper PID 16288); that is **not** counted as a defect reproduction.
The outer test passed under both launch modes.

The temporary **Temporary macOS responsibility probe** workflow uploaded raw
logs and JSON summaries for the runs below as GitHub artifacts, not
build-directory files. Substitute the artifact ID from the table into REST
`GET /repos/cortexkit/basal/actions/artifacts/ID/zip`:

| Run and experiment | Artifact ID | Relevant paths in archive |
| --- | --- | --- |
| 38654123849, inherited runner identity | 11937406778 | `summary.json`, full/single iteration logs, cat launch/echo checks |
| 38657149669, explicit live identity | 11938531354 | `summary.json`, `pressure.log`, iteration logs |
| 38659849713, responsible-originator exit | 11939795680 | `summary.json`, `orphan-full-2-35.log`, other iteration logs |
| 38662884865, initial anchored fix | 11942855007 | `privacy-probe/summary.json`, `privacy-mutations/summary.json`, corresponding logs |
| 38667184845, final diagnostic-free fix | 11944597200 | `privacy-probe/summary.json`, `privacy-probe/workspace.log`, `privacy-mutations/summary.json`, corresponding logs |

The temporary workflows and external diagnostic drivers are removed from the
delivery tip; their diagnostic commits and CI artifacts identify the exact
commands used. The final source is integrated with main `4b481396`, whose
changes do not alter the privacy test or worker-launch implementation.
After integration, the portable e2e target passed in both modes (four tests
without the rig hook, five with it), and both macOS e2e clippy configurations
passed. No stress or privacy test execution ran on the shared Mac.
