# How a flow script runs

A flow script runs in `ck-basal-worker`, a separate process that confines
itself before it reads anything from basal. The script reaches the world only
through host calls: `sink.*`, `facts`, `ops.call`, `llm`, `classify`, `kv` and
the built-ins. basal checks each one against the approved manifest, in the
parent process, and journals it there. This page documents install and dry-run
warnings, module-event triggers, module calls for a target agent and the digest
item shape, then two deliberate limits of the script runtime and public facts
across a global audience.

## Size limits for one run

Every host call's arguments, request and result are journaled, so a run can
replay after a crash without repeating a call. That journal sets the limits:

- **One call's arguments:** at most 256 KiB. A larger argument is refused as
  a journaled rejection before it is parsed.
- **One call's result:** at most 512 KiB. A larger result is recorded as a
  rejection with code `result_too_large` before anything parses it. Page reads
  that can grow, and keep each page well under the cap.
- **One run's journal:** at most 16 MiB across all its calls. A call that would
  pass it fails the run.
- **Calls:** at most 10,000 per run.

A terminal run's journal is kept for seven days, and longer while a call or
decision card still needs it, then deleted. These limits apply to every flow
and template on this basal.

## Install and dry-run warnings

Flow installs and dry runs report `protected_root` when an `fs.read`,
`fs.write`, `git.read` or template parameter `under` root lies in a folder an
operating system protects. The warning does not refuse the flow. It carries the
manifest `field`, the original `path`, and `protections`: one entry per
operating system whose protection covers the path, each with `os`, the
protected `folder` and its `effect`. Its `text` gives one plain line per entry.
A flow can run on any operating system, so both rules below are checked on
every host:

- **macOS** (`"os": "macos"`): `~/Desktop`, `~/Documents`, `~/Downloads` and
  each volume under `/Volumes/` (removable and network volumes; a root at
  `/Volumes` itself is reported as `/Volumes/*`, every volume). The first
  access asks the operator for permission.
- **Windows** (`"os": "windows"`): `~/Documents`, `~/Favorites`, `~/Music`,
  `~/Pictures` and `~/Videos`, in any letter case, the profile folders that
  Controlled Folder Access protects by default. Writes under them are blocked
  while the operator has Controlled Folder Access on.
- **Linux** has no general per-folder protection, so no entry names it.

A root written with `~` gets the same warning on every host; an absolute root
matches when it lies in the home folder of the host running basal, so its
warning can differ between hosts. Paths are compared lexically (`.` is dropped
and `..` collapsed): symlinks are not resolved and the folders need not exist.

## Revocation during a run

Core pushes `flow.install_revoked` when it revokes an approved version. Basal
verifies the hint with one status read before recording it. After that record,
every later host-call attempt on the run's pinned version is refused with typed
`version_revoked`, and the run ends even if the script would catch the rejection.
This includes ops, sinks, facts, llm, classify, built-ins, clock/random reads and
local `kv`: local effects are still effects of the revoked code.

Calls authorized before the revocation was recorded may still be sent and settle;
none is authorized after. Retries are checked again, and an earlier mutation
whose outcome is unknown follows the normal reconciliation rules. Health reports
the failed run's `version_revoked` kind without counting it towards auto-disable.
Template and package instances retain their enable switches. The existing
per-activation status check catches missed pushes; basal never polls for
revocations while idle.

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
Optional literal `trigger.events[].max_age` limits (1 minute to 7 days) skip
stale notices before admission or rate accounting and count `stale_dropped`
alongside `filtered_out` in health. They use the transport publish time, not a
body timestamp; scripts may retain their own body-age checks. Missing publish
times remain subject to the first-approval cutoff (`skipped_before_install`),
not the age filter.
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

## Resolving board waits

`sink.wait.resolve(outcome, note?)` resolves open waits on the install owner's
board whose `wait.ref` names this flow (`flow:<flow_id>`). It requires the exact
targetless manifest grant `"sinks": [{"kind":"wait"}]`; it grants no digest or
board-lane writes. `outcome` is `"met"` or `"failed"`. The optional note is at most
500 UTF-8 bytes with no control characters. Scripts cannot supply an agent,
flow identity, version, code hash, or receipt key.

```js
return await sink.wait.resolve('met', 'CI is green');
// => { resolved: 2, replayed: false }
```

Basal journals the approved install stamp and outcome before sending
`sink.wait_resolve` through its daemon-attested core carrier. A retry reuses the
receipt key and frozen request; core returns the original count with
`replayed: true`. Resolving no open waits succeeds with `resolved: 0`. Core must
advertise `flow-wait-resolve/v1`; otherwise the call is refused unsent with
`wait_resolve_unsupported`. No `flow-scopes/v1` capability is needed. Missing
grants are refused unsent with `sink_wait_not_granted` at runtime; manifest ops
listing `prefrontal-core/sink.wait_resolve` without the grant fail install.

Core's `sink_store_failed` refusal is retryable with the same receipt until the
run deadline. Invalid input, receipt conflicts, principal/installation/hash or
grant refusals, and expired instances are terminal rejections visible to the
script. Capture and live dry runs only record that the flow would resolve its
waits; neither sends a resolution. Resolving a wait does not unblock the board
item: its owner must update the item. Core owns the consent card's `Waits` fact
(“Can mark your board waits on this flow as met or failed”) and the terminal wait
markers, including `ended_without_resolving` when a flow ends without resolving.

## Board lanes

