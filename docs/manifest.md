# The flow manifest

A flow is a script plus a manifest. The manifest says everything the flow may do: what triggers it, which agents it may write to, which module ops it may call, whose facts it may read, and how many model tokens it may spend. basal checks every call the script makes against the approved manifest in the parent process before anything is dispatched, never in the confined worker that runs the script. This note is the schema; the code is `crates/basal-core/src/manifest.rs` (decoding and the checks that need nothing else) and `crates/basal-core/src/install.rs` (the checks against the catalog).

## Rules that apply everywhere

- The manifest is one JSON object, at most 64 KiB, checked before it is parsed.
- **Unknown fields are refused** at every level, and so are duplicate keys. A misspelt grant is an error, never a silently missing permission.
- Lists hold at most 64 entries and no entry twice.
- Approval binds to the **code hash**: BLAKE3 over the domain tag `basal-code-hash-v1` followed by a NUL byte, then the script's length as a little-endian u64, the script's exact bytes, the manifest's length as a little-endian u64 and the manifest's exact bytes, written as lowercase hex. Bytes cannot move between script and manifest without changing the hash. prefrontal-core recomputes the same hash for the consent card, and both sides pin the same test vectors (`ids::tests::code_hash_matches_the_shared_vectors`). Any edit, whitespace included, is a new version and needs a new approval.

## Fields

| Field | Type | Required | Default | Limits and meaning |
|---|---|---|---|---|
| `format` | integer | no | `1` | The manifest format. Only `1` is read. |
| `id` | string | yes | | The flow id: 1 to 63 bytes of `a-z`, `0-9`, `-`, `_`. Stable across versions; `kv`, the token windows, the rate limits and the owner are per flow id. `flow.install` refuses ids ending in `_` followed by exactly 16 lowercase hex characters (`_[0-9a-f]{16}$`) with `flow_id_reserved`: this namespace belongs to [package instances](packages.md). |
| `version` | integer | yes | | At least 1. A new install must be above the approved version. |
| `state_epoch` | integer | no | `1` | From 1 to 2147483647. Declares which stored-value layout this version reads; valid for ordinary flows, packages and templates. Part of the hashed manifest and approval card. |
| `purpose` | string | yes | | 1 to 1024 bytes, no control characters except newline. Shown on the card. |
| `trigger` | object | yes | | Exactly one of `events` and `schedule` (below). |
| `audience` | `{kind, id?}` | no | none | Approved delivery reach: global, workspace or agent. See below. |
| `max_targets` | integer | no | `64` | Maximum distinct agents a run may make `for:` module calls for; 1 to 1024. Shown on the card. Repeated calls for the same stable id use one slot. |
| `sinks` | list of sink grants | no | `[]` | Agents whose digest the flow may write. |
| `status` | list of agent names | no | `[]` | Agents whose status line the flow may set. |
| `claims` | list of claims | no | `[]` | Source kinds the flow takes over from core for an agent. |
| `ops` | list of op references | no | `[]` | Every module op the flow may call. Anything else is refused at run time. |
| `facts` | facts grant | no | none | Whose facts the flow may read. Without it, `facts()` is refused. |
| `llm` | model grant | no | none | Without it, `llm()` and `classify()` are refused. |
| `fs` | `{read?, write?}` lists of roots | no | none | Directories the file built-ins may read under (`fs.read`, `fs.list`, `fs.stat`) or write under (`fs.write`). Each root is absolute after `~` expansion and must exist at install. See `docs/builtins.md`. |
| `git` | `{read}` list of repositories | no | none | Repositories the git built-ins may read. Each is absolute after `~` expansion and must exist at install. |
| `net` | `{fetch}` list of `{host, methods?}` | no | none | Hosts `net.fetch` may reach over HTTPS on port 443 only, including redirect hops. `methods` defaults to `GET` and `HEAD`; others must be listed. Wildcard and IP-literal hosts are refused. |
| `params` | object of bounded parameter declarations | packages only | absent | Declares a [template](templates.md): at most 16 typed parameters with required bounds. Whole-string placeholders may only narrow the documented grants and timing slots. |
| `template` | `{max_per_agent, max_live?, max_lifetime}` | with `params` | absent | Instance quotas and mandatory expiry ceiling; part of the approved exact bytes. Ordinary install and dry run refuse either field with `params_require_package`. |
| `placement` | string | no | none | Where the flow runs, such as `"machine:studio"`. 1 to 128 bytes, no control characters. Shown on the card. |
| `concurrency` | integer | no | `1` | Only `1`: runs of a flow run one at a time, in trigger order. |
| `deadline` | duration | no | the runtime's default (10 minutes) | The per-run wall-clock deadline, from `"1s"` to `"24h"`; an audience limits it to `"12h"`. |

