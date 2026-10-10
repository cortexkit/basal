# Agent-facing basal operations

Core's scoped relay exposes these five operations: `flow.install`, `flow.dry_run`, `flow.disable`, `flow.enable` and `flow.list`. Caller identity comes from the daemon's route stamp, never request parameters. `flow.health` is not relayed: core uses it to decide whether a flow's claim to replace a source remains healthy, and the operator uses its runtime-wide figures.

The codemode operations `codemode.run`, `codemode.result` and `codemode.cancel` are core-only and not agent-relayed. See [codemode operations](#codemode-operations).

Package management is not agent-relayed: `package.register` is available to the attested operator and core; `package.get`, `flow.instance.ensure`, and `flow.instance.remove` are core-only. The attested operator is a caller on a route the daemon stamps as the reserved `callosum` module, the operator's own module. A plain local caller is a process holding the daemon's direct connection key, on a route with no scope, which the daemon cannot vouch for. A plain local caller refused any of these receives `operator_attestation_required`, consistently with the other management operations; other unauthorized callers receive `not_permitted`. See [package manifests](packages.md).

Shapes below use type names, `|` for alternatives, and a one-element array to describe each array item. `?` on a request key means optional. Reply keys are always present, except in the codemode result shape, where `?` marks a key that is omitted when absent. `object` and `any` describe open JSON values.

## flow.install

Params:
```json
{"script":"string","manifest":"string","author?":"string|null","loop_override?":"boolean"}
```
`manifest` is the exact manifest text, not a JSON object; its bytes enter the code hash. `author` defaults to the caller (operator defaults to `operator`); agents can author only for themselves and local callers only as `local:unverified`. Local callers share the unverified author identity `local:unverified`, cannot replace another author's flow, and must declare at least one digest sink so core can route the approval card. Only the operator can set `loop_override` true (bypassing the loop-install refusal).

Reply:
```json
{"flow_id":"string","version":"integer","code_hash":"string","card_id":"string","state":"pending|approved|rejected|stale","new":"boolean","warnings":"any","dry_run":"any"}
```
`warnings` is an array of warning objects (`kind`, `module`, `op`, all strings), or null if unavailable. `dry_run` has the same summary shape documented below for `flow.dry_run`, or `{error: string}` if the capture failed, or null if unavailable.

Installation records a version and raises its consent card, not approval. If consent delivery fails, the op returns an error although the version and card may already be recorded; retrying the same install retries delivery.

A manifest containing `$self` in any agent field is refused with `self_requires_package`: `$self` is available only in [package manifests](packages.md).

An agent-owned flow may name only its author in `sinks[].agent`, `status[]`, `claims[].agent` and `facts.targets[]`, even if the operator installs it in that agent's name. Manifest agents resolve through the catalog from display names or stable ids; each must resolve to the author's stable `agent_id`. A foreign target is refused with code `foreign_agent_target` and a message naming the field, target agent and author; no version or card is recorded or raised. Global flows (`author: operator`) and local installs may name any known agent. Ids ending in `_` and exactly 16 lowercase hex characters are refused with `flow_id_reserved` before recording or raising anything, because that namespace is reserved for package instances.

## flow.dry_run

Params:
```json
{"flow_id":"string","version?":"integer|null","mode?":"capture|live|null","trigger?":"any","window?":"string|null"}
```
Absent mode means capture. Absent version selects the approved version, otherwise the newest installed version. Absent trigger replays schedule fires over the time window ending now; `window` is its duration, such as `6h`. Capture is available to the operator and owning agent; live only to the operator. Local callers are refused.

Dry run validates the stored manifest under its author with the same self-only rule and `foreign_agent_target` refusal as install, including versions recorded before the rule was enforced.

Reply:
```json
{"flow_id":"string","version":"integer","summary":"any"}
```
The summary is the dry-run trace, including calls and sink writes. Its journal uses an isolated scratch store; live mode dispatches eligible query ops to real hosts and is not a guarantee of no external effects. Its fields are:
```json
{"mode":"capture|live","window":"window shape below","partial":"boolean","runs":[{"run_id":"string","trigger_id":"string","trigger":"any","state":"string","result":"any|null","error":{"kind":"string","detail":"string|null"},"ending":"string|null","partial":"boolean","calls":[{"position":"integer","kind":"string","module":"string|null","op":"string|null","args":"any","outcome":"outcome shape below|null","action":"live|captured|local|refused","partial":"boolean","sink?":{"agent":"string","requested":"string|null","cap":"string|null","effective":"string|null","policy":"string"}}],"calls_total":"integer"}]}
```
`error` may be null. If a replayed trigger is refused before a run can be admitted (for example, by a concurrency limit), its runs-array entry instead contains `{trigger_id: string, trigger: any, admission: string}`. `sink` is present only for digest calls. Large `args` are represented as `{truncated: string, bytes: integer}`. Outcome is `{fulfilled: true}` or `{rejected: any}` (the rejection code extracted from the result, or null); null outcome means no outcome. `calls` is capped at the configured listing limit (default 200); `calls_total` counts all calls. `window` is one of these shapes (times here are RFC 3339 strings):
```json
{"kind":"synthetic"}
```
```json
{"kind":"events","covered":"null","note":"string"}
```
```json
{"kind":"schedule","requested":{"from":"string","to":"string"},"covered":{"from":"string","to":"string"},"due_times_in_window":"integer","due_times_is_lower_bound":"boolean","replayed":"integer","capped":"boolean"}
```
A schedule's `covered` may be null when no fires are replayed. `partial` marks traces affected by simulated calls: capture rejects external reads and mutations rather than executing them, so subsequent script branches may differ from a live run. It does not imply live effects.

## flow.disable

Params:
```json
{"flow_id":"string","reason?":"string|null"}
```
The operator or owning agent may disable. Absent reason becomes `disabled by request`.

Reply:
```json
{"flow_id":"string","state":"disabled","changed":"boolean"}
```

## flow.enable

Params:
```json
{"flow_id":"string"}
```
The operator may enable a flow unless it involves a retired agent. An owning agent may undo its own disable, or a `grant_lost` pause after restoration polling was stopped; it cannot undo an operator or automatic disable. Local callers are refused. A standing retirement returns the typed `agent_retired` refusal, including retirement evidence in `detail`, even to the operator.

Reply:
```json
{"flow_id":"string","state":"enabled","changed":"boolean"}
```

## flow.list

Params:
```json
{"flow_ids?":"array of string|null"}
```
Only an object is accepted, with no other keys. Omitted or null `flow_ids` means all visible flows; an empty array selects none. The operator sees all flows; an agent whose core-owned scope is vouched for by the daemon sees only its own; a local caller sees only flows authored as `local:unverified`. Core and other callers are refused. Requested invisible and nonexistent ids are both silently absent. Entries are ordered by flow id.

The reply shape below is validated by `crates/basal-module/tests/list_contract.rs`.

Reply (machine-checked by `list_contract`):
```json
{"as_of":"integer","flows":[{"flow_id":"string","state":"enabled|disabled|unapproved","approved_version":"integer|null","pending_version":"integer|null","disabled":{"by":"operator|owner|auto|core","reason":"string","at":"integer"},"last_run":{"run_id":"string","state":"string","ended_at":"integer"},"needs_reconcile":"boolean","grant_losses":[{"flow_id":"string","provider":"string","grant":"string","grant_label":"string","echoable":"boolean","run_id":"string","version":"integer","revision":"integer","state":"polling|stopped","failures":"integer","next_poll_at_ms":"integer","lost_at_ms":"integer"}],"agent_retirement":{"agent_id":"string|null","agent_reference":"string|null","provider":"string","action":"string","at_ms":"integer"}}]}
```
`disabled` and `last_run` may each be null, but their keys must be present. All times are Unix epoch milliseconds. `pending_version` is the newest version with an open card, or null; `approved_version` is null until approval. `state` uses the same approval-first computation as `flow.health`. `last_run` is the most recent finished run, not an in-flight run. `needs_reconcile` is true if any run awaits reconciliation. The disable's actor kind, reason and time are the recorded disable, or null after enabling. `by` is `operator`, `owner` (the owning agent), `auto` (the runtime's loop protection) or `core`. A `core` disable may mean core revoked the version, holds no install of that version, or approved a different code hash for it; it may also mean `grant_lost` or `agent_retired`. Losing core's approval also clears the approved version, so the flow lists as `unapproved`; approval of a newer version clears that stop, but does not override an operator or owner stop.

