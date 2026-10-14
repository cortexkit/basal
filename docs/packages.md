# Flow packages and instances

A flow package is a flow version that is registered once, approved once, and run as one instance per agent. prefrontal-core decides which agents run which packages: a persona pins packages by exact version, and core reconciles each agent's instances when its persona changes. basal stores packages, validates them, and runs instances. Flows installed with `flow.install`, including global flows that reach several agents, are unchanged.

This document is the contract between basal and prefrontal-core. Its test vectors (fixed inputs with the exact outputs they must produce) are pinned in basal's tests, and core may pin them too.

Shapes use the notation of [`ops.md`](ops.md).

## Package manifests

A package manifest is a flow manifest ([`manifest.md`](manifest.md)) with three additional rules:

- `id` is the package id. It must be at most 46 bytes, so that the instance flow id below fits the 63-byte flow id limit. `version` is the package version.
- Every agent field names `$self` and nothing else: `sinks[].agent`, `status[]`, `claims[].agent` and `facts.targets[]`. `$self` stands for the agent an instance runs as. `$` is outside the agent-name alphabet, so `$self` can never be a real agent's name.
- `flow.install` refuses a manifest that contains `$self`, with `self_requires_package`. A package reaches only its own agent. An ordinary flow that must reach other agents may request a workspace or global audience regardless of its author; global reach requires approval on core's card.

The code hash is the ordinary code hash ([`manifest.md`](manifest.md)), taken over the exact manifest bytes, `$self` included. So one hash, and one approval, covers every instance of a package version.

Code-hash test vector: the script `"// package script bytes\n"` and the manifest

```json
{"format":1,"id":"dark-wake","version":4,"purpose":"Nudge this agent when its session goes quiet.","trigger":{"schedule":{"interval":"15m"}},"sinks":[{"agent":"$self","digest_max":"wake"}],"status":["$self"],"facts":{"targets":["$self"]}}
```

(exactly those bytes, with no trailing newline) have the code hash `443a9c90ca55ba548bfb2fc5b496916ac46e96605be0b2be9469de40bd09743c`.

## Instances

An instance is a flow whose id is derived from its package and its agent's stable `agent_id` (the `agent_…` id core assigns), never the agent's name, which can change:

```
instance flow id = <package> "_" <first 16 lowercase hex characters of h>
h = BLAKE3("basal-instance-id-v1\0" || u64_le(len package) || package || u64_le(len agent_id) || agent_id)
```

Both lengths are byte lengths. Renaming an agent leaves its instances, and their state, where they are.

| Package | `agent_id` | Instance flow id |
|---|---|---|
| `dark-wake` | `agent_47120287c700722b` | `dark-wake_d04794834aa77fe7` |
| `dark-wake` | `agent_d1e7002e2c9b9f43` | `dark-wake_4dc17caa47226d32` |
| `ci-watch` | `agent_47120287c700722b` | `ci-watch_461d8a6c652aa677` |

`flow.install` refuses any flow id that ends in `_` followed by exactly 16 lowercase hex characters (`flow_id_reserved`), so an ordinary flow can never take an instance's id.

The id doesn't depend on the package version. So an instance keeps its `kv`, token windows, rate windows and schedule across upgrades, the same way a flow keeps them across versions. Each agent's instance has its own state, so one agent's instance can't read or exhaust another's.

An instance's owner is its agent's `agent_id`. It acts only as that agent:
- Every `$self` in its manifest resolves to that `agent_id` when calls are authorized, and sink calls carry that id.
- The script receives the agent in its activation input as `self: {"agent_id": "<agent_id>"}`, recorded in the journal like `trigger`.
- Core registers the instance's flow scope under the instance flow id, with that agent as owner, as for any agent-owned flow.

## package.register

Caller: the operator, or prefrontal-core. Agents and local callers are refused with `not_permitted`.

Params:
```json
{"script":"string","manifest":"string"}
```

Reply:
```json
{"package":"string","version":"integer","code_hash":"string","new":"boolean"}
```

The package id and version come from the manifest. The manifest is validated as for `flow.install`, plus the package rules above. The one exception is the known-agent check, which runs when an instance is created. Registering the same bytes again answers `new: false`. A registered package version never changes.

Refusals, beyond `flow.install`'s manifest refusals:
- `package_names_agent`: an agent field names something other than `$self`.
- `package_id_too_long`: the id is over 46 bytes.
- `package_version_conflict`: the id and version are already registered with a different code hash.

