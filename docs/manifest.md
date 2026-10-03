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
| `id` | string | yes | | The flow id: 1 to 63 bytes of `a-z`, `0-9`, `-`, `_`. Stable across versions; `kv`, the token windows, the rate limits and the owner are per flow id. |
| `version` | integer | yes | | At least 1. A new install must be above the approved version. |
| `purpose` | string | yes | | 1 to 1024 bytes, no control characters except newline. Shown on the card. |
| `trigger` | object | yes | | Exactly one of `events` and `schedule` (below). |
| `sinks` | list of sink grants | no | `[]` | Agents whose digest the flow may write. |
| `status` | list of agent names | no | `[]` | Agents whose status line the flow may set. |
| `claims` | list of claims | no | `[]` | Source kinds the flow takes over from core for an agent. |
| `ops` | list of op references | no | `[]` | Every module op the flow may call. Anything else is refused at run time. |
| `facts` | facts grant | no | none | Whose facts the flow may read. Without it, `facts()` is refused. |
| `llm` | model grant | no | none | Without it, `llm()` and `classify()` are refused. |
| `placement` | string | no | none | Where the flow runs, such as `"machine:studio"`. 1 to 128 bytes, no control characters. Shown on the card. |
| `concurrency` | integer | no | `1` | Only `1`: runs of a flow run one at a time, in trigger order. |
| `deadline` | duration | no | the runtime's default (10 minutes) | The per-run wall-clock deadline, from `"1s"` to `"24h"`. |

Names: module ids use `a-z`, `0-9`, `-`, `_`; op names add `A-Z` and `.` (`browser.read_page`; the pair, never the dots, identifies the op); event names and source kinds are one NATS subject token, `a-z`, `0-9`, `_`; agent names use letters, digits, `-` and `_`. Every name is 1 to 128 bytes.

A duration is a positive whole number followed by one unit: `s`, `m`, `h` or `d` (`"90s"`, `"10m"`, `"6h"`, `"1d"`).

### `trigger`

- `{ "events": [ { "module": "plexus", "name": "pull_request_review", "version": 1 }, ... ] }`: at least one event. Install refuses an event or version the catalog does not declare, and an event whose `resolve_op` is not a query.
- `{ "schedule": { ... } }`: the scheduler's `ScheduleSpec` (`crates/basal-core/src/schedule/spec.rs`), decoded with unknown fields refused and compiled by `schedule::validate` when the manifest is checked. Exactly one of `cron` and `interval`:

  | Field | Type | Default | Limits and meaning |
  |---|---|---|---|
  | `cron` | string | | A five-field pattern (minute granularity; no seconds or years field) that matches at least one date. |
  | `tz` | string | `UTC` | With `cron` only: a canonical IANA zone name, spelt exactly (`Europe/Madrid`). A time in a spring-forward gap fires once at the transition; a time in a fall-back overlap fires once, at its first occurrence. |
  | `interval` | duration | | `"60s"` to `"365d"`; due times are the approval instant (to the second) plus whole periods. |
  | `missed` | `"once"`, `"skip"` or `"each"` | `"once"` | What happens to due times that passed while basal was not ticking (asleep, stopped, or more than the grace period late): one catch-up fire, none, or one each. |
  | `each_cap` | integer | 3 | With `each` only: 1 to 10; the newest missed due times are kept. |

  The newest due time is on time when it is at most the grace period (60 s by default) old; every older due time in the window is missed and follows `missed`. `once` fires one catch-up identified by the last missed due time; `each` keeps the newest `each_cap` missed due times and fires them oldest first. An on-time due time always fires, after any catch-up. A fire's trigger id is `schedule:<due time, RFC 3339 UTC>` (`schedule:2026-03-29T01:00:00Z`), and admission deduplicates on flow and trigger id, so one due time starts at most one run. The script sees `trigger` as `{ "kind": "schedule", "due": "..." }`; a catch-up fire adds `"catch_up": { "policy", "missed_count", "missed_window": { "first", "last" } }`, counting every missed due time, including those `each_cap` dropped. A scheduled fire, like every trigger, runs the flow's version approved and enabled when it is admitted.
- Both, or neither, is refused.

### Sink grant

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `agent` | agent name | yes | | Must be a known agent. |
| `digest_max` | `"silent"`, `"piggyback"` or `"wake"` | yes | | The most intrusive action the flow may request for this agent. An omitted action uses this approved cap, in both live dispatch and dry runs. A `sink.digest` call asking for more is refused. |
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

### Facts grant

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `targets` | list of agent names | yes | | Agents whose `facts()` the flow may read. Each must be known. |
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