Names: module ids use `a-z`, `0-9`, `-`, `_`; op names add `A-Z` and `.` (`browser.read_page`; the pair, never the dots, identifies the op); event names and source kinds are one NATS subject token, `a-z`, `0-9`, `_`; agent names use letters, digits, `-` and `_`. Every name is 1 to 128 bytes.

A duration is a positive whole number followed by one unit: `s`, `m`, `h` or `d` (`"90s"`, `"10m"`, `"6h"`, `"1d"`).

### Stored-value compatibility

Each flow id records the `state_epoch` its `kv` belongs to, initially 1 for existing
and new flows. Template instances inherit their version's epoch declaration,
but their recorded epoch starts at 1 and changes independently on admission.
If the declared and recorded epochs
differ, the first admitted run clears only that flow id's `kv` and records the
new epoch in the same transaction. Equality is the only rule: rolling back from
epoch 2 to epoch 1 clears values again. Approving the version approves this reset;
there is no separate reset card. Approval alone, or a version never admitted,
clears nothing.

Previously admitted runs retain their own version and finish before an
incompatible version can be admitted (`state_busy` internally while waiting).
Events follow normal bounded-backlog and eligibility rules; planned schedule
fires stay queued for retry.
An epoch-busy flow is skipped for the rest of that scheduler admission pass;
other flows' planned fires continue, and the next pass rechecks the waiting flow.
Token/rate windows, schedule positions, event receipts/backlog, deduplication
records, journals and decision cards are never cleared by the reset. Durable
`state_resets` history logs flow id, old/new epoch, keys cleared and time.

Management fields are described in [ops](ops.md): `flow.list` and `flow.health`
report recorded `state_epoch`; health adds `last_state_reset_at`; `package.get`
reports declared `state_epoch` and `instance_state_epochs`; `flow.dry_run` always
reports boolean `state_reset` without changing stored values.

### Agent fields

Agent fields also accept the literal `$self` in [package manifests](packages.md). Registration permits only `$self`; activation resolves it to the instance's stable owner id. Ordinary `flow.install` refuses `$self` with `self_requires_package`.

Every agent named by `sinks[].agent`, `status[]`, `claims[].agent` and `facts.targets[]` must be known to the catalog. Fields may use a display name or a stable `agent_id`; the catalog resolves both to that id. For an agent-owned flow, every resolved id must equal its author's stable id: the flow acts only for its owner. Install and dry run refuse a foreign target with `foreign_agent_target`, naming the field and agent, before recording an install or raising a consent card. The author's id comes from the daemon-stamped caller scope for an agent install, not the manifest. An operator installing in an agent's name uses that agent's stable id as the author and follows the same rule. Global flows authored as `operator` and local-caller installs authored as `local:unverified` may still name any known agent.

### `audience`

The audience is part of the manifest bytes, so changing or widening it changes
the code hash and requires a new version and approval. Core renders this reach
on the approval card. The only shapes are `{ "kind": "global" }`,
`{ "kind": "workspace", "id": "workspace-id" }` and
`{ "kind": "agent", "id": "agent-id-or-name" }`. Global forbids `id`;
the others require it. Unknown fields are refused. Workspace ids are nonblank
strings of at most 512 bytes; agent ids follow the agent-name rules above.

At install and dry run, agent ids resolve through core's `agent.list` registry
to a stable id. An unreadable registry fails closed. Workspace ids must exactly
match a workspace returned by Entorhinal's management query `enumerate` with
`{ "workspaceId": "workspace-id" }`; a missing workspace returns
`unknown_workspace`, and an unavailable or unreadable registry returns
`workspace_registry_unavailable`.

