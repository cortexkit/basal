# Flow templates and agent-created instances

A template lets an agent say "tell me when X happens" without asking the operator each time. The operator approves a template version once, together with the bounds of what agents may ask it to watch. After that, any agent creates instances of it with its own parameter values, and no approval card is raised.

Typical templates:
- a file appears, or contains or lacks some text;
- a git commit, tag or branch moves;
- a URL's content changes;
- a pull request or issue changes state;
- a time is reached.

A template is a flow package ([packages.md](packages.md)) whose manifest declares parameters. Package rules still apply: the registry preserves each version's exact script and manifest bytes, the operator approves each version once, `$self` names the creating agent, and each instance keeps its own flow state. This document adds parameters, the bounds that check them, the operations agents and core call, and the lifecycle synchronization contract.

Template manifests also accept the optional install-card `title` with the [manifest limits](manifest.md#fields); it is literal, not a `$params` substitution slot.

## Parameters in the manifest

A template manifest adds a `params` object, mapping a parameter name to its declaration. Names are 1 to 32 bytes of `a-z`, `0-9` and `_`, with at most 16 parameters. Each declaration has a `type` and the bounds for that type. Every bound shown is required unless marked optional.

| `type` | Value | Bounds |
|---|---|---|
| `path` | an absolute path, after `~` expansion | `under`: a list of roots. The value must resolve inside one of them. |
| `repo` | an absolute path with a Git repository layout | `under`: a list of roots, as for `path`. Creation recognizes ordinary, bare and worktree layouts without running Git; Git itself validates the repository on every read. A malformed layout inside the bounds can therefore be created, but its reads fail with the git built-in's normal error. Layout pointer files inspected at creation are capped at 1 MiB to bound validation memory. |
| `url` | an `https` URL | `hosts`: allowed hosts. The URL's host must be one of them. No wildcards and no IP literals, as for `net.fetch`. |
| `duration` | a duration string, such as `"15m"` | `min` and `max`. |
| `integer` | an integer in JavaScript's exact range, `-(2^53-1)` to `2^53-1` | `min` and `max`, both in that range. |
| `string` | a string | `max_bytes`, from 1 to 4096. No control characters. |
| `enum` | one of the listed strings | `values`: 1 to 32 strings. |
| `timestamp` | RFC 3339 in UTC, with at most three fractional digits, such as `"2026-11-01T09:00:00Z"` | `max_ahead`: how far in the future it may be. Non-UTC offsets and finer precision are refused. Valid values normalize to UTC `Z`, with exactly `.mmm` when milliseconds are nonzero and no fraction otherwise. |

The manifest also gains a `template` object, required whenever `params` is present:
- `max_per_agent`: 1 to 100. The most live instances of this template one agent may have.
- `max_live` (optional): 1 to 10,000. The most live instances of this template across all agents.
- `max_lifetime`: a duration. Every instance expires, and none may outlive this.

A `duration` parameter's `min` must be at least basal's minimum schedule interval (60 seconds), including a script-only duration. A template whose `min` is lower is refused at registration with `template_bound_invalid`. A duration used as `trigger.schedule.interval` must also have `max` at most `365d`, the scheduler's ceiling. A script-only duration may have a larger positive maximum. Minimums must not exceed maximums; path/repo roots must be absolute after `~` expansion and exist at registration; host, root and enum lists contain distinct entries. Host and root lists are nonempty, with at most 64 entries.

### Parameters only narrow

A parameter may appear in a grant only where it narrows what the template's own grants already allow. A placeholder is the whole string `"$params.<name>"`, and is allowed only in these positions:

| Position | Parameter types allowed | Meaning for the instance |
|---|---|---|
| `fs.read[]`, `fs.write[]` | `path` | The instance may read or write under exactly that path. |
| `git.read[]` | `repo` | The instance may read exactly that repository. |
| `net.fetch[].host` | `url` | The instance may fetch from that URL's host. |
| `trigger.schedule.interval` | `duration` | The instance's cadence. |
| `trigger.schedule.at` | `timestamp` | A one-shot fire (see below). |
| `trigger.events[].match.<header>` | `string`, `enum` | A cost-only notice-header pattern bound before admission. |

At registration, basal checks every placeholder: it names a declared parameter; that parameter's type fits the position; a `path` or `repo` placeholder sits in a grant list, and its `under` roots meet that list's rules (absolute, existing at registration); and a `url` parameter's `hosts` meet `net.fetch`'s host rules. Anything else is refused with `template_placeholder_invalid`, naming the field. Grants without placeholders apply to every instance as written.

**Paths are canonical.** A `path` or `repo` value is canonicalized, with symlinks resolved, before the `under` check, and the instance's effective grant is the resolved path; otherwise a symlink under a root could point outside it. If the path doesn't exist yet, basal resolves its deepest existing ancestor and checks that. At creation, the parameter's `under` roots are also canonicalized and recorded with the instance. On every file or git access, a parameter-derived grant is resolved again and must remain inside those recorded bounds. If the path existed at creation, it must also still resolve to the creation-time path. Replacing the root or an ancestor with a symlink therefore cannot move authority outside the approved bounds. Ordinary static grants keep their existing behavior. A template's `fs.stat` on a missing parameter-derived target returns `{"exists": false}` after checking the resolved deepest existing ancestor and the recorded boundaries. Once the target appears, normal stat metadata is returned. A missing target behind an ancestor moved outside the bounds is refused, not reported absent. Reads and writes keep their existing behavior, including missing-root refusals. Parameters not used in any grant or header filter (`string`, `integer`, `enum`, or a `url` or `path` the script only reads) are passed to the script and limit nothing.

The code hash covers the manifest's exact bytes, placeholders and bounds included, so one approval covers exactly the bounds the approval card shows.

Header-filter placeholders must occupy the whole pattern string; interpolation
and non-string parameter types are refused at registration. At instantiate,
values used as patterns must also satisfy the ordinary **1–256 UTF-8 byte**
pattern limit, even if the parameter's string bound or enum permits more;
otherwise the refusal is `param_out_of_bounds`. The effective instance manifest
carries the bound pattern, and the instance id already covers its parameter
values. `*` crosses `/`; `repo` is ASCII-case-insensitive and other headers are
exact. These filters narrow admission cost, not authority: scripts must check
the verified body, since publisher headers can be wrong.

### What the approval card shows

Core renders the card from the manifest bytes the code hash covers, as for every install card. For a template it shows, besides the usual grants: each parameter with its type and bounds; for each placeholder, which grant or event-header filter it narrows (for example "fs.read: one path under ~/Work"); that a `url` parameter narrows to a host, not to that one URL; and `max_per_agent`, `max_live` and `max_lifetime`. One card covers one template version.

### One-shot schedules

`trigger.schedule.at` takes one timestamp and fires once, at that time (or once after downtime). A template may use it only through a `timestamp` parameter. Its schedule permits only default or explicit `missed: "once"`: `tz`, `each_cap`, `missed: "skip"` and `missed: "each"` are refused at registration with `template_placeholder_invalid`, naming the field. After the run for that fire ends, whatever its outcome, the instance ends: no more fires, as if its owner had ended it. A timestamp already in the past is refused with `param_out_of_bounds` on every `flow.instantiate`, including a same-value repeat or reopen. Watching again after that time needs a new future timestamp, and therefore a new id. Sub-millisecond timestamps are refused rather than rounded.

## Instances

An instance's flow id covers its template, its agent and its parameter values, so asking twice for the same watch returns the same instance:

```
instance flow id = <template> "_" <first 16 lowercase hex characters of h>
h = BLAKE3("basal-template-instance-v1\0" || u64_le(len template) || template
           || u64_le(len agent_id) || agent_id || u64_le(len P) || P)
```

`P` is the canonical JSON of the validated parameter values: object keys sorted by their UTF-8 bytes, no whitespace, strings escaped as JSON escapes them, integers without a fraction, and durations exactly as given. Paths and repositories use resolved absolute paths; timestamps use the normalized UTC spelling above. Thus `09:00:00Z` and `09:00:00.000+00:00` on the same date represent the same value and id; `15m` and `900s` remain different duration values. The domain tag differs from persona instances, so the two kinds of id can't collide, and `flow.install` refuses ids of this shape.

An instance:
- **runs as its creator:** every `$self` is the creator's `agent_id`;
- **gets its grants with placeholders replaced** by the validated values, and is authorized against that effective manifest, so a file-watch instance reads its own path and nothing else under the template's roots;
- **receives its values** as the activation input `params`, recorded with the run;
- **expires** at `expires_at`: after that, no run is admitted and its schedule stops; core refuses its writes after `expires_at` too; an expired instance is kept, not deleted;
- **stays on the version it was created from.** If core revokes that version, the instance stops. Creating it again with the same values on a newer approved version moves it there and keeps its state, as long as the values fit the newer version's bounds. If they don't, the call is refused with `param_out_of_bounds` and the instance stays as it was.

Templates and persona packages don't mix. A persona supplies no values, so `flow.instance.ensure` refuses a package with `params` (`package_has_params`), and `flow.instantiate` refuses a package without them (`package_not_template`).

## Operations

### `flow.instantiate`

Caller: an agent, with the agent taken from its route's daemon stamp, or the operator, naming `agent_id`.

Request:

```json
{"template": "string", "version": 1, "params": {}, "expires_in": "7d"}
```

`version` is required. An agent doesn't know template versions, so the component that relays its call fills in the newest version the operator has approved; basal refuses a call without one. `expires_in` is a duration, at most the template's `max_lifetime`.

Reply:

```json
{"flow_id": "string", "template": "string", "version": 1, "agent_id": "string",
 "params": {}, "expires_at": 1793001600000, "new": true}
```

`params` is echoed in canonical form, and `expires_at` is in Unix milliseconds. Calling again with the same template, agent and values is idempotent: it moves the instance to the given version, extends `expires_at` to the new `expires_in` without ever shortening it, and answers `new: false`.

### `flow.instance.end`

Request `{"flow_id": "string"}`, from the owning agent or the operator. Reply `{"flow_id": "string", "ended": true}`; a repeat returns `ended: false`. Ending is permanent for that id: no more runs and no schedule. The instance is kept for history. Creating the same valid values again later reopens the same id with an expiry that never shortens its previous expiry. Anyone else is refused with `not_permitted`. An existing ordinary flow or persona instance returns `package_not_template`; persona instances belong to the persona reconciler. An unknown id returns `not_found` to the operator, consistently with `flow.disable`; nonowners are refused before looking up template metadata.

### Script self-ending: `instance.end()`

A template run may call `await instance.end()` with no arguments. It returns `true` and records the request as a local journaled host call. The current run continues, and its instance ends when that run finishes, whether it succeeds, fails or is cancelled. Replay after a crash preserves the request without duplicating the effect. Only the calling run's own instance can end; ordinary flows and persona instances receive `not_a_template_instance`. A run from a previous, ended incarnation cannot end the instance if its owner has since reopened it. This supports event watches that report a condition once and then stop.

### Dry runs

An agent can test a registered template before creating an instance:

```json
{"flow_id": "watch", "version": 1, "params": {"path": "/workspace/project"}, "trigger": {}}
```

`version` is optional: a template name selects the newest registered version by default; an instance selects its approved version. Supply a synthetic `trigger`, or a schedule replay `window` such as `"6h"`. Basal validates supplied `params` through the same path as `flow.instantiate`, including canonical paths and repositories, timestamp rules and integer bounds. The script receives the canonical values as activation input `params`, and `self` is the calling agent. Grant and timing placeholders are replaced by those values.

Template dry runs are capture-only: authorized host calls, including reads, receive `captured` without execution; calls outside the effective grants are refused. `instance.end()` is also recorded as captured, both for a template with supplied values and for an existing template instance. It does not end an instance or request a local lifecycle change. An ordinary-flow dry run still refuses it with `not_a_template_instance`. No instance is created, no lifetime is needed, and neither `max_per_agent` nor `max_live` is consumed or checked. All run state is disposable, and the only result is the dry-run trace. A one-shot `at` template replays once immediately, not at its future timestamp; its schedule trigger carries `dry_run: true` and `due` is the request time.

Naming an existing template instance without `params` uses its bound values, recorded path boundaries and effective manifest. Its owning agent or the operator may do this; `self` remains the bound owner. Supplying `params`, even the same values or `null`, returns `params_not_allowed`. A registered template without `params` still returns `template_values_required`; basal never guesses sample values. Operators cannot dry-run a template package directly because they have no calling agent identity. `mode: "live"` is refused for both templates and instances.

Ordinary `flow.install` manifests carrying `params` or `template`, and ordinary-flow dry-run requests carrying `params`, return `params_require_package`, naming the field. The ordinary dry runner also refuses stored manifests carrying package-only fields.

### `flow.instance.live`

Caller: prefrontal-core only; anyone else is refused with `not_permitted`. Request `{}`. Reply:

```json
{"instances": [{"flow_id": "string", "template": "string", "version": 1,
                "agent_id": "string", "expires_at": 1793001600000, "seq": 3}]}
```

It lists every template instance that hasn't ended, including expired ones that haven't been ended, with each instance's current `seq` (see the events below).

### Stored-value compatibility and listing

A template version may declare optional `state_epoch`, an integer from 1 to
2147483647, default 1. It is in the hashed manifest and on the approval card;
instances share that declaration but record their epochs independently per flow
id. Existing state starts at epoch 1. At each instance's first admitted run of a
version with a different epoch, only that instance's `kv` is cleared and its
recorded epoch changes in the same transaction as admission. Earlier admitted
runs finish on their own version first. Approval is approval of the reset; there
is no separate card, and approval alone resets nothing. Rolling back to a
different epoch resets again. Token/rate windows, schedule positions, event
receipts/backlog, deduplication, journals and decision cards survive the reset.

`flow.list` and `flow.health` include each instance's recorded integer
`state_epoch`. Health adds `last_state_reset_at` (RFC 3339 UTC or null when never
reset). Core-only `package.get` includes declared `state_epoch` and, if instances
exist, `instance_state_epochs`, a map of retained instance flow id to recorded
integer epoch (including ended and expired instances). Without instances the
map is omitted. Core uses differing epochs to render "Upgrading clears this
flow's stored values (every instance)" on the version's card.

`flow.dry_run` always returns boolean `state_reset` alongside `summary`: true
when any retained instance differs from the selected template version's epoch,
or when the named instance differs; otherwise false, including templates with
no instances. This is a preview only and never resets real state.

### Listing, disabling and enabling

`flow.list` (scoped to the caller as for every flow) shows instances with `template`, `version`, `params` and `expires_at`. `flow.disable` and `flow.enable` work on an agent's own instances as on any of its flows.

### Refusals

| Code | When | Detail |
|---|---|---|
| `package_unknown` | that template version isn't registered | |
| `package_not_template` | the package has no `params` | |
| `agent_unknown` | core's agent registry answered, and no agent has the creating agent's stable id | |
| `agent_registry_unavailable` | core's agent registry could not be read (core restarting, a timeout, an unreadable reply); nothing is created, and the same call can be repeated | `{"retryable": true}` |
| `param_missing`, `param_unknown`, `param_out_of_bounds` | one parameter is missing, undeclared, or outside its bounds | `{"param": name, "bound": ...}`; parameters are checked in name order and the first failure is reported |
| `lifetime_too_long` | `expires_in` exceeds `max_lifetime` | |
| `instance_limit_reached` | the agent already has `max_per_agent` live instances of this template, or the template has `max_live` across all agents | `{"limit": n, "scope": "agent" \| "template"}`; expired and ended instances don't count |
| `template_placeholder_invalid` | at registration: a placeholder is in a position or of a type this document doesn't allow | names the field |
| `template_bound_invalid` | at registration: a bound breaks its rules | names the field |
| `package_has_params` | `flow.instance.ensure` names a template | |
| `params_require_package` | ordinary install or ordinary-flow dry run carries `params` or `template` | `{"field": "params" \| "template"}` |
| `template_values_required` | dry run names a registered template without `params` | |
| `params_not_allowed` | dry run of a template instance supplies `params` | |
| `not_a_template_instance` | a non-template run calls `instance.end()` | |
| `sink_instance_expired` | core receives a digest write at or after `expires_at` on its own clock | `{"flow_id": id, "expires_at_ms": ms}` |
| `sink_flow_not_installed` | core receives a write from an ended or revoked instance | |
| `not_found` | the operator ends an unknown flow id | |
| `not_permitted` | unauthorized caller or ending another agent's instance | |

When core refuses a sink write with `sink_instance_expired` or `sink_flow_not_installed`, basal rejects the script's sink call with that code unchanged. Basal does not retry: these are lifecycle refusals, not temporary bus or service failures.

`bound` is the full declaration (including `type`) for a missing or invalid declared value, and `null` for an unknown name. Bounds and unknown names are checked in one UTF-8 name order. A live instance for quota purposes is neither ended nor expired; a disable or revocation does not free its slot.

## Lifecycle events

An instance can end or expire without any reply passing through the component that tracks it: an operator ends it, or its expiry passes. So basal publishes a module event of module `basal` on each change, and declares all four in its daemon manifest:

| Event | Published when |
|---|---|
| `flow_instance_created` | an instance is created, or an ended one is reopened |
| `flow_instance_extended` | a repeat `flow.instantiate` moves its version or extends its expiry |
| `flow_instance_ended` | it is ended, or its one-shot fire has run |
| `flow_instance_expired` | its `expires_at` passes |

Each is published on `ck.<account>.event.basal.<event>.v1`, version 1, as a notice with an empty payload. The `Nats-Msg-Id` is `<flow_id>:<seq>`; `Ck-Content-Digest` is bare lowercase SHA-256 hex over the body's canonical JSON bytes. Notice headers `flow_id`, `agent` (the instance's `agent_id`) and `seq` let core discard stale notices without fetching a body. Lifecycle event bodies stay off the NATS bus because parameters can contain an agent's private note text.

