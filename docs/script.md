# How a flow script runs

A flow script runs in `ck-basal-worker`, a separate process that confines
itself before it reads anything from basal. The script reaches the world only
through host calls: `sink.*`, `facts`, `ops.call`, `llm`, `classify`, `kv` and
the built-ins. basal checks each one against the approved manifest, in the
parent process, and journals it there. This note documents the digest item
shape, then two behaviours of the script runtime that are deliberate limits,
not guarantees.

## Module-event triggers

An event-triggered script receives `trigger` in this shape:

```json
{
  "event": {
    "module": "plexus",
    "name": "github_pr_changed",
    "version": 1,
    "event_key": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    "headers": { "kind": "github_pr_changed", "repo": "owner/repo", "pr": "42" },
    "body": { "action": "opened" }
  }
}
```

Before sending any script to the worker, basal calls the source's `events_get`
query on **that flow's scoped route**, with `{event_key, event_name}`. It hashes
the exact UTF-8 bytes of the returned body string with SHA-256 and requires
that hash and the reply's bare lowercase-hex digest to equal the notice's
digest. Only a notice's `sha256:` prefix is normalized; uppercase and other
spellings are refused. The verified body bytes are journaled before the body
is parsed as JSON and passed to the script. Replay reads those bytes and
never calls the source again.

An invisible or unknown body fails the run as `event_not_visible`; changed
bytes or inconsistent digests fail it as `event_body_mismatch`. Other source
refusals retain their code as the failure kind. An unavailable source defers
the preamble with durable exponential backoff until the run's deadline. No
script instruction runs before a body has been verified.

For a capture-mode dry run, supply the complete synthetic `{event: {...}}`
value above as the `flow.dry_run` request's `trigger`. The supplied `body` is
already a JSON value, not stored text; it is delivered without a live body
lookup or any other live call. Header labels are context for script logic,
not authority, and manifest header filters remain a follow-up.

## Digest items

`sink.digest(agent, item, action)` delivers `item` to the agent's digest
through prefrontal-core, which validates it strictly. `action` is `silent`,
`piggyback` or `wake`, capped by the manifest's `digest_max`. The item has
these four fields. `data` and `links` may be omitted, and then default to
empty. Core refuses a missing `title` or `body`, or any field not listed
here, with `sink_item_invalid`, and the refusal names the field:

| Field | Type | Limit |
| --- | --- | --- |
| `title` | string, not blank | 200 characters |
| `body` | string, not blank | 4,096 bytes |
| `data` | object, optional, default `{}` | 8,192 bytes as canonical JSON |
| `links` | array, optional, default `[]` | 8 entries |

A link is either `{ kind: 'url', url: 'https://…' }`, an HTTPS URL with a
host, or `{ kind: 'work', id: 'wi_…' }`. The whole item may be at most 16,384
bytes as canonical JSON.

```js
await sink.digest('BASAL', {
  title: 'basal nightly CI: failure',
  body: 'Commit 0123abc, started 2026-10-09T03:17:00Z.',
  data: { run_id: 123 },
  links: [{ kind: 'url', url: 'https://github.com/cortexkit/basal/actions/runs/123' }],
}, 'wake');
```

## The script body is not a syntactic boundary

basal places the script text inside an async function before evaluating it
(`crates/basal-worker/src/engine.rs`, the `(async function () { ... })`
wrapper). That wrapper is built by joining text. A script can close the
function early and put statements outside it. Those statements run when the
source is evaluated, just before the flow function starts.

They gain nothing. They run in the same confined worker, under the same
lockdown, memory budget and JavaScript CPU budget as the rest of the script.
Any host call they make is checked and journaled like any other. The code
hash, and so the consent card, covers the script's exact text, so the
approved code is exactly what runs. Treat the wrapper as a convenience for
`await` and `return`, not as a sandbox boundary.

## A script can make its failure look like a stack overflow

QuickJS reports call-stack exhaustion as a plain `RangeError` with the message
`Maximum call stack size exceeded`. It has no flag that says the engine raised
it. So when a run ends with an uncaught `RangeError` carrying exactly that
message, basal reports the run as having exhausted its stack budget, whether
the engine raised it or the script threw it itself.

Only that label is affected. The memory and CPU budgets are detected from
signals inside the worker that no script can produce: the allocator refusing
an allocation, and the thread's CPU clock. A script therefore can't fake
either of those. A script that throws the stack message only misreports why
its own run failed.
