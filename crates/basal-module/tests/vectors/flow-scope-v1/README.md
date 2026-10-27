# Flow scope v1 vectors

Copied byte-for-byte from the prefrontal repository's
`test-vectors/flow-scope-v1/` directory at commit
`73c66ff1f7517c9c9496fd353b9c4a1d0c89d0cf`, except the two registered-scope
files below.
The JSON files are compact sorted-key JSON followed by one LF. Reply files
contain the operation result, which the transport unwraps from `result`.
`flow_scope_vectors.rs` checks the published SHA-256 of every file, the
request encoder, and the selector's translation to the daemon vocabulary.

`registered-scope.json` and `registered-global-scope.json` are copied from the
prefrontal repository's `test-vectors/flow-scope-v1/` directory at commit
`4b8be7866`, where `prefrontal-routing` joined the carrier's targets and the
records gained the flow identity attributes (`flow_version`, `flow_code_hash`,
`flow_install_id`). Their published SHA-256 digests are:

- `registered-scope.json`:
  `65bb5c060f2f5723f9e8c8e756f20d354f52430cbc34fb94686db02b769d61ba`
- `registered-global-scope.json`:
  `a537d6f96d7bf8a321cf3ad880895ef82049b213c94ecf2bb07cc13017a4ebba`

A global scope names its flow but carries no agent identity or delegation.

The reference scope belongs to core, has exactly basal as its carrier, and
names a flow and its owner agent. The carrier's approved targets are broca,
plexus, prefrontal-core and prefrontal-routing; only broca has opted in among
them in the install-status fixture. Core's carrier operations still use
basal's unscoped route.
