# Flow notices v1

Capability: `flow-notice/v1`.

These samples are captured from core's real `sink.notice.send` and `sink.notice.resolve` handlers, the app `notice.list` handler, `notice.watch` on an attested phone route, and the phone push plaintext builder. They are for BASAL, CKIOS and CKDESK. Titles, bodies and resolutions are synthetic sample text.

## Contract

Only the attested `reserved:basal` carrier can call either sink. Every request carries a `stamp` with `flow_id`, `flow_version` and `code_hash` matching the approved install. The hashed manifest grants `{"kind":"notice","severity_max":"attention"}` (or `info`) in `sinks`; its audience must be its author agent. The notice's durable sender is `{"kind":"flow","id":"<flow id>","subject":"<author agent id>"}`. App rows expose this as `senderKind`, `senderId` and `subjectAgentId`; push plaintext uses `sender.kind`, `sender.id` and `sender.subject`.

`key` is required for both ops: 1–128 ASCII letters, digits or `._:-`. It is scoped to the flow, never to its author or another flow. `clientId` is required: 1–128 printable ASCII bytes. BASAL supplies its receipt key. Repeating it for the same flow returns the first successful reply without another notice, update, resolution or rate-limit charge. Receipt keys must be unique across send and resolve calls. Authorization is checked again on replay; a revoked install cannot replay. Failed calls do not claim a receipt.

Core never accepts `urgent` from a flow, including when an approved manifest claims an urgent cap. An `attention` request against an `info` grant is a different refusal. Flow notices count against the author's shared daily budget and use `notify`'s demotion to `info` after the budget is exceeded. Each sender/key retains its own charge-reuse window.

Refusal codes are final, except `sink_store_failed` (transient store error). Relevant codes include `sink_principal_refused`, `sink_flow_not_installed`, `sink_flow_hash_mismatch`, `notice_not_granted`, `notice_requires_agent_flow`, `notice_grant_invalid`, `notice_urgent_refused`, `notice_severity_above_grant`, and `notice.*` validation/target refusals. Match on the code; the message is prose for the flow's author.

App cards and push payloads name the sender as `<author agent name> (flow: <manifest title, or the flow id when the manifest has no title>)`, for example `TEST (flow: Nightly build check)`. Attention notices use the same hourly phone summary as agent notices; when an hourly summary holds just one notice, it is sent as a single `notice` payload. The phone sample here is that single-notice case, before sealing/encryption.

## Files

- `flow-notice.json`: send, same-client replay, resolve and resolve replay, urgent/above-grant/hash/no-grant refusals, app read row, and phone push payload.
- `notice-watch.json`: real watch resync and change frame containing the flow sender. Clocks, cursors and journal counters use the existing watch sample normalizers.
- `SHA256SUMS`: hashes of both JSON files and this README.

The unit tests additionally cover update/reopen, sender isolation, revocation, author-budget sharing in both directions, global/operator refusals and hourly summaries.

## Regenerate

From the repository root, using a local build directory on the internal disk:

```sh
CARGO_TARGET_DIR="$HOME/.cache/ck-flow-notice-samples" RECORD_FLOW_NOTICE_SAMPLES=1 nice -n 10 cargo nextest run --locked -p prefrontal-core-module --lib --bins -E 'test(/flow_notice_real_handler_samples_and_app_push_builders$/) | test(/flow_notice_watch_real_handler_sample$/)'
(cd test-vectors/flow-notice-v1 && shasum -a 256 README.md flow-notice.json notice-watch.json > SHA256SUMS)
```

Remove the temporary build directory after regeneration. Ordinary verification runs remotely without `RECORD_FLOW_NOTICE_SAMPLES` and compares the handler results with these files.