## Codemode operations

Codemode runs one short JavaScript program that calls tools from a catalog and returns one result. prefrontal-core owns the agent-facing tool, builds the catalog and issues the run's scope; basal runs the program in a fresh confined worker and dispatches each tool call on that scope.

`codemode.run`, `codemode.result` and `codemode.cancel` are core-only and not agent-relayed: only prefrontal-core, on its own route stamped `reserved:prefrontal-core`, may call them, and core's scoped relay never forwards them for an agent. A plain local caller is refused `operator_attestation_required`; the operator, agents and every other caller are refused `not_permitted`. A refusal is an op error with a code and a message and has no result body. Every success reply is the [codemode result](#codemode-result).

## codemode.run

Core-only, not agent-relayed. Kind: mutate.

Params:
```json
{"run_id":"string","agent_id":"string","program":"string","catalog":[{"name":"string","input_schema":"any","module":"string","op":"string"}],"limits?":{"wall_ms?":"integer","tool_calls?":"integer","output_bytes?":"integer"},"description?":"string","scope":{"owner":"object","ref":"string","epoch":"integer"},"deadline_ms":"integer"}
```
- `program` is a function body, wrapped the way flow scripts are; its return value is the run's `value`. It sees only `tools` (one async function per catalog `name`), `console.log` and the frozen JavaScript intrinsics.
- `catalog` entries have exactly these four keys. `op` is the tool name sent to `module`'s tool provider; the program calls `tools[name]`. `input_schema` is a JSON Schema draft 2020-12 that checks every input before it is sent; it may refer only to itself (`#` or `#/...`). An empty array is valid.
- `limits` may hold only `wall_ms`, `tool_calls` (1 to 200, default 200) and `output_bytes` (1 to 65,536, default 65,536). The run's wall deadline is `min(deadline_ms, admission time + wall_ms)`; omitted `wall_ms` means `deadline_ms`.
- `description` is optional: the one-line summary the agent wrote, which the person's phone shows. It is a string of at most 1,024 UTF-8 bytes with no line break (U+000A, U+000D, U+2028 or U+2029). basal stores it with the run and `codemode.result` returns it unchanged. It plays no part in idempotency.
- `scope` is the live scope core issued for this run: `owner` is a reserved principal (`{"kind":"reserved","module_id":"..."}`), `ref` is non-blank, and `epoch` is the scope epoch. basal asks the daemon to describe the scope and admits the run only if it is live at that epoch and its attested `agent_id` and `run_id` equal the request's. Every tool call is dispatched on that scope, so the daemon stamps it on each call.
- `deadline_ms` is an absolute Unix-millisecond time on basal's runtime clock.

