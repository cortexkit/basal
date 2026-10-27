# Flow scope gate v1 vectors

`flow-scope-refusal.json` is copied byte-for-byte from the prefrontal
repository's `test-vectors/flow-scope-gate-v1/` directory at commit
`9a301ba67`. Its published SHA-256 is
`c7daea699187a67e88dccd301d4ba9d529f63ecff97633c1878fdde666368c28`.

It is core's final refusal of a flow-scoped call to an operation flows may not
use: the error body carries the code `flow_scope_not_supported`, the refused
`op`, and the `allowed_ops` a flow may call. Unlike the flow-scope-v1 files it
is pretty-printed JSON, not compact.

`flow_scope_vectors.rs` pins the digest and drives the refusal through basal's
flow-call path as core's reply to a flow's `prefrontal-core` call.
