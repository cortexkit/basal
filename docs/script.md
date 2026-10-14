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
query on **that flow's scoped route**, with `{event_key, event_name, version}`.
Basal always sends the integer version from the notice (`1` or `2` for Plexus),
although Plexus defaults to v1 when callers omit it. It hashes
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

Plexus v2 notices use `ck.<acct>.event.plexus.<event_name>.v2`. Their
`Nats-Msg-Id` is `<event_key>.v2`, while the required `event_key` header carries
the bare fetch key. Basal refuses a missing header or mismatched transport id
as malformed; it never fetches using the suffixed transport id. The v2 digest
covers exactly the stored JCS body string, which `events_get` returns unchanged.
An old row without a v2 envelope fails a v2 fetch as `event_not_visible`, with
no fallback to v1.

The v2 body has `kind` (`issue`, `pull_request`, `ci_job`, `ci_run` or `release`),
`event_key`, lowercase `repo`, `action`, `actor_logins` and `actor_unknown`, plus
the fields defined for that kind. Every defined field is always present;
unknown values are JSON `null`, never omitted. Scripts can therefore distinguish
unknown values without testing for absent properties. V1 bodies keep their
existing shape.

For a capture-mode dry run, supply the complete synthetic `{event: {...}}`
value above as the `flow.dry_run` request's `trigger`. The supplied `body` is
already a JSON value, not stored text; it is delivered without a live body
lookup or any other live call. Synthetic dry-run triggers exercise script logic,
not live notice admission. Optional `trigger.events[].match` filters skip
non-matching notices before any run, body lookup, receipt or backlog entry, and
count skips in `filtered_out` health. Patterns use only `*` (including across
`/`); `repo` compares ASCII-case-insensitively, other headers exactly, and every
filtered header must be present and non-empty. See [manifest filters](manifest.md#trigger).
Headers are publisher hints, **not authority**: a publisher may label wrongly.
A filter only reduces cost; scripts must still check the verified body before
deciding what to do. It never grants access to an otherwise invisible body.

## Module calls for a target agent

`ops.call(module, op, args)` uses the flow's own daemon scope. A manifest with
an audience may instead call `ops.call(module, op, args, {for: "agent"})`.
The options object contains only `for`, a nonempty agent name or stable id.
The target is journaled separately from the provider's arguments; the provider
receives `args` unchanged on that agent's core-owned scope. Targeted queries
read as the agent; mutations require the op's own `for_target: "delegate"`
grant. See [the manifest rules](manifest.md#audience) for limits and refusals.

There is no script-triggered lazy fetch of `trigger.event.body`: basal resolves
it before the script starts, on the flow scope. A script that explicitly calls
a granted body resolver such as `events_get` can use the same fourth argument
as any other module query.

## Digest items

Sink signatures are unchanged by an install audience:
`sink.digest(agent, item, action, options)` and `sink.status(agent, value)`. With no
audience, the recipient must be listed statically in `sinks[].agent` or
`status[]`. With an audience, the corresponding manifest entry is `$audience`,
and the script supplies a concrete recipient on every write. For example:

```json
{"audience":{"kind":"workspace","id":"my-workspace"},"sinks":[{"agent":"$audience","digest_max":"piggyback"}],"status":["$audience"]}
```

These are manifest fields, not a complete manifest. Basal checks the requested
sink kind is declared; prefrontal-core checks whether the recipient is inside
the approved audience at delivery time. A refusal such as
`sink_target_not_in_audience` reaches the script unchanged, so it can catch the
rejection. Membership is not cached at install or decided by the script.

Any author may request a workspace or global audience through `flow.install`;
the requester does not grant that reach. Core allows self installs by default,
asks for workspace installs by default (configurable), and always asks for
global installs. Basal never accepts a policy approval for global reach, even
at startup recovery: that audience needs core's card answer.

`sink.digest(agent, item, action, options)` delivers `item` to the agent's digest
through prefrontal-core, which validates it strictly. `action` is `silent`,
`piggyback` or `wake`; the manifest's `digest_max` is the approved cap. The item has
these four fields. `data` and `links` may be omitted, and then default to
empty. Core refuses a missing `title` or `body`, or any field not listed
here, with `sink_item_invalid`, and the refusal names the field.

The optional fourth argument may contain `{ status_tag: 'NI' }`. The tag must
match `[A-Z]{1,4}`; malformed values (including `null`) are refused with
`status_tag_invalid`. The recipient's digest sink grant must have a
`status_group`, or basal refuses the write with `status_tag_without_group`
before preparing or sending a core request. The same checks apply in dry runs
and to the `$audience` sink grant. To use the default action with a tag, pass
`undefined` as the third argument.

The options object may also contain `replace_key`. It must be a string of 1 to
200 UTF-8 bytes with no control characters; otherwise basal refuses the call
before dispatch with `invalid_arguments`. Basal sends it as the top-level
`replace_key` field beside `item`, and journals it with the core request. Basal
only sends the field when the current prefrontal-core catalog advertises
`flow-digest-replace-key/v1`. If it does not, basal refuses the call before
dispatch with `digest_replace_key_unsupported`, which a script may catch and
then retry without the key. Basal never silently drops a supplied key. The
replacement behavior belongs to core: within one flow, a write with the same
key replaces that agent's earlier undelivered item; after delivery, the next
write starts fresh.

Basal sends the tag to prefrontal-core as top-level `status_tag`, not in the
item or its `data`. Without a tag that field is omitted and the wire request
is unchanged. The journal freezes it with the request, so crash replay resends
the same bytes.

The item fields are:

| Field | Type | Limit |
| --- | --- | --- |
| `title` | string | at most 200 characters |
| `body` | string | at most 4,096 bytes |
| `data` | object, optional, default `{}` | 8,192 bytes as canonical JSON |
| `links` | array, optional, default `[]` | 8 entries |

Title and body must not both be empty.

A link is either `{ kind: 'url', url: 'https://…' }`, an HTTPS URL with a
host, or `{ kind: 'work', id: 'wi_…' }`. The whole item may be at most 16,384
bytes as canonical JSON.

```js
const item = {
  title: 'basal nightly CI: failure',
  body: 'Commit 0123abc, started 2026-10-09T03:17:00Z.',
  data: { run_id: 123 },
  links: [{ kind: 'url', url: 'https://github.com/cortexkit/basal/actions/runs/123' }],
};
try {
  await sink.digest('BASAL', item, 'wake', { status_tag: 'NI', replace_key: 'nightly-ci:123' });
} catch (e) {
  if (e.data.code !== 'digest_replace_key_unsupported') throw e;
  await sink.digest('BASAL', item, 'wake', { status_tag: 'NI' });
}
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

## Public facts across a global audience

A global flow may declare `facts: {"targets": ["$audience"]}` to call
`facts(agent, {include: ['activity']})` for board owners discovered at run time.
Activity values such as `activity.last_activity_at` are Fact envelopes: inspect
`status` before using `value`. An `unknown` status is not evidence of activity.

This facts form is global-only and cannot grant private text (`text: true`) or
mix named targets with `$audience`. Workspace and agent audiences cannot use it
until core can check facts membership. Named facts targets retain their existing
static grants. Basal permits a global public-facts call for any agent id to be
sent to core; core's refusal for an unknown agent reaches the script unchanged.
