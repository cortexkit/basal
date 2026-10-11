# Flow templates and agent-created instances

A template lets an agent say "tell me when X happens" without asking the operator each time. The operator approves a template version once, together with the bounds of what agents may ask it to watch. After that, any agent creates instances of it with its own parameter values, and no approval card is raised.

Typical templates:
- a file appears, or contains or lacks some text;
- a git commit, tag or branch moves;
- a URL's content changes;
- a pull request or issue changes state;
- a time is reached.

A template is a flow package ([packages.md](packages.md)) whose manifest declares parameters. Everything packages already do still holds: an immutable registry, one approval per version, `$self` for the agent, and instances that are flows with their own state. This document adds parameters, the bounds that check them, the operations agents and core call, and the events basal publishes about instances.

## Parameters in the manifest

A template manifest adds a `params` object, mapping a parameter name to its declaration. Names are 1 to 32 bytes of `a-z`, `0-9` and `_`, with at most 16 parameters. Each declaration has a `type` and the bounds for that type. Every bound shown is required unless marked optional.

| `type` | Value | Bounds |
|---|---|---|
| `path` | an absolute path, after `~` expansion | `under`: a list of roots. The value must resolve inside one of them. |
| `repo` | an absolute path to a git repository | `under`: a list of roots, as for `path`. |
| `url` | an `https` URL | `hosts`: allowed hosts. The URL's host must be one of them. No wildcards and no IP literals, as for `net.fetch`. |
| `duration` | a duration string, such as `"15m"` | `min` and `max`. |
| `integer` | an integer | `min` and `max`. |
| `string` | a string | `max_bytes`, from 1 to 1024. No control characters. |
| `enum` | one of the listed strings | `values`: 1 to 32 strings. |
| `timestamp` | RFC 3339 in UTC, such as `"2026-11-01T09:00:00Z"` | `max_ahead`: how far in the future it may be. |

The manifest also gains a `template` object, required whenever `params` is present:
- `max_per_agent`: 1 to 100. The most live instances of this template one agent may have.
- `max_live` (optional): 1 to 10,000. The most live instances of this template across all agents.
- `max_lifetime`: a duration. Every instance expires, and none may outlive this.

A `duration` parameter's `min` must be at least basal's minimum schedule interval; a template whose `min` is lower is refused at registration with `template_bound_invalid`.

### Parameters only narrow

A parameter may appear in a grant only where it narrows what the template's own grants already allow. A placeholder is the whole string `"$params.<name>"`, and is allowed only in these positions:

| Position | Parameter types allowed | Meaning for the instance |
|---|---|---|
| `fs.read[]`, `fs.write[]` | `path` | The instance may read or write under exactly that path. |
| `git.read[]` | `repo` | The instance may read exactly that repository. |
| `net.fetch[].host` | `url` | The instance may fetch from that URL's host. |
| `trigger.schedule.interval` | `duration` | The instance's cadence. |
| `trigger.schedule.at` | `timestamp` | A one-shot fire (see below). |

At registration, basal checks every placeholder: it names a declared parameter; that parameter's type fits the position; a `path` or `repo` placeholder sits in a grant list, and its `under` roots meet that list's rules (absolute, existing at registration); and a `url` parameter's `hosts` meet `net.fetch`'s host rules. Anything else is refused with `template_placeholder_invalid`, naming the field. Grants without placeholders apply to every instance as written.

**Paths are canonical.** A `path` or `repo` value is canonicalized, with symlinks resolved, before the `under` check, and the instance's effective grant is the resolved path; otherwise a symlink under a root could point outside it. If the path doesn't exist yet, basal resolves its deepest existing ancestor and checks that. The file and git built-ins also check containment at every access, so a symlink created later can't escape either. Parameters not used in any grant (`string`, `integer`, `enum`, or a `url` or `path` the script only reads) are passed to the script and limit nothing.

The code hash covers the manifest's exact bytes, placeholders and bounds included, so one approval covers exactly the bounds the approval card shows.

### What the approval card shows

Core renders the card from the manifest bytes the code hash covers, as for every install card. For a template it shows, besides the usual grants: each parameter with its type and bounds; for each placeholder, which grant it narrows (for example "fs.read: one path under ~/Work"); that a `url` parameter narrows to a host, not to that one URL; and `max_per_agent`, `max_live` and `max_lifetime`. One card covers one template version.

### One-shot schedules

`trigger.schedule.at` takes one timestamp and fires once, at that time. A template may use it only through a `timestamp` parameter. After the run for that fire ends, whatever its outcome, the instance ends: no more fires, as if its owner had ended it. A timestamp already in the past when the instance is created is refused with `param_out_of_bounds`; it never fires immediately.

