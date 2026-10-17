# work.unsettled v1

`work.unsettled` lists every delivered work item that its owning head has not
settled yet. Basal's board-hygiene flows call it to nudge owners who leave
deliveries unsettled; the flow applies its own age threshold.

These files are captured by
`work_unsettled::tests::work_unsettled_vectors_match_live_replies` in
`crates/prefrontal-core-module`. Each scenario runs against a fresh store with
one live head, `ag_owner` at session `ses_owner`, which launched every worker.

## Request

`{stamp: {flow_id, flow_version, code_hash}, cursor?}`, the same stamp
`board.read` uses, sent by the daemon-attested `reserved:basal` carrier. Unknown
fields are refused. Core admits the call only when:

- the stamp names an active install whose approved code hash matches;
- the approved audience is `{"kind":"global"}` (the reply spans every head's
  work, so an agent or workspace audience, a flow with no audience, and every
  package instance is refused with `work_unsettled_audience_not_global`);
- the approved manifest lists `{"module":"prefrontal-core","op":"work.unsettled"}`
  in `ops` (otherwise `work_unsettled_not_granted`).

Other refusals: `sink_principal_refused` (not `reserved:basal`),
`sink_flow_not_installed` (unknown, unapproved or revoked stamp),
`sink_flow_hash_mismatch`, and `sink_item_invalid` (a malformed body or a cursor
that `work.unsettled` did not return).

## Reply

`{items: [...], next_cursor?}` with at most 200 items. Pass `next_cursor` back
as `cursor` for the next page; it is absent on the last page. Each item:

- `agent_id`: the live head agent that owns the session that launched the
  worker, or `null` when no single live head holds that session any more.
- `item_id`, `title`.
- `delivery`: the value `work(action="list", waiting=true)` prints:
  `unmerged`, `merged`, `none` or `unknown` (no Git facts were available, for
  example because the repository is gone or the Git budget ran out). Core does
  not emit `error` today; a reader should still accept it.
- `merged_ref`: the integration ref containing the delivery, only when
  `delivery` is `merged`.
- `delivered_at_ms`: when the execution ended; the waiting list's age is
  measured from it.
- `campaign_id`: present for campaign items only.
- `blocks`: campaign items with a `blocks` dependency on this item that are
  neither done nor cancelled, as `{item_id, title}`, ordered by `item_id`.

`merged_at_ms` is never sent: core does not record when a delivery was first
seen contained in the integration ref.

Within a page, items follow the waiting list's order (unmerged first, then
failures, merged, and the rest, each oldest first); pages advance by delivery
time.

## Files

- `request-global.json`: an accepted request from a global-audience flow.
- `reply-merged-blocked.json`: a merged campaign slice whose next slice still
  waits on it. A done dependent and a worker that never delivered are not listed.
- `reply-unmerged.json`: a standalone delivery whose branch is not merged.
- `reply-empty.json`: nothing to settle.
- `reply-page-1.json`, `request-page-2.json`, `reply-page-2.json`: 201 delivered
  items without Git facts, read as a full page of 200 and a last page of one.
- `request-non-global.json`, `refusal-non-global.json`: a workspace-audience
  flow that declares the op.
- `request-unknown-stamp.json`, `refusal-unknown-stamp.json`: a stamp naming no
  approved install.
- `SHA256SUMS`: hashes of every JSON file above (UTF-8, trailing newline).

Refusals are `{code, message}`, the error body on the wire.

To recapture, run:

```
UPDATE_WORK_UNSETTLED_VECTORS=1 cargo nextest run -p prefrontal-core-module --lib -E 'test(work_unsettled_vectors_match_live_replies)'
```

The ordinary test compares every reply and hash without writing.