The reply is returned as soon as the run is recorded; the program keeps running if the caller disconnects. Its `status` is `running`, or terminal if the run ended at admission. A run admitted at or after `deadline_ms` is recorded `budget_exhausted:wall` with `duration_ms` 0 and never starts a worker.

Admission checks run in this order; the first failure refuses with the code shown and records nothing:
1. A known `run_id` returns the stored run, whatever the other parameters are (program, catalog, scope, limits or description), with its admitted `catalog_digest` and `description`. It never starts a second run or sends a call. A `run_id` pruned by retention is refused `unknown_run`.
2. `unsupported_platform` on Windows.
3. `no_scope`: `scope` missing or malformed, an owner that is not a reserved principal, or a blank `ref`.
4. `no_scope` when the daemon's description of the scope is not `live` at `epoch`; `scope_mismatch` when its attested `agent_id` or `run_id` differs from the request's.
5. `invalid_request`: a missing or non-string `run_id`, `agent_id` or `program`; a missing `catalog`; a missing or non-integer `deadline_ms`; a `program` over 1 MiB; a non-string, over-long or multi-line `description`.
6. `invalid_limits`: an unknown key, a non-integer, a value below 1, or a value above its maximum.
7. `invalid_catalog`: not an array; an entry with a missing or extra key; a duplicate `name`; a `name` not matching `^[A-Za-z_][A-Za-z0-9_]*$`, longer than 128 bytes, or one of `__proto__`, `constructor`, `prototype`, `tools` and `console`; an empty or over-128-byte `module` or `op`; an `input_schema` that does not compile or refers outside itself.
8. `busy`: the attested agent already has 2 running codemode runs, or basal has 16.

