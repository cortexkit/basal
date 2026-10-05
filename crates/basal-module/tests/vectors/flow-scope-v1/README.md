# Flow scope v1 vectors

Copied byte-for-byte from the prefrontal repository's
`test-vectors/flow-scope-v1/` directory at commit
`73c66ff1f7517c9c9496fd353b9c4a1d0c89d0cf`.
The JSON files are compact sorted-key JSON followed by one LF. Reply files
contain the operation result, which the transport unwraps from `result`.
`flow_scope_vectors.rs` checks the published SHA-256 of every file, the
request encoder, and the selector's translation to the daemon vocabulary.

`registered-global-scope.json` is copied from the prefrontal repository's
`test-vectors/flow-scope-v1/` directory at commit
`873870be88fb73e3e55e7b3600571ba6932159d5`; its published SHA-256 is
`9ea04c51cf987028ba00040c6abfada7e4780d47d48bcb0008d6f789994a518c`.
A global scope names its flow but carries no agent identity or delegation.

The reference scope belongs to core, has exactly basal as its carrier, and
names a flow and its owner agent. Only broca has opted in among the fixture's
approved targets. Core's carrier operations still use basal's unscoped route.