The retrievable body is the JSON object:

```json
{"flow_id": "string", "template": "string", "version": 1, "agent_id": "string",
 "expires_at": 1793001600000, "seq": 3}
```

`flow_instance_expired` also includes `params`, the instance's canonical parameter object, so core can tell the instance's owning agent which watch parameters have expired.

### Body retrieval: `events_get`

Caller: prefrontal-core only. Request `{"event_key":"<flow_id>:<seq>","event_name":"flow_instance_expired","version":1}`. The integer `version` may be omitted for v1; lifecycle bodies have no other representation, so any other version receives `event_not_visible`. Reply `{"body":"<exact canonical JSON bytes>","digest":"<bare lowercase SHA-256 hex>","event_key":"<flow_id>:<seq>","event_name":"flow_instance_expired","published_at":1793001600000}`. `body` is a JSON string, not a parsed object. `published_at` is Unix milliseconds when basal recorded the publish acknowledgement; it is `null` while that local acknowledgement is pending, including after a crash between the server acknowledgement and its local recording. A notice's body is retrievable in that gap too. An unknown key and any non-core caller receive the same `event_not_visible` refusal with message `event not visible`. For core, an existing key with a different `event_name` is refused as `event_name_mismatch`, with message `event_name mismatch: expected <stored>, got <given>`. Non-core callers receive `event_not_visible` even when the name mismatches, so they learn nothing about the stored event.