Reply: the [codemode result](#codemode-result).

## codemode.result

Core-only, not agent-relayed. Kind: query.

Params:
```json
{"run_id":"string"}
```
Returns the run's current or final state. An unknown run id, a flow run id, or a run pruned 24 hours after it ended is refused `unknown_run`; a non-string `run_id` is refused `invalid_request`.

Reply: the [codemode result](#codemode-result).

## codemode.cancel

Core-only, not agent-relayed. Kind: mutate.

Params:
```json
{"run_id":"string"}
```
An unknown or pruned run id is refused `unknown_run`. A run that has already ended returns its stored state unchanged. A running run is stopped: its worker is killed, its queued calls become `cancelled`, calls already sent without a recorded answer become `outcome_unknown` / `no_outcome`, its scope is released, and it ends `cancelled`. An answer that arrives after the cancel is not recorded.

Reply: the [codemode result](#codemode-result).

## Codemode result

Result (machine-checked by `codemode_results_decode_against_documented_shape`):
```json
{"status":"running|completed|failed|budget_exhausted:js_cpu|budget_exhausted:memory|budget_exhausted:stack|budget_exhausted:wall|budget_exhausted:tool_calls|cancelled|interrupted","value?":"any","error?":{"code":"string","message":"string"},"output":"string","calls":[{"tool":"string","outcome":"pending|ok|error|refused|consent_unavailable|tool_unavailable|outcome_unknown|cancelled","code?":"string","duration_ms?":"integer"}],"warnings":[{"code":"output_truncated","message":"string"}],"catalog_digest":"string","description?":"string","duration_ms":"integer"}
```
Keys marked `?` are omitted when absent, never null:
- `value` is the program's JSON return value, present only when `status` is `completed`.
- `error` is `{code, message}`, present exactly when `status` is `failed`, `budget_exhausted:*` or `interrupted`, with a code from the terminal table below and a non-empty message.
- `description` is present only when the admitted run carried one.
- In each call, `code` is present only for the outcomes that carry one (see the call outcomes table), and `duration_ms` only once basal began dispatching the call to its provider.

The other keys are always present:
- `output` is the kept `console.log` text: each call's arguments (strings as they are, other values as JSON) joined by one space, with a newline after each line. Lines are kept while the total stays within the output budget; the line that would cross it and every later line are dropped, with one `output_truncated` warning. Output kept before a cancel or a kill is still returned.
- `calls` lists every tool call in the order the program made them. Provider answers are never returned. A call's `duration_ms` is runtime-clock milliseconds from dispatch to its recorded outcome, or to the run's end for a call that ends `outcome_unknown` / `no_outcome`; time spent queued is excluded.
- `warnings` is an array of `{code, message}`; the only code is `output_truncated`, at most once.
- `catalog_digest` is the lowercase hex BLAKE3-256 of the admitted catalog's RFC 8785 canonical JSON, entries sorted by `name`. See [catalog digest vectors](#catalog-digest-vectors).
- `duration_ms` is runtime-clock milliseconds since admission, frozen when the run ends.

### Terminal codes

| Terminal path | `status` | `error.code` |
| --- | --- | --- |
| The program returned a JSON value of at most 16,384 bytes | `completed` | none |
| A budget was exhausted (JS CPU, memory, stack, wall or tool calls) | `budget_exhausted:<kind>` | equal to `status` |
| The worker made a call other than a tool call | `failed` | `profile_violation` |
| The worker's prelude differs from basal's | `failed` | `engine_mismatch` |
| A tool input over 1 MiB | `failed` | `arguments_too_large` (the message has the size) |
| A return value over 16,384 bytes of JSON | `failed` | `result_too_large` (the message has the size) |
| A return value that is not JSON | `failed` | `result_not_json` |
| An uncaught error in the program | `failed` | `script` |
| The program awaits a promise no call will settle | `failed` | `stalled` |
| Any other engine failure | `failed` | `engine_error` |
| The worker exited, or could not start, without basal stopping it | `failed` | `worker_lost` |
| The scope closed and a later call was refused | `interrupted` | the scope reason: `scope_ended`, `scope_not_live`, `scope_not_synced` or `scope_changed` |
| basal restarted while the run was running | `interrupted` | `basal_restarted` |
| `codemode.cancel` | `cancelled` | none |

A worker basal stopped itself (a cancel, the wall deadline, or any other end of the run) is never reported `worker_lost`.

### Call outcomes

| Outcome | Produced when | `code` | Ends the run |
| --- | --- | --- | --- |
| `pending` | queued, or sent with no answer yet; only while the run is `running` | none | no |
| `ok` | the provider answered with a value of at most 1 MiB | none | no |
| `error` | the provider answered with a value over 1 MiB, or refused the call | `value_too_large`, or the provider's refusal code | no |
| `refused` | basal refused the call before sending it | `unknown_tool`, `invalid_input`, `shell_capable`, `not_in_catalog` or `queue_full` | no |
| `refused` | the daemon refused the call because the scope closed | `scope_ended`, `scope_not_live`, `scope_not_synced` or `scope_changed` | yes, `interrupted` |
| `consent_unavailable` | the provider could not obtain consent | `consent_unavailable` | no |
| `tool_unavailable` | the call could not be routed to a ready provider | `tool_unavailable` | no |
| `outcome_unknown` | the connection was lost, the reply timed out, or the reply could not be read | `connection_lost`, `reply_timeout` or `reply_unreadable` | no |
| `outcome_unknown` | the run ended after the call was sent and before an answer was recorded | `no_outcome` | already ended |
| `cancelled` | the run ended while the call was still queued | none | already ended |

Each call is sent at most once and never retried; an unknown outcome is reported, never re-sent. A call outcome other than `ok` that does not end the run rejects the program's promise with an `Error` carrying `code`, `tool` and `outcome`, which the program can catch.

## Catalog digest vectors

`catalog_digest` is the lowercase hex BLAKE3-256 of the catalog array's RFC 8785 (JCS) canonical JSON, with its entries sorted by `name`. Canonical JSON sorts object members by name, has no whitespace, writes numbers the way JavaScript does (`1.0` is `1`, `1e21` is `1e+21`), writes non-ASCII text as itself and escapes control characters. Core computes the same digest; these vectors pin it. Each shows a catalog as sent, its canonical JSON, and its digest. `crates/basal-core/tests/codemode_digest_vectors.rs` reproduces every vector with its own canonical JSON and BLAKE3 code and checks basal's digest against them.

### Vector `base`

```json
[{"name":"find","module":"search","op":"search.find","input_schema":{"type":"object","properties":{"q":{"type":"string","maxLength":200}},"required":["q"]}},{"name":"read","module":"notes","op":"notes.read","input_schema":{"type":"object","properties":{"id":{"type":"integer","minimum":1}}}}]
```
```text
[{"input_schema":{"properties":{"q":{"maxLength":200,"type":"string"}},"required":["q"],"type":"object"},"module":"search","name":"find","op":"search.find"},{"input_schema":{"properties":{"id":{"minimum":1,"type":"integer"}},"type":"object"},"module":"notes","name":"read","op":"notes.read"}]
```
Digest: `aa6c1832de7397efb328c47d6c9d3dbfc15f09294ba77d84424fae2ed8537697`

### Vector `keys-reordered`

The `base` catalog with the members of every object in another order; the digest is the same.

```json
[{"op":"search.find","input_schema":{"required":["q"],"properties":{"q":{"maxLength":200,"type":"string"}},"type":"object"},"name":"find","module":"search"},{"module":"notes","input_schema":{"properties":{"id":{"minimum":1,"type":"integer"}},"type":"object"},"op":"notes.read","name":"read"}]
```
```text
[{"input_schema":{"properties":{"q":{"maxLength":200,"type":"string"}},"required":["q"],"type":"object"},"module":"search","name":"find","op":"search.find"},{"input_schema":{"properties":{"id":{"minimum":1,"type":"integer"}},"type":"object"},"module":"notes","name":"read","op":"notes.read"}]
```
Digest: `aa6c1832de7397efb328c47d6c9d3dbfc15f09294ba77d84424fae2ed8537697`

### Vector `entries-reordered`

The `base` catalog with its entries in the other order; the digest is the same.

```json
[{"name":"read","module":"notes","op":"notes.read","input_schema":{"type":"object","properties":{"id":{"type":"integer","minimum":1}}}},{"name":"find","module":"search","op":"search.find","input_schema":{"type":"object","properties":{"q":{"type":"string","maxLength":200}},"required":["q"]}}]
```
```text
[{"input_schema":{"properties":{"q":{"maxLength":200,"type":"string"}},"required":["q"],"type":"object"},"module":"search","name":"find","op":"search.find"},{"input_schema":{"properties":{"id":{"minimum":1,"type":"integer"}},"type":"object"},"module":"notes","name":"read","op":"notes.read"}]
```
Digest: `aa6c1832de7397efb328c47d6c9d3dbfc15f09294ba77d84424fae2ed8537697`

### Vector `trailing-zero`

`1.0` is written `1`.

```json
[{"name":"scale","module":"math","op":"math.scale","input_schema":{"type":"number","multipleOf":0.5,"maximum":1.0}}]
```
```text
[{"input_schema":{"maximum":1,"multipleOf":0.5,"type":"number"},"module":"math","name":"scale","op":"math.scale"}]
```
Digest: `b3f666837a0021f0cdb2fb3815d50858d69577993c4ee657ab2cdbee99eba9a0`

### Vector `exponent`

Numbers at or above 1e21, and below 1e-6, take the exponent form with a lowercase `e` and an explicit sign.

```json
[{"name":"measure","module":"lab","op":"lab.measure","input_schema":{"type":"number","minimum":2.5E-7,"maximum":1e21}}]
```
```text
[{"input_schema":{"maximum":1e+21,"minimum":2.5e-7,"type":"number"},"module":"lab","name":"measure","op":"lab.measure"}]
```
Digest: `c57fc8b429409c4fbd9ba5d30f9c13561a7a6a3a4c2011d85791d2a67a2a8cda`

### Vector `string`

Non-ASCII text is written as itself, whether it was sent raw or escaped; the control character U+0007 is written `\u0007`.

```json
[{"name":"greet","module":"cafe","op":"cafe.greet","input_schema":{"type":"string","description":"Grüße, \u6771\u4eac\u0007!"}}]
```
```text
[{"input_schema":{"description":"Grüße, 東京\u0007!","type":"string"},"module":"cafe","name":"greet","op":"cafe.greet"}]
```
Digest: `dceff93392fb8a334efca1083e5503e335d450e2ad80be38a55291d7941f0eb6`

### Vector `empty`

```json
[]
```
```text
[]
```
Digest: `d53d18c23212ea7b6300594bb89bce60218f6eff2b9d628b8cc42d3e79bbd5ab`

## Operator decision cards

Basal raises `flow_decision` cards for reconciliation, re-enabling an
auto-disabled flow, and lost grants. For each new card it reads the daemon
catalog: a provider advertising `consent/v1` receives `consent.request` on
basal's attested `reserved:basal` route; with no such provider, core receives
the legacy `elicitation.request`. Install cards still go through core.

The chosen path and provider are committed before sending, including requests
whose replies may be lost. A card never changes paths. Updates use its original
endpoint and `dedup_key`; withdrawals use that endpoint and the current card id.
Cingulate refreshes identical content in place, but changed content replaces the
pending card with a new id. Basal records that replacement id and rejects answers
to older bodies or known superseded ids. If a replacement reply is lost, only an
outstanding cingulate grant-loss revision with an exact current context can learn
its new id from an answer; legacy cards retain their existing id checks.

Both answer feeds remain active while they have open cards. Basal applies each
answer durably and idempotently before acknowledging it on its own path. The
cingulate feed uses `consent.answers` continuations and a durable cursor, then
`consent.ack` through the fully processed page, including empty final pages that
cover withdrawals. A failed apply keeps the page unacknowledged; a failed ack
is retried even after the last card closes or basal restarts. Retention
tombstones close only their matching owned card and never apply a choice.

No switch-window timing is required: removing the capability sends new cards
back to legacy while cingulate-raised cards keep using their recorded provider
whenever it is reachable. A listed but unreachable consent provider delays its
cards rather than duplicating them on core. Catalog read failures also delay
selection rather than guessing a path.

The decision body remains core's validated v2 body on either endpoint;
cingulate checks the envelope but stores that body opaquely. Flow decisions
cannot use `consent.report_execution` under the requester contract, so basal
acknowledges settlements without reporting execution. The byte-pinned requester
corpus is copied from cingulate tag `consent-requester-v1`, commit
`c650e9db6e87dbb28fe20dd5fc2815db5deb41e9`, into
`crates/basal-module/tests/vectors/consent-requester-v1/`. Its `SHA256SUMS` digest
is `e4eb488651dbd1e3c3164644a4e4bc0ba3d39f81cb4f3e1f09cc7913564a3248`.
The copied corpus README describes reproduction in its owning repository.

## Lost grants and retirement in health and listing

Both `flow.list` and operator/core `flow.health` include `grant_losses` (an array,
empty when no grant-loss cause remains) and `agent_retirement` (an object or null).
They retain this evidence even when an operator or owner takes over the disable,
so automatic restoration cannot hide a manual stop.

A `module_grant_absent` or `agent_grant_absent` refusal records `grant_lost` and
pauses the flow. Each `(flow_id, provider, grant)` has one durable subject:
`flow_id`, `provider`, `grant`, `grant_label`, `echoable`, `run_id`, `version`,
`revision`, `state`, `failures`, `next_poll_at_ms`, and `lost_at_ms`. Repeated loss
updates the subject and revision. `state` in visible evidence is `polling` or
`stopped`. String grant references are preserved verbatim; structured references
use compact JSON with recursively sorted object keys. References over 4 KiB use
a BLAKE3 hex key with `echoable:false`: the key is neither sent to the provider
nor suitable for a card to echo. The flow still stays disabled.

Basal performs only the read-only `grants.would_ask` diagnostic on the flow's
core-registered scoped route, never a carrier route. Unanswered or negative
checks back off from one second to a one-minute cap on the runtime clock; `yes`
clears the lost-grant cause. The flow runs again only if no other lost grant,
stopped restoration, retirement, or operator/owner disable remains. Removal or
revocation stops polling. Explicit enable clears the pause and permits the
deferred call to retry; a fresh grant refusal can pause it again. Restoring
access is an operator action outside basal, currently `ck-plexus-admin grant …`.
Basal does not grant or offer provider access. It raises one open `flow_decision`
card of kind `grant_lost` per `(flow_id, provider, grant)`, updating that card on
repeat loss. Its typed body is `{flow_id, provider, grant, grant_label,
refused_at_ms}`; the run and version remain in basal's durable record, not in
this body's closed schema. The `grant_lost` card prompt includes the refusal's
full UTC date and time, so refusals on different days are distinguishable.
`check_now` schedules an immediate scoped diagnostic; `keep_disabled` stops
polling for that grant. Expiry or silence applies neither choice and keeps
background polling. Once the grant returns, basal marks the card stale and
durably queues withdrawal on the card's raising path (`consent.withdraw` or
`elicitation.withdraw`), retrying while that provider is unavailable. An
oversized hash fallback raises no card.

An `agent_retired` refusal fails the run and disables the flow without a
retirement or permission card. If an earlier mutation may already have been
sent, its call remains unknown and the run still needs reconciliation: retirement
does not settle whether that mutation happened. Its existing reconcile card
remains available. A "not applied" answer permits the established re-issue path;
a subsequent retirement refusal fails the run without another reconcile card.
`agent_retirement` records `agent_id`,
`agent_reference`, `provider`, `action`, and `at_ms`. Unknown identity is retained
as null and cannot be assumed restored. Explicit enable is refused while that
evidence stands. A newly approved reinstall clears retirement only when its
owner differs from the retired agent and its manifest no longer names that
agent. For an operator-installed flow, removal of the retired target from all
manifest agent fields is sufficient. Changing code while retaining the retired
owner or target does not clear retirement.
