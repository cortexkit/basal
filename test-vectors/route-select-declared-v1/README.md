# route.select declared consumers v1 (routing 0.7.22)

This set uses separate request/reply files, like `route-report-attempt-v1`, rather
than the combined files of `route-head-choices-v1`. Each `<stem>.request.json` is
the compact `route.select` handler body with `method` and `params`; its
`<stem>.reply.json` is the Response frame body with the `result` envelope, or the
Error frame body with top-level `code`, `message` and `detail` for a refusal.
Neither JSON file has a trailing newline. The bytes are captured through the
real routing handler, without normalizing member order or generated fields.

The stand-ins are the routing conformance model catalog, vault accounts and
runner declarations, extended with the declarations below. Each pair gets a
fresh store, a clock fixed at `1800000000000` milliseconds since the Unix epoch,
and replay IDs starting at zero. The subscription multiplier is fixed at 22.
These are illustrative catalog rates, not current provider quotes.

Production routing advertises `consumer_declarations`, `decision_harness`,
`select_eligible_routes` and `select_price`. Capture stops if any is absent;
no sample uses a test-only capability override. Every answer passes the pinned
crate's `check_answer_serving` for its request, resolving the declared IQ default
before checking the Magic Context answer. Refusals decode as the crate's typed
`Refusal` and assert the expected code and reason without wrapping the Error body
in a `result` or `error` envelope.

| Filename stem | Declaration / request | Answer |
| --- | --- | --- |
| `basal-flow` | Basal declares `basal.flow` with `placement: {module: "broca"}` and no demand default. Per-call IQ 50, `tools: "none"`, and exact `eligible_routes` narrow the call. | Broca, even though another runner appears first in settings. No `runner.harness`: today's harness-less Broca shape. |
| `magic-context-summary` | Magic Context declares `magic-context.summary`, IQ 40 and `placement: "caller_runner"`. The request omits IQ and sends `tools: "none"` and `runners: ["broca"]`. | The non-runner attested caller `reserved:magic-context` resolves to the single catalog runner Broca. Resolved IQ is 40. |
| `price-metered` | Pin `example-provider/model-a`. | `basis: "metered"`, catalog list rates, no multiplier. |
| `price-subscription-5m-1h` | Pin `example-subscription/model-c`, with twenty completed tasks in real usage history for `example_summary` at IQ 50. | `basis: "subscription_equivalent"`, multiplier 22, `5m` / `1h` cache-write lifetimes, and `estimated_task_usd: 0.009272727272727273` together. |
| `price-subscription-30m-12h` | The same subscription route, with different stand-in catalog rates. | Multiplier 22 and `30m` / `12h` lifetimes, preserving catalog lifetime names. |
| `price-unpriced` | Pin the metered route with all catalog rates removed. | No `price`, not a zero-price quote. A dollar-capped caller must refuse to send without a price. |
| `decision-harness` | Request `harness: "sample-harness"` on a runner declaring that harness. | `runner.module: "sample-harness-runner"`, `runner.harness: "sample-harness"`. |
| `broca-harness` | The same Basal demand as `basal-flow`, with Broca's real after-slot installed verbatim from `crates/striatum-module/src/routing_v1/runners/broca-runner-slot.after.json`. | `runner.module: "broca"`, `runner.harness: "broca"`. This is the shape core sees once Broca switches its harness facts on. |
| `caller-runner-unresolved` | Magic Context's declared `caller_runner` consumer, selected by its owner with `runners: []`. | Error frame: `no_route`, `detail.reason: "caller_runner_unresolved"`, no `reset_at_ms`. |
| `consumer-not-owned` | The same owned consumer, selected by `reserved:unrelated-caller`, which is neither the owner nor a declared selector. | Error frame: `consumer_not_owned`. |

The short-lifetime catalog quotes input 4, output 20, cache read 1, cache write 5,
and cache-write TTL rates `5m: 5`, `1h: 8` USD per million tokens. Metered rates
stay at list; subscription rates divide every bucket by 22. The long-lifetime
catalog quotes input 22, output 110, cache read 11, cache write 44, and TTL rates
`30m: 55`, `12h: 88`, again divided by 22. The price pairs use `example_summary`, the
stand-in summary consumer supplied by the routing conformance fixture; the
declared-consumer pairs exercise admission and placement directly.

The short-lifetime subscription pair records twenty completed tasks with 1,000
fresh input tokens and 10,000 output tokens each. It reads the store's closed-task
usage, builds a snapshot with routing's real profile refresh builder, and publishes
that snapshot into the live cache through a test-only publication helper. The
request still passes through the real frame handler and its live profile lookup;
no profile callback is substituted. Its estimate is `(0.004 + 0.2) / 22` USD using
the quoted rates. An empty snapshot is published at the end of the pair, including
on assertion failure, so no later sample inherits its history or estimate. The
other pairs have no token history and carry no `estimated_task_usd`.

`caller_runner` resolves an attested `reserved:<module>` caller to that module
when it is a catalog runner. Otherwise it resolves the single `runners` entry,
which must name a catalog runner. The Magic Context pair exercises the latter
case: the attested caller is not a runner. This identity comes from the bound
route, not from any request-body claim.

The annotated tag for this set is `route-select-declared-vectors-v1.0`, following
the `route-head-choices-vectors-v1.1` naming scheme. The maintainer sets it on
merged main during deployment checks. This slice does not create the tag.

## Regeneration and verification

From the worktree root:

```sh
UPDATE_ROUTE_SELECT_DECLARED_VECTORS=1 cargo nextest run --locked -p striatum-module --bin ck-striatum -E 'test(=routing_v1::select::declared_samples_tests::serving_bytes_match_tagged_vectors)'
```

For a remote build with read-only synced sources, run regeneration in a writable
scratch copy. Copy the twenty JSON files and `SHA256SUMS` to a new output
directory, return those paths, then install the files here locally. Only the
update environment variable permits writing generated files; it does not rewrite
this README. Without it, the same test generates samples in memory and fails on
any byte or digest difference, and rejects unlisted files by name.

`SHA256SUMS` covers all twenty JSON files and this README in bytewise filename
order, excluding only the manifest itself. Independently verify it with:

```sh
(cd test-vectors/route-select-declared-v1 && shasum -a 256 -c SHA256SUMS)
```