`sink.board` writes only the flow's own lane on an agent's board:

```js
const { lane_id, item_id } = await sink.board.append('SYNAPSE', 'Review the build');
await sink.board.setState('SYNAPSE', item_id, 'active');
await sink.board.setState('SYNAPSE', item_id, 'done');
await sink.board.retire('SYNAPSE');
```

Grant boards in the hashed manifest with
`"board": [{"agent": "SYNAPSE", "title": "Build work"}]`. Without an audience,
`agent` is a concrete agent name. Core also accepts `$self`, but basal admits
it only in templates and packages, as for every sink; it resolves to the
instance owner's stable id. An ordinary agent-authored flow may name only its
own agent. With an audience, the grant must use
`$audience`, and each call supplies a concrete recipient; core checks current
audience membership. Duplicate agents, missing or blank titles, titles over 56
characters, and control characters in titles are refused at install and dry run.

There is one lane per flow per agent board, `virtual:flow:<flow_id>`, titled
from the approved manifest. Scripts cannot choose a lane or edit another flow's
items. `append` returns `{lane_id, item_id}`. `setState` accepts `pending`,
`active`, `done`, or `blocked`. Its optional `wait` is allowed only for `blocked`
and follows the board's wait rules (`{on, ref?}`), enforced by core: `pm:`
references are refused, and `item:` and `peer:` references must name something
that exists. Core refuses an invalid wait with `sink_board_wait_invalid`.
`retire` removes the flow's lane. Update and retire replies pass core's JSON
object through. Core enforces text, item and lane caps and returns the typed
refusals below when a cap is reached.

Basal checks the target grant before dispatch; an ungranted target is provably
unsent and rejected with `sink_board_target_not_granted`. Board writes use the
stamped receipt-key contract: `{stamp:{flow_id,flow_version,code_hash}, agent,
receipt_key, operation}`. Basal resolves the recipient and its grant names to
stable agent ids in one registry traversal, freezes the
stamp and operation before sending, and uses the journal's `bk1-…` idempotency
key as `receipt_key` (nonempty, at most 256 bytes, no control characters). Core
dedupes by sink, flow and receipt key, so a replay returns the original result.
Digest and status still use their older envelopes, not this board contract.
Dry runs capture board calls without sending, including in live-read mode.

Final rejections retain their core code: `sink_board_target_not_granted`,
`sink_board_item_not_owned`, `sink_board_wait_invalid`, `sink_board_text_cap`,
`sink_board_item_cap`, `sink_board_lane_cap`, `sink_board_lane_missing`,
`sink_item_invalid`, `sink_input_conflict`, `sink_flow_not_installed`,
`sink_flow_hash_mismatch`, `sink_principal_refused`, and
`sink_target_not_in_audience`. Catch them as `error.data.code`, like digest
refusals. `sink_store_failed` and `sink_audience_unavailable` defer the call
with bounded backoff and the same receipt key, up to the run deadline; expiry
records the typed unsent refusal. `sink_audience_check_required` is logged as a
core wiring error and rejected finally, never retried as a readiness problem.

A failed `agent.list` read is not evidence that the target is ungranted. Basal
defers that provably unsent call as `sink_board_target_unavailable`, using the
same durable backoff and run-deadline bound as the retryable core refusals.
An answered registry read without the target or its grant remains the final
`sink_board_target_not_granted` refusal. After resolving successfully, basal
journals the stable target id before sending and does not resolve it again on
replay.

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

The options object may contain `replace_key`. It must be a string of 1 to
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

A flow may withdraw that pending item without replacing it with a placeholder:
`await sink.digest.withdraw(agent, { replace_key })`. It uses the same digest
sink grant and the same 1-to-200-byte, no-control-character key validation as a
write. Basal journals the request before sending it and requires core's
`flow-digest-withdraw/v1` catalog capability; without it, the call is refused
unsent with `digest_withdraw_unsupported`, which a script may catch and handle
with a silent empty replacement. The core reply's `disposition` is `withdrawn`,
`already_delivered` or `not_found`; all three are successful outcomes.
A withdrawal has no item or action. It uses the same `sink.digest` wire
operation and digest grant as a write.

```js
try {
  await sink.digest.withdraw('BASAL', { replace_key: 'nightly-ci:123' });
} catch (e) {
  if (e.data.code !== 'digest_withdraw_unsupported') throw e;
  await sink.digest('BASAL', { title: '0 items', body: 'Nothing is outstanding.' }, 'silent', {
    replace_key: 'nightly-ci:123',
  });
}
```

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
  await sink.digest('BASAL', item, 'wake', { replace_key: 'nightly-ci:123' });
} catch (e) {
  if (e.data.code !== 'digest_replace_key_unsupported') throw e;
  await sink.digest('BASAL', item, 'wake');
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
`facts(agent, {fields: ['activity']})` for board owners discovered at run time.
Core selects fact groups by `fields`; `include` accepts only `text`, which opts
into private-text fields and is refused under this grant.
Activity values such as `activity.last_activity_at` are Fact envelopes: inspect
`status` before using `value`. An `unknown` status is not evidence of activity.

This facts form is global-only and cannot grant private text (`text: true`) or
mix named targets with `$audience`. Workspace and agent audiences cannot use it
until core can check facts membership. Named facts targets retain their existing
static grants. Basal permits a global public-facts call for any agent id to be
sent to core; core's refusal for an unknown agent reaches the script unchanged.
