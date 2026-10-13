# Repository flows

These are operator-installed flows, not templates or automatically installed services.

## Board hygiene

`board-hygiene/manifest.json` and `board-hygiene/flow.js` define one global flow
that scans live boards every five minutes through core's `board.read` operation.
It sends per-item digest nudges for ended waits, overdue ETAs, active items left
unchanged while their owner is active, and completed lanes awaiting retirement.
It uses activity facts only for candidate stale items. No network or LLM is used.

The flow keeps bounded, sharded indexes of its per-rule KV entries so it can
remove memory for vanished agents, lanes, and items. If memory is full, it skips
unrecordable nudges rather than sending reminders it cannot rate-limit. Digest
refusals are retained in a bounded run-result log without interrupting the rest
of the scan. The manifest's four-minute deadline fits within the five-minute
schedule. Run and dispatch rate limits are configured by the operator on basal's
runtime (`basal_core::Config::rate`), not as manifest fields.

Installation and audience selection belong to the operator. The offline tests
in `crates/basal-module/tests/board_hygiene.rs` exercise the shipped script and
validate its manifest with basal's parser.

Reminders use a recipient-scoped digest replacement key when core supports it.
If core does not advertise replacement support, basal refuses the keyed write
before dispatch with `digest_replace_key_unsupported`. The flow retries the
same reminder without the key and records the fallback once in the run-result
log. Keys over 200 UTF-8 bytes are omitted. KV repeat throttling remains active
in both modes.
Item nudges carry current state and wait metadata; one-time copies also carry
`own_*` identifiers for the waiting owner's item so the renderer targets the
item to re-plan rather than the peer's item whose ETA passed.