An agent-authored agent audience must resolve to that author, or installation
and dry run return `foreign_agent_target`. Any author may request a workspace or
global audience through core's install approval flow. Self installs are allowed
by default, workspace installs ask by default (configurable), and global installs
always require a card answer: basal refuses global policy approvals, including
during startup recovery. Installing in an agent's name still applies that agent's
static target authorship rules. An agent-authored flow may claim sources or read named facts
targets only for its own stable agent id; operator-authored flows may name other agents.

With an audience, every digest sink must declare `agent: "$audience"` and every
status entry must be `"$audience"`. A named sink or status target returns
`audience_sink_named`; static and audience targets cannot mix. Without an
audience, `$audience` is refused with `audience_placeholder_without_audience`.
It is not valid in claims or an agent audience id. Facts may use it only with a
global audience, exclusively as `facts: {"targets": ["$audience"]}` with `text`
absent or false. Workspace and agent audiences return `audience_facts_requires_global`
until core supports checking facts membership. Mixing named facts targets returns
`audience_facts_mixed_targets`; granting private text returns `audience_facts_private_text`.
Without any audience the placeholder returns `audience_placeholder_without_audience`.
A declaration grants only
its sink kind, not another kind. Scripts still pass a recipient on each write;
core checks current audience membership at delivery time and refuses an outside
recipient with `sink_target_not_in_audience`. Basal passes that code through
unchanged. An omitted audience keeps all existing static-target behavior.

An audience also permits `ops.call(module, op, args, {for: "agent"})` to use
that target agent's core-owned daemon scope. Names resolve to stable ids before
the request is journaled. Without an audience, the option returns
`for_requires_audience`; without `for`, the call keeps using the flow's own
scope. A targeted query reads as the agent. A targeted mutation requires that
op's own `for_target: "delegate"` grant, or returns
`for_target_requires_delegate` before dispatch, even if another op delegates.
The distinct-target limit returns `too_many_targets` before sending a new
target's call. Core checks current approved audience membership when registering
the scope. Its permanent refusals are `sink_target_not_in_audience`,
`flow_not_approved`, `flow_has_no_audience` and `scope_expiry_too_far`.
`sink_audience_unavailable` is transient and defers the call for retry until the
run's deadline. These refusals retain their codes. A
`flow_target_scope_principal_refused` refusal fails the call and logs a carrier
route bug rather than retrying it.
Replay asks core again, so it cannot reuse authority from an old audience.
If revalidation fails, catching the refusal does not permit subsequent calls
to fall back to that run's old cached authority.

Audience manifests with a run deadline above 12 hours are refused at install
and dry run with `run_bound_exceeds_audience_limit`. Core registers target scopes
for twice the maximum run duration, which must fit the daemon's 24-hour scope
limit. When a later run needs a scope that lasts longer, core registers a new
one (a new ref) instead of extending the old one, and the old one stays valid
until it expires, so no run loses its scope because another run asked. Basal
reuses a scope only if its expiry covers the asking run's deadline, and a run
keeps using the first scope it got for each target. A scope that ends is never
reused; if it ends while a call is in flight, a mutation needs reconciliation
and only queries may retry.

### `trigger`

