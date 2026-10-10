# Agent-facing basal operations

Core's scoped relay exposes these five operations: `flow.install`, `flow.dry_run`, `flow.disable`, `flow.enable` and `flow.list`. Caller identity comes from the daemon's route stamp, never request parameters. `flow.health` is not relayed: core uses it to decide whether a flow's claim to replace a source remains healthy, and the operator uses its runtime-wide figures.

Basal also provides the agent-facing `codemode` tool directly: the daemon delivers named tool requests to basal, rather than core forwarding them. See [codemode](#codemode).

Package management is not agent-relayed: `package.register` is available to the attested operator and core; `package.get`, `flow.instance.ensure`, and `flow.instance.remove` are core-only. The attested operator is a caller on a route the daemon stamps as the reserved `callosum` module, the operator's own module. A plain local caller is a process holding the daemon's direct connection key, on a route with no scope, which the daemon cannot vouch for. A plain local caller refused any of these receives `operator_attestation_required`, consistently with the other management operations; other unauthorized callers receive `not_permitted`. See [package manifests](packages.md).

Shapes below use type names, `|` for alternatives, and a one-element array to describe each array item. `?` on a request key means optional. Reply keys are always present unless a key is marked optional with `?`. `object` and `any` describe open JSON values.

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

## Codemode

Basal is a `tool-provider/v1` provider with one agent tool, `codemode`. Core grants access and owns each ephemeral run scope; it does not carry the tool or provide its input schemas. The daemon verifies the caller's principal and stamps the incoming route with scope owner, ref, epoch and authoritative attributes including `agent_id`; basal takes its agent and invoking scope only from that stamp. Arguments cannot override either. Each child provider route carries the invoking route's `BindIdentity` unchanged (project root, harness, session and any other protocol identity fields), so relative paths and shell working directories resolve as for the agent's direct calls. Bind identity is configuration only: basal never derives agent or scope authority from it.

The named tool request uses the daemon wire protocol's [`ToolCallRequest`](https://docs.rs/subc-protocol/0.30.0/subc_protocol/tool_call/struct.ToolCallRequest.html):
```json
{"name":"codemode","arguments":{"code":"string","description?":"string","limits?":{"wall_ms?":"integer","tool_calls?":"integer","output_bytes?":"integer"}},"call_key?":"string","schema_pin?":"string"}
```

The JavaScript function body sees only `tools.<name>(input)`, `console.log(...)`, and frozen intrinsics. Each tool has the same input and provider permission rules as the agent's direct calls, including writes and shell. Only codemode itself is excluded. Model tools are not implemented yet.

- A keyed run's ID is domain-separated BLAKE3 over length-framed `(agent_id, call_key)`. A repeated key attaches to its running run or returns its finished result, never starting another worker or dispatching another call.
- Without a key, a fresh OS-random run ID executes once, with no deduplication, withdrawal or late result. Its result warns that a lost reply cannot be recovered.
- `description` is optional, at most 1,024 UTF-8 bytes, and has no line breaks.
- `limits` may only lower wall time (1–1,800,000 ms), tool calls (1–200), and captured output (1–65,536 bytes).
- The parent enforces 30 minutes of wall time, excluding intervals when the worker is blocked and every in-flight call is held on a person. Person wait has one cumulative 10-minute cap per run. The confined worker enforces 10 seconds of JavaScript CPU, 64 MiB heap and 1 MiB stack.
- Windows refuses `unsupported_platform` until worker confinement is available there.

Basal records the run before calling core's `codemode.run_scope.open` over its own route stamped `reserved:basal` (the daemon-verified basal module principal), never by borrowing a program's agent authority:
```json
{"agent_id":"string","invoking_scope":{"ref":"string","epoch":"integer"},"run_id":"string","expires_at_ms":"integer"}
```
Core replies:
```json
{"scope":{"ref":"string","epoch":"integer"},"expires_at_ms":"integer","catalog":[{"tool":"string","module":"string","op":"string"}]}
```
Expiry is fixed at admission time + 41 minutes. Basal checks the returned scope's daemon stamp for matching agent and run, and reads input schemas only from the daemon's declarations for the returned module/op. Core refusal codes, messages and optional details are preserved unchanged. No worker starts after a refused open or mismatched stamp.

Every terminal path calls core's `codemode.run_scope.close` with `{run_id, epoch}` before exposing the result. Close replies `{closed, already_closed}`. Cancellation closes the scope before killing the worker. Startup interrupts unfinished runs, closes leftover scopes and never resends a call. If close fails, the result carries a warning and the scope's fixed expiry remains its outer bound.

### Results and long calls

The tool returns a rendered `text` containing a status line (status, duration, call count and person-wait time), JSON return value (at most 16 KiB), captured output, a per-call outcome/person-wait/duration table, and warnings. The reply also carries those fields as structured JSON. Budget termination preserves partial output and known call outcomes; sent calls without a recorded answer are `outcome_unknown`, never retried. Queued calls proven unsent are `cancelled`.

Keyed calls that outlive the foreground wait reply with `status: "running"` and `run_id`. Basal declares the `late_results` session capability. The custodian pulls `late_results {since: null | cursor, limit?}` and acknowledges with `late_results.ack {through: cursor}`; replies use the tool-provider contract's entries/cursor/more shapes. Only the custodian principal verified by the daemon on its route receives entries: the scope owner for a direct call, or the carrier for an onward call. A changed incarnation or future cursor refuses `cursor_incarnation_changed`. Unacked results survive scope closure and remain for 24 hours, then become identity-preserving `expired` entries; retention is bounded to 1,000 results per session and 1,000 expired entries per owner.

`tool.withdraw {call_key, carrier?, scope?}` follows the tool-provider contract's carrier/owner/scope checks. It cancels the run. A call proven not to have started replies `withdrawn`; an already started call replies `already_started` with an outcome, or `unknown`; a completed call returns its retained result. Basal stores the first withdrawal answer and returns identical bytes to every permitted caller and repeat. Role ops (`role.describe`, `tool.catalog`, `tool.withdraw`, `late_results`, `late_results.ack`) are named requests on the tool route, never model tools.

## Catalog digest vectors

`catalog_digest` is the lowercase hex BLAKE3-256 of the catalog array's RFC 8785 (JCS) canonical JSON, with its entries sorted by `name`. Canonical JSON sorts object members by name, has no whitespace, writes numbers the way JavaScript does (`1.0` is `1`, `1e21` is `1e+21`), writes non-ASCII text as itself and escapes control characters. basal computes this digest for its run records; these vectors pin it. Each shows a catalog as sent, its canonical JSON, and its digest. `crates/basal-core/tests/codemode_digest_vectors.rs` reproduces every vector with its own canonical JSON and BLAKE3 code and checks basal's digest against them.

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
