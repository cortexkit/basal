# Flow packages and instances

A flow package is a flow version that is registered once, approved once, and run as one instance per agent. prefrontal-core decides which agents run which packages: a persona pins packages by exact version, and core reconciles each agent's instances when its persona changes. basal stores packages, validates them, and runs instances. Flows installed with `flow.install`, including global flows that reach several agents, are unchanged.

This document is the contract between basal and prefrontal-core. Its test vectors (fixed inputs with the exact outputs they must produce) are pinned in basal's tests, and core may pin them too.

Shapes use the notation of [`ops.md`](ops.md).

## Package manifests

A package manifest is a flow manifest ([`manifest.md`](manifest.md)) with three additional rules:

- `id` is the package id. It must be at most 46 bytes, so that the instance flow id below fits the 63-byte flow id limit. `version` is the package version.
- Every agent field names `$self` and nothing else: `sinks[].agent`, `status[]`, `claims[].agent` and `facts.targets[]`. `$self` stands for the agent an instance runs as. `$` is outside the agent-name alphabet, so `$self` can never be a real agent's name.
- `flow.install` refuses a manifest that contains `$self`. A package reaches only its own agent. A flow that must reach other agents is a global flow, installed by the operator.

The code hash is the ordinary code hash ([`manifest.md`](manifest.md)), taken over the exact manifest bytes, `$self` included. So one hash, and one approval, covers every instance of a package version.

Code-hash test vector: the script `"// package script bytes\n"` and the manifest

```json
{"format":1,"id":"dark-wake","version":4,"purpose":"Nudge this agent when its session goes quiet.","trigger":{"schedule":{"interval":"15m"}},"sinks":[{"agent":"$self","digest_max":"wake"}],"status":["$self"],"facts":{"targets":["$self"]}}
```

(exactly those bytes, with no trailing newline) have the code hash `443a9c90ca55ba548bfb2fc5b496916ac46e96605be0b2be9469de40bd09743c`.

## Instances

An instance is a flow whose id is derived from its package and its agent:

```
instance flow id = <package> "_" <first 16 lowercase hex characters of h>
h = BLAKE3("basal-instance-id-v1\0" || u64_le(len package) || package || u64_le(len agent) || agent)
```

Both lengths are byte lengths, and the agent is the exact agent name, so case matters.

| Package | Agent | Instance flow id |
|---|---|---|
| `dark-wake` | `ALF` | `dark-wake_d87dc8558dc3b3cf` |
| `dark-wake` | `BASAL` | `dark-wake_07185384bda9b4cb` |
| `ci-watch` | `basal-test_01` | `ci-watch_06fd7ecbcfc46e1f` |

The id doesn't depend on the package version. So an instance keeps its `kv`, token windows, rate windows and schedule across upgrades, the same way a flow keeps them across versions. Each agent's instance has its own state, so one agent's instance can't read or exhaust another's.

An instance's owner is its agent. It acts only as that agent:
- Every `$self` in its manifest resolves to that agent when calls are authorized.
- The script receives the agent in its activation input as `self: {"agent": "<agent>"}`, recorded in the journal like `trigger`.
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

## flow.instance.ensure

Caller: prefrontal-core only, on its own unscoped route.

Params:
```json
{"package":"string","version":"integer","agent":"string"}
```

Reply:
```json
{"flow_id":"string","new":"boolean","previous_version":"integer|null"}
```

The call makes the agent's instance of the package run the given version:
- **Absent:** it creates the instance. `new` is true and `previous_version` is null.
- **Present:** it points the instance at the given version and returns the version it replaced.
- **Removed:** it clears the removed mark, so the instance runs again with the state it kept.
- **Enable state:** it never changes it. Removal is a separate mark, not a disable, so a disable by the operator, the owning agent or the runtime stays as it is, with its usual rules for lifting it.

The call is idempotent by package and agent. Repeating it with the same version changes nothing, and answers `new: false` with `previous_version` equal to that version.

Approval isn't checked here. As for every flow, each activation and resume asks core's `flow.install_status`, with the instance flow id, the package version and the package's code hash. Runs already admitted finish on the version they were admitted with, so an upgrade needs no drain.

Refusals:
- `package_unknown`: that version isn't registered.
- `agent_unknown`: the catalog doesn't know the agent.

## flow.instance.remove

Caller: prefrontal-core only, on its own unscoped route.

Params:
```json
{"package":"string","agent":"string"}
```

Reply:
```json
{"flow_id":"string","removed":"boolean"}
```

The call marks the instance removed. No new run of a removed instance is admitted, and a run already admitted ends at its next activation, as for a flow core has revoked. The instance's enable state is left as it is. Nothing is deleted: its `kv`, journal and audit stay, and a later `flow.instance.ensure` clears the mark and the same instance runs again with its state. `removed` is false when the instance doesn't exist or is already removed.

## flow.instantiate

Agents will also create instances, of packages that declare bounded parameters, with `flow.instantiate`, which isn't specified yet. One agent may run several instances of such a package with different parameters, so their identity also covers the bound parameters, and the instance flow id above applies only to persona instances.