- `{ "events": [ { "module": "plexus", "name": "github_pr_changed", "version": 1, "match": { "repo": "cortexkit/*" } }, ... ] }`: at least one event. Each entry has `module`, `name`, `version`, and an optional `match` object. Install refuses an event or version the catalog does not declare, an event whose `resolve_op` is not a query, or a resolver absent from `ops`. For Plexus notices grant `{ "module": "plexus", "op": "events_get" }`, its separate read-only body tool. The broader `events` tool can change subscriptions, so it is not a safe body resolver. Matching uses the module, name and version, the optional header filters, and the flow-targeting rule below.
- `{ "schedule": { ... } }`: the scheduler's `ScheduleSpec` (`crates/basal-core/src/schedule/spec.rs`), decoded with unknown fields refused and compiled by `schedule::validate` when the manifest is checked. Exactly one of `cron`, `interval`, or a template-only `at`:

  A template's `at` is the whole-string placeholder `"$params.<name>"` for a `timestamp` parameter. It fires once, including one catch-up after downtime, and ends the instance after that run regardless of outcome. Only default or explicit `missed: "once"` is permitted; `tz`, `each_cap`, `skip` and `each` are refused with `template_placeholder_invalid`, naming the field. A literal timestamp is not permitted in the registered manifest. [Templates](templates.md#one-shot-schedules) specifies creation-time timestamp validation and normalization.


  | Field | Type | Default | Limits and meaning |
  |---|---|---|---|
  | `cron` | string | | A five-field pattern (minute granularity; no seconds or years field) that matches at least one date. |
  | `tz` | string | `UTC` | With `cron` only: a canonical IANA zone name, spelt exactly (`Europe/Madrid`). A time in a spring-forward gap fires once at the transition; a time in a fall-back overlap fires once, at its first occurrence. |
  | `interval` | duration | | `"60s"` to `"365d"`; due times are the approval instant (to the second) plus whole periods. |
  | `missed` | `"once"`, `"skip"` or `"each"` | `"once"` | What happens to due times that passed while basal was not ticking (asleep, stopped, or more than the grace period late): one catch-up fire, none, or one each. |
  | `each_cap` | integer | 3 | With `each` only: 1 to 10; the newest missed due times are kept. |

  The newest due time is on time when it is at most the grace period (60 s by default) old; every older due time in the window is missed and follows `missed`. `once` fires one catch-up identified by the last missed due time; `each` keeps the newest `each_cap` missed due times and fires them oldest first. An on-time due time always fires, after any catch-up. A fire's trigger id is `schedule:<due time, RFC 3339 UTC>` (`schedule:2026-03-29T01:00:00Z`), and admission deduplicates on flow and trigger id, so one due time starts at most one run. The script sees `trigger` as `{ "kind": "schedule", "due": "..." }`; a catch-up fire adds `"catch_up": { "policy", "missed_count", "missed_window": { "first", "last" } }`, counting every missed due time, including those `each_cap` dropped. A scheduled fire, like every trigger, runs the flow's version approved and enabled when it is admitted.
- Both, or neither, is refused.

`match` has at most **8** keys. Keys are header names: **1–64 bytes** of
`a-z`, `0-9`, or `_`, and must appear in that event version's catalog `headers`
declaration. An unknown key (including when headers are absent or empty) is
refused at install and dry run with `event_header_not_declared`, naming the key.
Values are string patterns of **1–256 UTF-8 bytes**. Only `*` is special: it
matches any run of characters, including zero characters and `/`. Everything
else is literal: no `?`, character classes, regex, or escape syntax. `repo`
values compare case-insensitively for ASCII letters; all other values compare
exactly. Every key must be present with a **non-empty** matching value. An
absent or empty `match` imposes no filters. Templates may bind whole-string
`$params.<name>` values of type `string` or `enum` (see [templates](templates.md)).

Filters run before admission, receipts, rate accounting and backlog insertion.
A filtered notice is acknowledged without any of that work and increments the
flow's cumulative `filtered_out` health counter (and the reader's delivery
counter). Like `skipped_before_install`, redelivery after a lost acknowledgment
may count a skip again. Filters and their placeholders are part of the exact
hashed manifest bytes shown on the approval card.

**Headers remain hints, not authority.** A publisher can mislabel a notice.
Filters can only skip work to reduce cost; they never establish authorization,
repository identity or a condition worth acting on. Scripts must still check
the verified body before making decisions. Body retrieval uses the flow's own
scope regardless of the filter.

Module notices fan out to the flows approved and enabled at admission, but only
when JetStream's stored publish time is at or after the flow's **first approval**.
Publisher headers never establish that time. A newer version's approval does
not move the cutoff: eligible events published between approvals run the code
approved at admission. The same rule applies to package instances and to
backlog draining. Older matching deliveries are acknowledged without a run or
receipt and increment the flow's cumulative `skipped_before_install` health
counter; an acknowledgment lost after commit can count a delivery again.
There is no catch-up opt-in yet.

