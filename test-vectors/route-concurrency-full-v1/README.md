# Concurrency-full refusals on routing/v1

These request/reply pairs are captured through routing's real frame handler
using the synthetic account-class fixture and the pinned routing contract.
Requests contain `method` and `params`; replies are Error frame bodies with no
`result` or `error` envelope. JSON bytes are preserved without a trailing newline.

Each capture starts with a fresh store and deterministic replay IDs. At epoch
millisecond `1800000000000`, the runner reserves the one available slot on each
of two eligible synthetic accounts through `route.account_for`. Enforcement is
enabled only in this fixture; the production default remains off. The clock then
advances to the fixed refusal time `1800000048000`. Both reservations expire at
`1800000060000`, so the reset is twelve seconds away, not the thirty-second
fallback. The quota reading remains fresh and neither account is blocked.

* `select.request.json` / `select.reply.json`: the owner of the worker consumer
  `mason` asks for a route. All adequate routes are full. The answer is `no_route`
  with reason `concurrency_full`, the reset time and the rejected route list.
  The complete diagnostics also retain seeded catalog routes rejected because
  they have no eligible account; only the synthetic route has eligible accounts.
* `account-for.request.json` / `account-for.reply.json`: a runner asks to admit a
  new conversation. The answer is `no_route` with reason `concurrency_full` and
  exactly the allocator's `reset_at_ms`. A separate legacy call in the test
  confirms the same reset. Routing emits no `is_wait` member: the pinned
  `cortexkit-role-routing` contract derives wait classification from the
  `concurrency_full` reason.
* `basal-flow-select.request.json` / `basal-flow-select.reply.json`: attested
  `reserved:basal` selects its declared consumer `basal.flow`, with Broca as
  its placement and without a request `detail` member. The request uses Basal's
  `requirements.iq`, `requirements.tools` and `eligible_routes` fields, with the
  single synthetic route substituted.
  The fixture removes all seeded model facts except `synthetic/model`; its two
  synthetic accounts are filled via the same real admissions as above. This keeps
  the public sample free of real catalog routes, including in refusal diagnostics.

## Regeneration and verification

From the worktree root, with writable sources:

```sh
UPDATE_CONCURRENCY_FULL_SAMPLES=1 cargo nextest run --locked -p striatum-module --bin ck-striatum -E 'test(=routing_v1::concurrency_samples_tests::handler_samples_regenerate_byte_for_byte)'
```

For remote builds with read-only synced sources, regenerate in a writable scratch
copy and return the six JSON files and `SHA256SUMS`, then install them locally.
The environment variable enables writes only for generated files, not this README.
Without it, regeneration happens in memory and any byte difference, missing file,
unexpected file or manifest drift fails the test. The three additional handler tests
check the refusal semantics independently of the recorded files.

`SHA256SUMS` covers the six JSON files and this README, in bytewise filename
order, excluding the manifest itself. Independently verify from the worktree root:

```sh
(cd test-vectors/route-concurrency-full-v1 && shasum -a 256 -c SHA256SUMS)
```
