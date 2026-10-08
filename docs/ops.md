# Agent-facing basal operations

Core's scoped relay exposes these five operations. Caller identity comes from the daemon's route stamp, never request parameters. `flow.health` is not relayed: core uses it to decide whether a flow's claim to replace a source remains healthy, and the operator uses its runtime-wide figures.

Package management is not agent-relayed: `package.register` is available to the attested operator and core; `package.get`, `flow.instance.ensure`, and `flow.instance.remove` are core-only. A plain local caller refused any of these receives `operator_attestation_required`, consistently with the other management operations; other unauthorized callers receive `not_permitted`. See [package manifests](packages.md).

Shapes below use type names, `|` for alternatives, and a one-element array to describe each array item. `?` on a request key means optional. Reply keys are always present. `object` and `any` describe open JSON values.

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
The operator may enable any flow. An owning agent may undo only its own disable, not an operator or automatic disable. Local callers are refused.

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
{"as_of":"integer","flows":[{"flow_id":"string","state":"enabled|disabled|unapproved","approved_version":"integer|null","pending_version":"integer|null","disabled":{"by":"operator|owner|auto|core","reason":"string","at":"integer"},"last_run":{"run_id":"string","state":"string","ended_at":"integer"},"needs_reconcile":"boolean"}]}
```
`disabled` and `last_run` may each be null, but their keys must be present. All times are Unix epoch milliseconds. `pending_version` is the newest version with an open card, or null; `approved_version` is null until approval. `state` uses the same approval-first computation as `flow.health`. `last_run` is the most recent finished run, not an in-flight run. `needs_reconcile` is true if any run awaits reconciliation. The disable's actor kind, reason and time are the recorded disable, or null after enabling. `by` is `operator`, `owner` (the owning agent), `auto` (the runtime's loop protection) or `core` (core no longer stands behind the approved version: the operator revoked it in core, core holds no install of it, or core approved other code under it; such a flow also has no approved version and lists as `unapproved`). A newer version approved through its consent card lifts a `core` disable.
