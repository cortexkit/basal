# Staged run-log admission

`run_admission::RunAdmission` compiles in ordinary builds, but its only
constructor is behind `cfg(test)` / `test-support`. `Runtime` and `Scheduler`
continue selecting the original admission functions. No numbered migration or
production schema initialization changes here.

## Durable field mapping

This maps every write in the original `admission::insert_run` (including the
follow-up input updates) to the staged schema:

| Original admission field / write | New home |
| --- | --- |
| `runs.run_id` | Full `run-{store_id}-{n}-{r}` primary key in `run_identities` and `live_runs`; the identical full ID is stored in `trigger_inbox` and returned by dedup/tombstones. No numeric-prefix lookup is introduced. |
| `flow_id`, `flow_version` | Both `run_identities` and `live_runs`, constrained by their composite foreign key. |
| `snapshot_id`, `code_hash` | `run_identities`; the retained `run_snapshots` row stores script and manifest once per approved version, as before. |
| `trigger_id`, `attempt`, `trigger`, `admitted_at`, `admit_seq`, `deadline_ms` | Same-named typed/JSON columns in `live_runs`. `trigger_kind` is explicitly `event` or `schedule` for these two entry points. |
| `state = pending` | Existence in `live_runs` means admitted and unfinished. Execution state and claiming belong to the RunStore actor; admission writes no execution log record. |
| `self_input` | `live_runs.self_input`: the same package-flow `{"agent_id": owner}` snapshot, or JSON null for non-package flows. |
| `params_input` | `live_runs.params_input`: parsed JSON from the current `template_instances.params`, or JSON null when absent. |
| `grant_guards` | `live_runs.grant_guards`: parsed JSON from `template_instances.grant_guards`, or JSON null when absent. |
| `template_incarnation` | `live_runs.template_incarnation`, nullable integer. |
| `end_instance` | `live_runs.end_instance`: true only for a template instance whose approved manifest has a text `trigger.schedule.at`, preserving the original CASE expression. |
| `trigger_inbox` row | Unchanged table, with flow/trigger/attempt/full run ID/admission time, in the same transaction. |
| `meta.next_run` | Unchanged counter key. The staged allocator checks positive integer range/exhaustion and increments it transactionally; it draws eight bytes from `getrandom` for every new identity. The entropy suffix is fixed-width 16-digit hexadecimal. Deduplication allocates neither a counter nor entropy. |
| State-epoch reset | The existing `state_epoch::reset` writes `kv`, `flows.state_epoch`, `last_state_reset_at` and `state_resets` in the admission transaction. The staged gate checks `live_runs` rather than the old run table for held slots. |
| New recovery anchor | `live_runs.admit_epoch`, `admit_segment`, `admit_offset` encode the current published synced LSN. Each batch and each inserted run also advances `meta.log_hwm`. |

When RunStore commits a run's terminal state, it transfers admission metadata
to `run_history`; identity rows remain with the history. Admission itself
creates no rows in `run_history`, `run_calls` or `obligations` (the table for
late work of finished runs).

## Batch and integration contracts

- `admit_events` performs matching, fan-out, rate charging, receipts and backlog
  placement in one fenced `Store::write`. `acknowledge` runs only after the
  commit and the `AdmissionCommitted` crash boundary. An acknowledgement failure
  leaves durable receipts for normal deduplication on redelivery.
- `EventWakes` is the host-integration seam: wake matching writes share that
  transaction, and `committed` runs after commit without a held connection.
  Tests use a no-op implementation; binding the registry-aware Broca wake path
  (for long-running model calls) belongs to host integration. No production host is attached to this path.
- `AdmissionLog` reads an already-published epoch-qualified watermark; it must
  never initiate or wait for a sync or acquire SQLite. The log-binding owner
  supplies this adapter. Epoch or store-ID mismatches abort the whole batch.
- `tick_at` performs expiration, old-fire admission, due-schedule planning and
  advancement, and new-fire admission in one fenced write transaction. Shared
  schedule planning and fire disposition preserve the old ordering and drop
  policy. `Draining` leaves planned fires intact while planning continues;
  `StateBusy` blocks only its own flow during each admission pass.
- Each event/flow offer, backlog entry, schedule plan and scheduled fire has a
  savepoint. Fire-local invalid/corrupt data rolls back its writes and records
  an audit (and, for schedules, the existing drop/disable disposition). Store,
  entropy I/O, simulated crash and fencing failures escape and roll back the
  entire batch. Rate refusals are values, not storage errors: their saturation
  accounting and the existing queue/drop policy commit as before.
- `drain_event_backlog` remains bounded to 128 receipts and preserves per-flow
  arrival order. It uses the same staged allocator, gate and live tables.

## Admission crash boundaries

`AdmissionWritten` cuts inside a batch after the run identity, live row, counter,
state reset and trigger dedup have been written, but before commit. A cut rolls
back every fire in that batch. `AdmissionCommitted` cuts after the fenced commit
but before any receipt acknowledgement; redelivery finds the committed receipts
and does not create another identity or charge admission twice. The admission
crash test enumerates both variants and reopens the store at each cut. Dispatch,
claims, deadlines set when runs are claimed, broken counts and execution budgets are not
mutated by admission and remain the other lifecycle stages' crash obligations.
