# Staged RunStore lock contract

The production driver is unchanged until cutover. `RunStore::new_staged` is
constructed directly by tests; it is not selected by `Runtime` or admission.
The records and driver adapters supply `RunRecords`, `TerminalBuilder`,
`TerminalWrite` and `CloseDriver`. Accounting adapters supply `Budget` and
encode a metric/window in `Window`; these are not new operation/record kinds.
Token units and dollar microunits must not share a window key.

Order: **Broca per-send gate → actor → flow accounting → SQLite**. The debug
ordering token is thread-bound. Actor/accounting mutexes acquire it internally;
every `Store::read` and `Store::write` acquires the SQLite token, including reads
through legacy code. Broca adapters can use `lock_order::enter(SendGate)`.
`RunLog::sync_through` and saturation assert that SQLite is not held. Adapters
must use `assert_lock_free` before host, sink or notification calls.

## Entry points

Registry-map, flow-map and routing/closer bookkeeping are short internal locks.
Map lookups clone handles and release the map before taking an actor/accounting
lock. The sole reverse direction is close's removal of its registry entry; no
map holder waits on an actor, so removal cannot form a cycle.

| Entry point | Locks, in acquisition order | Lock-free work |
| --- | --- | --- |
| `insert`, `get`, `snapshot`, `Run::routing` | Registry/flow-map bookkeeping or atomic load only | None |
| `Run::activate` | Actor; validates generation **and** Open before exposing records | Dispatch/reply/host hooks after return |
| `Activation::records`, `dispatch_entrant` | Already-held actor; dispatch count registered in the Intent/resend critical section | Spawn dispatch with counted entrant after return |
| `Run::enter` | Actor; Open check and entrant increment together | Closing → retryable sink error; Closed → obligations adapter |
| `Entrant::finish`, entrant drop | Actor; recorded return allowed during Closing; final drop decrements and signals | Completion acknowledgement after record durability and guard return |
| `Activation::reserve` | Actor → accounting → SQLite read; records its reservation before accounting unlock | Dispatch only after records adapter syncs Intent |
| `Activation::settle`, `Entrant::settle` | Actor → accounting; entrant form records/syncs Arrived before replacing reservation | Host/sink notification after return |
| `Activation::saturate`, `saturate_dispatch` | Actor → accounting; append identity, sync **without SQLite**, then SQLite transaction, then refusal Arrived | Refusal hook/ack only after records adapter supplies required sync |
| `close`, steps 1–3 | Actor to bump generation/set Closing; release; stop/join worker; actor/condition drain releases actor while waiting | `CloseDriver::stop_worker` |
| `close`, steps 4–6 | Actor → accounting held continuously from terminal build through commit wait, accounting removal, Closed and registry removal | After unlocking, `CloseDriver::closed` releases slot, wakes next run and marks records reclaimable |
| Terminal committer | SQLite only; at most 25 ms to fill each group; one savepoint/run | Build→commit and commit→Closed hooks run on committer thread outside SQLite; it never takes actor/accounting |
| `accounting_write` (closed obligations/resolutions) | Accounting → SQLite; no actor | Acknowledgement/diagnostic after commit and unlock |
| `audit_at` (retention/token reader snapshot) | Flow-map lookup, release; accounting snapshot, release | SQLite transaction afterwards; reader revalidation afterwards |
| Admission, admission saturation, retrigger | SQLite only; retrigger only after Closed | Receipt ack after commit |
| Log registration/promotion | SQLite only on writer thread | Filesystem work/sync outside SQLite |
| Deadline expiry, retention, `decisions_due`, grant loss, secret revocation | Registry/accounting snapshot **before** SQLite, release snapshot locks; SQLite; revalidate afterwards | Control transition/card notification after revalidation |
| Health, `flow_health`, `run_ages`, diagnostics, `startable`, `wake_flow`, Broca identity/deadline/run-ended, token readers | Registry/accounting snapshot **before** SQLite, release snapshot locks; SQLite; revalidate afterwards | Host/sink calls outside locks |
| StateStore save adapters | Own short SQLite transaction; no actor | Sink call after commit |

The final rows are integration contracts for future implementation work, not
descriptions of the old runtime's behavior. An adapter must not read a registry handle and
then acquire its actor from inside a SQLite closure: debug builds reject that
inversion. Dispatch tickets abandoned inside their registering activation are
decremented by the activation before it releases the actor, avoiding a recursive
lock during error unwinding. A dispatched entrant finishes its record effect
only after the activation releases the actor, then drops its count normally.

## Closing and retry

The terminal builder captures all summary/result, overlay, accounting, obligations,
tombstones, non-success call detail, the owner-notice episode latch (which
prevents repeat health alerts), template effects and deferred
refusal/outbox work in one owned, retryable SQLite-only projection. Success is
rejected unless the records adapter reports all calls released. The committer
rolls back a failed savepoint alone and retries it in another group, while the
closer retains both locks. Whole-transaction/fence failures are returned, not
hidden. A stopped/failed close remains Closing; a subsequent close can resume
with no second generation bump. Concurrent closers are refused. A store cut by
crash injection rejects all subsequent operations; it must be recovered by a new
process, never resumed in place.

After a successful fold, live accounting is removed while the accounting lock
is still held, so another budget check can observe neither a missing charge nor
a double charge. Reservations retain their intent's `audit_at` through refunds
and settlement. The terminal adapter updates `commit_seq` (terminal commit
sequence) and `log_hwm` (the durable log high-water mark) along
with its other SQLite projections; the records/log binding adapters provide the
synced watermark, not a new SQLite read during a sync wait.