To receive Plexus's v2 envelope, declare the same event name with `"version": 2`
in `trigger.events`, provided the source's `catalog-events/v1` declaration lists
that version. Notices on `ck.<acct>.event.plexus.<event_name>.v2` match only v2
triggers; `.v1` matches only v1. A missing version suffix is malformed. Other
version suffixes must be canonical and declared by the catalog, or the notice
is malformed.
The same event key on v1 and v2 subjects represents two distinct deliveries;
deduplication within each subject is unchanged. The v2 `event_key` header,
transport id, explicit fetch version and always-present body fields are
described in [the script contract](script.md#module-event-triggers). Every
declared version beyond v1 uses the bare `event_key` header and an exact
`<event_key>.v<N>` transport id, where `N` is the subject version.

Flow-targeted events are admitted only to the flow named by the notice's
`flow_id` header. Currently the known flow-targeted contract is
`prefrontal-core`'s `digest_delivered` event, declared in `trigger.events`
with version `1` and an `ops` grant for `prefrontal-core.events_get`.
Other flows declaring that event are no-match; a missing or empty `flow_id`
header is malformed. Both are acknowledged without a run for those flows.
After fetching the body, basal also requires its `flow_id` to equal the run's
flow, including on replay, or fails the run with `event_body_mismatch`.

A `(flow_id, subject, event_key)` is admitted once, including after redelivery
or restart. Basal commits the fan-out before acknowledging the notice; an
unmatched notice is acknowledged without a receipt. Receipt history is kept
eight days (seven days plus maintenance slack), then pruned by age. Existing
run inbox keys and permanent trigger tombstones continue preventing duplicate
effects after receipt history expires.

The reader binds ck-bus's existing `m_basal` durable and never creates or
changes it. Its deliver-all policy starts at the oldest retained notice.
Pulls hold at most 32 notices and use progress acknowledgments during a slow
commit. History older than first approval cannot flood a newly installed flow.
For eligible history, the normal per-flow rate limit bounds initial admissions;
excess work waits oldest-first in a durable backlog of at most 120 notices
per flow, equivalent to two default 60-run rate windows. Overflow is refused,
counted in `flow.health` (`event_backlog`, `event_overflow`, `overflowed`), and
duplicates of a refused notice do not retry admission during its retention.
By default, three consecutive one-minute windows in which an admission or
dispatch is refused at its rate limit automatically disable the flow. Module
health exposes reader connection/backoff and first-batch age, size and elapsed
time; those measurements let operators distinguish historical catch-up from
current traffic.

Production lookup reads each module's declared `events` from the daemon catalog.
It requires the daemon-wide `catalog-events/v1` capability, supplied in basal's
registration acknowledgement and refreshed on reconnect (the same list as
`server.describe`). Before that acknowledgement, or without the capability,
event installs are refused. Missing or empty event declarations admit no events;
only an exact declared name and version is accepted at install and dry run.
Declared filter headers are retained as hints, not authority. No alternative
wire field is inferred.

### Sink grant

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `agent` | agent name or `$audience` | yes | | A known static agent without an audience; exactly `$audience` with one. |
| `digest_max` | `"silent"`, `"piggyback"` or `"wake"` | yes | | The most intrusive action approved for the sink. An omitted action uses this cap, in both live dispatch and dry runs. Basal refuses static writes above the cap; audience writes go to core for delivery admission. |
| `status_group` | string matching `[A-Z]{1,4}` | no | absent | Groups undelivered tagged digest items in the agent's status segment, for example `GH NI3 NRPR13`. Part of the hashed manifest shown on the approval card; checked at install and dry run. Only this grant permits per-write `status_tag`. |
| `break_through` | boolean | no | `false` | Shown on the card as requested. Only the operator grants it, through policy. |

### Claim

`{ "agent": "BASAL", "source_kind": "idleness" }`. The agent must be known. Core enforces claims; basal shows them on the card.

### Op reference

`{ "module": "cerebellum", "op": "browser.read_page" }`. Install refuses an op that:

- is not in the catalog;
- has no `query` or `mutate` marker;
- is shell-capable: marked so in the catalog, or on basal's denylist (`ShellDenylist`, by default AFT's `bash` and basal's `codemode`, matched without regard to ASCII case);
- is a `mutate` op on a module whose events trigger the flow and does not declare `cause_echo`, unless the operator overrides the loop install rule (`InstallRequest::loop_override`). The card then states that loop protection for the flow is rate limiting only.

At run time the denylist and the catalog's shell marker are checked again before each dispatch, so an op marked shell-capable after the flow was approved is still refused.

An op reference may add `"for_target": "delegate"`, for example
`{ "module": "cerebellum", "op": "browser.click", "for_target": "delegate" }`.
The only value is `"delegate"`; absence grants a targeted read, not a targeted
mutation. Delegation is valid only with an audience, or installation returns
`for_target_requires_audience`. Core shows the marker on the approval card and
marks target scopes as delegating when any approved op has it. Basal still
enforces delegation per op. Providers must explicitly support delegation;
`flow_delegation_unsupported` is a typed, unsent refusal with no retry and no
flow-health penalty, as is `for_target_requires_delegate`.

### Facts grant

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `targets` | list of agent names | yes | | Agents whose `facts()` the flow may read. Named targets must be known. A global audience may instead declare only `"$audience"`, permitting public facts of any agent id; core's unknown-agent error code and message reach the script unchanged. |
| `text` | boolean | no | `false` | Whether `private-text` fields may be read (`include: ["text"]`). |

### Model grant

| Field | Type | Required | Meaning |
|---|---|---|---|
| `token_cap.tokens` | integer | yes | 1 to 1,000,000,000 tokens per window, counted as fresh input plus cache write plus output. Cached input is recorded, not capped. |
| `token_cap.window` | duration | yes | `"1m"` to `"31d"`. Windows are fixed intervals anchored at the Unix epoch in UTC, so `"1d"` is a UTC calendar day. |
| `iq` | integer | yes | 0 to 100. The minimum intelligence demand passed to fleet routing for every `llm` and `classify` call. |
| `eq` | integer | no | 0 to 100; defaults to 0. The minimum judgment demand passed to fleet routing. |
| `max_output` | integer | yes | 1 to 200,000, and not above the cap: the ceiling on any one call's output tokens. A call asking for more is clamped to it; `classify` is clamped to at most 64. |

Model calls get no tools: an `llm` request with a `tools` field is refused. Scripts cannot supply a `model` or `provider`, even one previously selected: the call is rejected with `model_not_allowed`. Fleet routing chooses a Broca runner before the intent commits, and its exact selection and decision id are journaled. Retries reuse that selection; there is no default or environment-configured model.

## Card fields not in the manifest

The authoring agent comes from the install request, not the manifest, so a flow cannot claim another author. The code hash, the code itself, the warnings (private text together with a `mutate` op; an operator override of the loop rule) and the current window's token usage split by field are computed by basal.

## Complete example

This example is parsed, round-tripped and installed by `dispatch_manifest::documented_example_round_trips_and_installs`, so it cannot drift from the code.

<!-- manifest-example -->
```json
{
  "format": 1,
  "id": "synapse-xcom-inference-news",
  "version": 3,
  "state_epoch": 1,
  "purpose": "Walk the operator's x.com feed for inference news and send hits to Synapse.",
  "trigger": { "schedule": { "cron": "*/30 * * * *", "tz": "Europe/Madrid", "missed": "once" } },
  "sinks": [ { "agent": "SYNAPSE", "digest_max": "piggyback", "break_through": false } ],
  "status": [ "SYNAPSE" ],
  "claims": [],
  "ops": [ { "module": "cerebellum", "op": "browser.read_page" } ],
  "facts": { "targets": [ "SYNAPSE" ], "text": false },
  "llm": { "iq": 65, "eq": 20, "token_cap": { "tokens": 200000, "window": "1d" }, "max_output": 2000 },
  "placement": "machine:studio",
  "concurrency": 1,
  "deadline": "10m"
}
```

An events trigger instead of the schedule:

```json
{ "events": [ { "module": "plexus", "name": "pull_request_review", "version": 1 } ] }
```
