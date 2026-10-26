# flow-room-post/v1

Basal's side of the `sink.room.post` contract: the manifest grant, the script
call, the request Basal sends and the refusals it acts on. Core's owner
published the contract samples this directory pins as
`flow-room-post-vectors-v1`; `vectors.json`, `README.md` and `SHA256SUMS` are
copies, byte for byte, and a test re-checks both files against `SHA256SUMS`.

The files below are Basal's own and describe what Basal sends core or reads
from it, not what the script supplies:

- `capability.txt` is the feature gate. Core must list `flow-room-post/v1`
  among what it provides, read from the daemon catalog for the call itself.
- `manifest-grant.json` is the exact grant inside the manifest's `sinks`
  array, taken from the published fixture: one room, named by its room id, and
  the agent ids the flow may mention. It grants no digest, status, board or
  wait permission.
- `requests.json` is every exact request Basal puts on the wire, one entry
  per case of `vectors.json`. Each is that case's `params` with the receipt
  key the journal will use (`bk1-…` in a live run). A lost reply resends the
  same bytes under the same key.
- `replies.json` is each case's `reply` reduced to the fields Basal reads:
  a `result` reply keeps `room_id: "rm_fleet"` and `seq: 3`, and an `error`
  reply keeps only its code.
- `refusals.json` binds every code core can refuse with to what Basal does
  with it: pass it through to the script as final, or treat it as proof that
  the call was never stored and retry it later under the same receipt. The
  rate-limit refusal is final, so a refused post is never sent again
  automatically.

All files are UTF-8 with a final LF; `SHA256SUMS` covers the pinned copies
only and excludes itself.