Changes, sequence increments and immutable event bodies commit together in a durable outbox. Basal retries bus failures with exponential backoff and marks rows published only after a fresh or duplicate receipt. Published bodies remain retrievable for at least seven days after acknowledgement, then maintenance prunes them. Unpublished rows never expire.

### Ordering and recovery

`seq` strictly increases with every change to that instance, across all four events, so a reader drops any event whose `seq` is at or below the last one it applied for that `flow_id`. That makes redelivery and reordering harmless: the bus delivers at least once and can reorder across a reconnect. The message id (`Nats-Msg-Id`) is `<flow_id>:<seq>`, so the bus deduplicates a republished change.

The events don't enforce expiry; they confirm it:
- **A reader enforces `expires_at` itself, on its own clock,** refusing an instance's writes at the deadline whether or not `flow_instance_expired` has arrived, so a lagging bus can never extend a write window.
- **After a restart, a reader rebuilds from basal, not from the stream:** it subscribes to the four events first, then calls `flow.instance.live`, then applies buffered and later events whose `seq` is above the snapshot's. Subscribing first closes the gap where a change between the two would be missed. An instance the reader still tracks that is absent from the snapshot is treated as ended. The stream's retention therefore never matters.

## Test vectors

Instance id vectors are pinned in [`test-vectors/templates/instance-ids.json`](../test-vectors/templates/instance-ids.json), including canonical parameter JSON and full BLAKE3 hashes. [`generate.py`](../test-vectors/templates/generate.py) independently reproduces them with Python and the `blake3` package. `basal-core` tests every vector byte-for-byte. Lengths in the id formula count UTF-8 bytes, not characters.

| Template | Agent | Canonical `P` | Instance flow id |
|---|---|---|---|
| `watch` | `agent_47120287c700722b` | `{}` | `watch_78b36aaf3802c776` |
| `watch` | `agent_47120287c700722b` | `{"interval":"15m","path":"/work/notes/todo"}` | `watch_bab31709f57dda03` |
| `watch` | `agent_d1e7002e2c9b9f43` | `{"interval":"15m","path":"/work/notes/todo"}` | `watch_d5475a5580c9b252` |
| `watch` | `agent_47120287c700722b` | `{"interval":"900s","path":"/work/notes/todo"}` | `watch_9ab76103d538a9e1` |

The JSON corpus also pins quotes, backslashes and non-ASCII UTF-8 text, and equivalent UTC timestamp spellings. Captured lifecycle notice subjects, headers and digests, `events_get` request/reply bytes, and a `flow.instance.live` request and reply are pinned in [`lifecycle.json`](../test-vectors/templates/lifecycle.json); the module tests produce them through the real NATS publisher and dispatcher.
