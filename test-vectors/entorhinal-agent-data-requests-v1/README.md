# Entorhinal's agent-data/v1 request samples (copy)

`new-format.json` is a byte-for-byte copy of Entorhinal's published sample
`test-vectors/agent-data-requests-v1/new-format.json` (entorhinal commit
`02478111072f9187816edd9fe66ddf82cdaa4480`). Its SHA-256 matches the one
Entorhinal's own `SHA256SUMS` lists for that file, and `SHA256SUMS` here pins
it again.

The file records the requests Entorhinal's move driver sends a participant
during one completed move (`side: "source"` and `side: "receiver"`) and one
move aborted after export (`side: "source-abort"`). Entorhinal captured them
for a participant that holds `opencode-session` refs. Basal's participant
tests (`crates/basal-module/src/agent_data_tests.rs`) replay these requests
exactly (method, `cutId`, `holder`, field names and order) with only the ref
swapped for basal's own kind, `{"ref_kind": "agent", "ref": <agent id>}`,
using the sample's agent id `agent_0123456789abcdef`. The two registry
records at the top of the file are Entorhinal's own registry calls; basal's
tests skip them.

Don't edit the file here. When Entorhinal publishes a new sample set, copy it
again and update `SHA256SUMS` and the commit named above.

Check: `(cd test-vectors/entorhinal-agent-data-requests-v1 && shasum -a 256 -c SHA256SUMS)`
