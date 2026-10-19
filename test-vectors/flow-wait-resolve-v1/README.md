# flow-wait-resolve/v1

Pinned core/Basal sink contract. All files are UTF-8 with final LF. JSON files are
request **params** or response **result**, not protocol envelopes. The operation
is `sink.wait_resolve`; the carrier must be daemon-attested `reserved:basal`, just
like `sink.board` / `sink.digest`. Basal fills the stamp and receipt key; script
arguments cannot choose identity or targets. Do not require `flow-scopes/v1`:
core does not advertise it. `capability.txt` is the feature gate.

- `request-met.json`: resolves the two open waits on the install owner's board.
- `request-failed.json`: the other terminal outcome, no note.
- `reply-met.json`, `reply-replay.json`, `reply-noop.json`: complete result objects.
- `manifest-grant.json`: exact targetless sink entry within the manifest.
- `card-text.txt`: consent fact value, label `Waits`.
- `terminal-lane.json`: served lane consumed by the TypeScript board codec test;
  `ref_state: terminal` feeds the existing ended-wait hygiene rule. The item stays
  blocked until its owner updates it.
- `refusals.json`: stable codes plus conditions/patches relative to the met request.
  `params_patch` is a shallow merge; `note_generator` describes long UTF-8 input.
  Board ref and manifest refusals are not sink request patches.

The install for examples is flow `wait-flow`, version 1, owned by `ag_wait`,
with the zero hash in the request and the manifest's wait grant approved. Other
flows/agents/versions are absent unless a case says otherwise. After Basal
lifecycle end, expiry, revocation, or absence from the live snapshot, any still
open wait uses `resolution: ended_without_resolving` and renders `ended without
resolving`. Already met/failed markers are never overwritten.

From this directory verify `sha256sum -c SHA256SUMS` (or `shasum -a 256 -c
SHA256SUMS` on macOS). The internal design artifact is
`.cortexkit/internal-docs/designs/flow-wait-resolve-v1.md`.
