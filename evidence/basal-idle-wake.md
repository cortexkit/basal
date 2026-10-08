# Idle engine and consent measurement

The opt-in `basal-module` test `production_idle_cpu_before_and_after` launches the production `ck-basal` executable against a test daemon that authenticates it exactly as the production end-to-end test does (`crates/basal-module/tests/e2e.rs`). It places a real sandboxed worker beside the module under development executable names. The fake core answers empty elicitation pages immediately; there are zero flows and no open cards. Both samples use debug binaries, exclude five seconds of startup, and span 120 seconds.

The baseline is built from commit `efa26d7d7c88424a26dfcdda417ab8a32f436e15`. The result's raw `ps -p PID -o time=` readings and consent answer RPC counts are in `basal-idle-cpu.json`:

| Module | CPU time at start | CPU time at end | CPU time increase | Consent answer RPCs |
| --- | --- | --- | --- | --- |
| Before | 0:00.09 | 0:00.62 | 0.53 seconds | 470 |
| After | 0:00.07 | 0:00.07 | No measurable increase at hundredth-second resolution | 0 |

CPU readings are measurements, not test thresholds. The deterministic tests instead count passes and RPCs, exercise the actual work notifications, and advance the manual clock by the production loop's selected timeout. Event observers keep their existing 60-second hang bound, while event tests use a ten-minute fallback. Run wall-clock deadlines are not reduced.

One unconditional answer-page reconciliation is retained at startup, even with no open cards. It finishes an acknowledgement interrupted after applying the final answer. If applying a page of answers or acknowledging it fails, the poller keeps polling until a whole page is applied and acknowledged, even if no card is open any more. Otherwise, with no open cards it makes no calls.

## Ordered timer indexes

The schedule MIN already uses the existing `schedules_due` covering index. The deadline query takes three scalar MINs, one per live state, so each can stop at the first entry of an ordered state/deadline index. The deferred query walks retry order and checks run liveness by primary key, excluding completed runs.

`runs_state` and `journal_deferred` can narrow the live cohorts but cannot supply deadline or retry ordering. Store migration 13 therefore adds two ordered indexes, so computing the next wake never visits every live deadline or deferred call:

```sql
CREATE INDEX runs_state_deadline ON runs(state, deadline_at)
WHERE deadline_at IS NOT NULL;

CREATE INDEX journal_deferred_retry ON journal(retry_not_before, run_id)
WHERE dispatch = 'deferred' AND retry_not_before IS NOT NULL;
```

`maintenance::tests::next_schedule_uses_the_due_index` and `maintenance::tests::timer_plans_use_the_ordered_wake_indexes` check the actual query plans against the fully migrated schema.

## In-memory timers

The next-wake calculation folds in retention's next pass (including its one-second backlog cadence), install-gate backoff, pool spawn retry, and bound-worker retirement. Pending runs are notified rather than included as immediate timers, so a full activation pool cannot turn the loop into a busy wait. A worker that dies while idle is noticed by the engine's 30-second fallback pass.

The existing `elicitation.await` request accepts `timeout_ms` (basal currently sends zero to retrieve expired records). This indicates a possible long-polling surface, but no answer notification or long-polling behavior is introduced here; answering still uses pages and apply-before-ack.