Registering raises no card. Core owns approval of package versions: it raises a version's card when a persona first references it.

## package.get

Caller: prefrontal-core only.

Params:
```json
{"package":"string","version":"integer"}
```

Reply:
```json
{"package":"string","version":"integer","script":"string","manifest":"string","code_hash":"string"}
```

This returns the exact registered bytes, so core can render the package card from them and check the code hash itself. Refusal: `package_unknown`.

## Ordering: the generation

Core can lose a route while a call is still running inside basal, then send a newer call for the same package and agent on a new route. The two calls may then finish in either order. So every `flow.instance.ensure` and `flow.instance.remove` carries a `generation`: an integer that core assigns, that strictly increases for each (package, `agent_id`) pair, and that survives core restarts. basal stores the generation of the last call it applied to the pair, in the same transaction as the call's change, and decides each call by it:

- **Higher than stored:** the call applies.
- **Equal:** the call is a resend. Nothing changes, and basal answers the same reply as the first time. An equal generation that arrives with a different operation or version is a caller error, and is refused with `generation_conflict`.
- **Lower:** the call is stale. Nothing changes, and basal refuses it with `generation_stale`, with the stored generation in the refusal detail as `current`. Core treats that as superseded, not as failure.

A `flow.instance.remove` for a pair with no instance still records its generation, so a stale `flow.instance.ensure` arriving after it can't create the instance.

## flow.instance.ensure

Caller: prefrontal-core only, on its own unscoped route.

Params:
```json
{"package":"string","version":"integer","agent_id":"string","generation":"integer"}
```

Reply:
```json
{"flow_id":"string","generation":"integer","new":"boolean","previous_version":"integer|null"}
```

The call makes the agent's instance of the package run the given version:
- **Absent:** it creates the instance. `new` is true and `previous_version` is null.
- **Present:** it points the instance at the given version and returns the version it replaced.
- **Removed:** it clears the removed mark, so the instance runs again with the state it kept.
- **Enable state:** it never changes it. Removal is a separate mark, not a disable, so a disable by the operator, the owning agent or the runtime stays as it is, with its usual rules for lifting it.

A newer generation with the same version changes nothing else, and answers `new: false` with `previous_version` equal to that version.

Approval isn't checked here. As for every flow, each activation and resume asks core's `flow.install_status`, with the instance flow id, the package version and the package's code hash. Runs already admitted finish on the version they were admitted with, so an upgrade needs no drain.

Refusals:
- `generation_stale` and `generation_conflict`: see above.
- `package_unknown`: that version isn't registered.
- `agent_unknown`: core's agent list has no agent with that `agent_id`.

A refused call records no generation.

## flow.instance.remove

Caller: prefrontal-core only, on its own unscoped route.

Params:
```json
{"package":"string","agent_id":"string","generation":"integer"}
```

Reply:
```json
{"flow_id":"string","generation":"integer","removed":"boolean"}
```

The call marks the instance removed. No new run of a removed instance is admitted, and a run already admitted ends at its next activation. The instance's enable state is left as it is. Nothing is deleted: its `kv`, journal and audit stay, and a later `flow.instance.ensure` clears the mark and the same instance runs again with its state. `removed` is false when the instance doesn't exist or is already removed. Refusals: `generation_stale` and `generation_conflict`.

### Revoking an instance version

For a revoked package or template version, core sends `flow.install_revoked` once
per instance flow id, with its version and code hash. Basal verifies each hint
with a single status read. Once recorded, revocation blocks every later
host-call attempt on that instance/version, including built-ins and local `kv`.
The next call is refused with `version_revoked` and ends the run; health reports
that kind without counting it towards auto-disable. Calls authorized before the
revocation was recorded may still be sent and settle; none is authorized after.
Retries pass the fence again, and unknown mutation outcomes still require
reconciliation. The instance's enable switch, state and history remain unchanged.
The existing per-activation check covers instances core did not notify, without
idle revocation polling.

## flow.instantiate

The agent-facing [`flow.instantiate`](templates.md#flowinstantiate) creates instances of packages with bounded parameters. One agent may run several instances with different values, so template instance ids additionally cover the canonical values, using a distinct domain tag. The persona instance formula above applies only to packages without `params`; `flow.instance.ensure` refuses templates with `package_has_params`.