## Instances

An instance's flow id covers its template, its agent and its parameter values, so asking twice for the same watch returns the same instance:

```
instance flow id = <template> "_" <first 16 lowercase hex characters of h>
h = BLAKE3("basal-template-instance-v1\0" || u64_le(len template) || template
           || u64_le(len agent_id) || agent_id || u64_le(len P) || P)
```

`P` is the canonical JSON of the parameter values: object keys sorted by their UTF-8 bytes, no whitespace, strings escaped as JSON escapes them, integers without a fraction, and durations and timestamps exactly as given after validation. The domain tag differs from persona instances, so the two kinds of id can't collide, and `flow.install` refuses ids of this shape.

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

Request `{"flow_id": "string"}`, from the owning agent or the operator. Ending is permanent for that id: no more runs and no schedule. The instance is kept for history. Creating the same values again later reopens the same id with a new expiry. Anyone else is refused with `not_permitted`.

### `flow.instance.live`

Caller: prefrontal-core only; anyone else is refused with `not_permitted`. Request `{}`. Reply:

```json
{"instances": [{"flow_id": "string", "template": "string", "version": 1,
                "agent_id": "string", "expires_at": 1793001600000, "seq": 3}]}
```

It lists every template instance that hasn't ended, including expired ones that haven't been ended, with each instance's current `seq` (see the events below).

### Listing, disabling and enabling

`flow.list` (scoped to the caller as for every flow) shows instances with `template`, `version`, `params` and `expires_at`. `flow.disable` and `flow.enable` work on an agent's own instances as on any of its flows.

### Refusals

| Code | When | Detail |
|---|---|---|
| `package_unknown` | that template version isn't registered | |
| `package_not_template` | the package has no `params` | |
| `param_missing`, `param_unknown`, `param_out_of_bounds` | one parameter is missing, undeclared, or outside its bounds | `{"param": name, "bound": ...}`; parameters are checked in name order and the first failure is reported |
| `lifetime_too_long` | `expires_in` exceeds `max_lifetime` | |
| `instance_limit_reached` | the agent already has `max_per_agent` live instances of this template, or the template has `max_live` across all agents | `{"limit": n, "scope": "agent" \| "template"}`; expired and ended instances don't count |
| `template_placeholder_invalid` | at registration: a placeholder is in a position or of a type this document doesn't allow | names the field |
| `template_bound_invalid` | at registration: a bound breaks its rules | names the field |
| `package_has_params` | `flow.instance.ensure` names a template | |
| `not_permitted` | ending another agent's instance, or calling `flow.instance.live` as anyone but core | |

## Lifecycle events

An instance can end or expire without any reply passing through the component that tracks it: an operator ends it, or its expiry passes. So basal publishes a module event of module `basal` on each change, and declares all four in its daemon manifest:

| Event | Published when |
|---|---|
| `flow_instance_created` | an instance is created, or an ended one is reopened |
| `flow_instance_extended` | a repeat `flow.instantiate` moves its version or extends its expiry |
| `flow_instance_ended` | it is ended, or its one-shot fire has run |
| `flow_instance_expired` | its `expires_at` passes |

Each is published on `ck.<account>.event.basal.<event>.v1`, version 1. The message body is the JSON object:

```json
{"flow_id": "string", "template": "string", "version": 1, "agent_id": "string",
 "expires_at": 1793001600000, "seq": 3}
```

`seq` strictly increases with every change to that instance, across all four events, so a reader drops any event whose `seq` is at or below the last one it applied for that `flow_id`. That makes redelivery and reordering harmless: the bus delivers at least once and can reorder across a reconnect. The message id (`Nats-Msg-Id`) is `<flow_id>:<seq>`, so the bus deduplicates a republished change.

The events don't enforce expiry; they confirm it:
- **A reader enforces `expires_at` itself, on its own clock,** refusing an instance's writes at the deadline whether or not `flow_instance_expired` has arrived, so a lagging bus can never extend a write window.
- **After a restart, a reader rebuilds from basal, not from the stream:** it subscribes to the four events first, then calls `flow.instance.live`, then applies buffered and later events whose `seq` is above the snapshot's. Subscribing first closes the gap where a change between the two would be missed. An instance the reader still tracks that is absent from the snapshot is treated as ended. The stream's retention therefore never matters.

## Test vectors

Instance id vectors and captured request, reply and event bytes are pinned under `test-vectors/templates/` once the implementation lands; other components copy them byte for byte.
