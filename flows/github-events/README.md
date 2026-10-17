# GitHub events: live owner delivery

Version 3 replaces prefrontal-core's GitHub wake consumer. It resolves Plexus
v2 issue, PR and release notices, applies the routing, suppression and own-echo
rules below (unchanged from the v2 comparison shadow, which matched core for a
full day with no missed wakes and no owner mismatches), and delivers each
accepted event to its **real owner** from the `OWNERS` map:

- A newly opened issue or PR is sent with `wake`; every other accepted event
  (comments, edits, closes, reopens, releases, unknown actions) is an update
  sent with `piggyback`. Core's delivery planner may still downgrade either
  action under the manifest's `digest_max` and the recipient's wake policy.
- Every delivery uses `replace_key` `github:<repo>:<kind>:<number>`, so later
  updates replace the same undelivered item.
- The body is the same rendering as before: quoted, sanitized title, local
  observation times and the overflow count. Digest `data` carries the item's
  identity (`repo`, `kind`, `number`, `event_key`, `state`), its observation
  counts and times, and the modeled debounce timestamps. It no longer carries
  the shadow's comparison fields (`would_owner`, `would_action`,
  `suppressed_reason`, `rule`).

**Seen reaction.** On the first delivery of a newly opened issue or PR, and
only then, the flow reacts 👀 as the owner through Plexus's narrow
`github_react` tool:
`ops.call('plexus', 'github_react', request, {for: owner})`, granted in the
manifest as `{module: "plexus", op: "github_react", for_target: "delegate"}`
(the only targeted grant; the body resolver `events_get` reads on the flow's
own scope). The request is exactly
`{repository, kind: "issue" | "pr", target: {number}, reaction: "eyes",
request_nonce: "github-events:<event_key>"}`. Plexus keys the nonce per
(agent, nonce), so a retried request replays rather than reacting twice.
Scripts cannot see their run id; admission starts at most one run of this flow
per event key, so the event key already identifies the run, and the flow-id
prefix keeps the nonce distinct from other flows' nonces. Updates and releases
never react, and a second `opened` notice for an item already reacted to does
not react again.

The digest is delivered and the item's history saved **before** the reaction.
A refused or failed reaction never blocks or undoes that delivery: its code
(for example `vendor_rejected`, `agent_grant_absent`, `policy_blocked`,
`outcome_unknown`) is recorded in the run result and the run succeeds.

`github_react` is a mutation on the module whose events trigger this flow, so
install accepts it only if Plexus's catalog declares it `cause_echo`, or the
operator overrides the loop install rule (`docs/manifest.md`, op reference).

Each run returns `events`, a compact comparison record (`repo`, `kind`,
`number`, `event_key`, `would_owner`, `would_action`, `suppressed_reason`,
`rule`, plus observation fields when delivered), `delivered` and `unmapped`
counts, and `reaction`: `null` when no reaction was attempted, otherwise
`{owner, request_nonce, code}` with `code: null` on success. Suppressed events
appear in the run result only. Malformed/nonmatching transport bodies are
ignored before routing.

## Core rules and source references

References below are to the sibling **prefrontal** repository, inspected on
2026-10-16. Full source paths (not copies of private store contents):

- **consumers.rs**: `crates/prefrontal-core-module/src/wake_external_events/consumers.rs`
- **commands.rs**: `crates/prefrontal-core-module/src/wake_external_events/commands.rs`
- **runtime.rs**: `crates/prefrontal-core-module/src/wake_external_events/runtime.rs`
- **external_events.rs**: `crates/prefrontal-core-store/src/external_events.rs`

| Implemented rule | Core source |
| --- | --- |
| A nonempty repository/kind and item number earn an item; no kind/action whitelist. | commands.rs:866-891,934-939; runtime.rs:111-155 |
| Ignore reliably empty actor lists on closed PRs, merged or not; unknown enrichment is not empty. | consumers.rs:1227-1253,1437-1451,1661-1666 |
| Drop events marked `suppressed_own_echo`. | consumers.rs:124,1214-1216; commands.rs:1030-1032,2001 |
| `reopened` forces state open, even over a stale closed body. | consumers.rs:1121-1128 |
| Own subject authorship is separate from comment authorship. Own-bot-authored subjects need a human comment/review to pass; a human login is nonempty and does not end in `[bot]`. This exception also bypasses the actor-set gate. | consumers.rs:1282-1327,1673-1701 |
| Operator, project and own-agent logins form one set, compared case-insensitively and without a trailing `[bot]`. An event is an own echo, and never wakes, when every actor is in the set, or at least one is and every other is `github-actions[bot]` (automation an agent's own action set off). `github-actions[bot]` alone, any human, or any unlisted bot still wakes; a missing or empty actor list wakes. A workflow run is decided by who started it (`workflow_run.triggering_actor`, else `actor`): started by a suppressed login with only suppressed or `github-actions[bot]` actors (an empty set counts) is an echo; started by anyone else wakes. | Ufuk's own-echo ruling, 2026-10-17, as core's consumer implements it |
| Opened issues/PRs are new (`wake`); everything else is an update (`piggyback`). | commands.rs:424-491,751-815 |
| Model rolling update debounce at 30 minutes, capped at 60 minutes from the first update in that window. New items bypass debounce. | commands.rs:424-491,751-815 |
| Repeated observations become one item with the first six times, an overflow count and the newest summary. | commands.rs:1501-1618,1712-1762 |
| Use fixed action words: opened, new comment, edit, closed, reopened, otherwise update; never interpolate arbitrary vendor action text. | commands.rs:965-1010 |
| Quote vendor titles; replace `<` with `‹` (U+2039), double quote with apostrophe, and each CR/LF with a space. Apostrophes themselves are unchanged. | commands.rs:1420-1433,1456-1498 |
| Observation clocks use local HH:MM. | commands.rs:955-963 |
| Seen event IDs do not earn another pending item. | external_events.rs:287-324,329-373,709-742 |

Routing deliberately uses the exact `OWNERS` map from
`flows/ci-failure-router/flow.js:3-39`, rather than core's stamped subscription
project IDs (consumers.rs:1597-1639), because those IDs are not in the v2 body.
Flow scripts cannot import another script, so a regression test compares copies.

The bot map comes from Plexus `bindings.read`, which lists the GitHub App bound
to each repository's agent; `repo_binding_generation 32` identifies that snapshot.
MOTOR and CLOUD are owners but have no bound bot: their repos have **no own-bot
rule**, while operator/project actor suppression still applies.

Operator logins (`ualtinok`, `alfonso-aft`, `MC-Alfonso`) mirror core's
`module_config` key `events_suppress_actors` as of 2026-10-16, supplied by ALF.
The only project-specific row is AFT's `events_suppress_actors:<project_id>`:
`cubic-dev-ai` and `greptile-apps`, represented by lowercase repo
`cortexkit/aft`. No other project rows were reported. These are login identifiers
only; no private configuration records or real comment text are included.

## Explicit differences from core and assumptions

- **Immediate writes, modeled timing.** Event-triggered scripts have no
  timers/continuations and cannot combine schedule and event triggers
  (`docs/manifest.md:163-180`, `crates/basal-worker/src/prelude.js:219-224,361-371`).
  Every accepted update is therefore sent immediately. KV and digest `data`
  carry `first_seen_ms`, `latest_ms`, `due_ms = min(latest + 30 min, first +
  60 min)` for update windows. An opened item has `due_ms = latest_ms`; its
  first update starts an update window. An update arriving after the modeled
  due time starts another window. These timestamps are observations, not
  vendor `updated_at` values.
- **Action policy.** The one `ACTION` constant maps opened items to `wake` and
  updates to `piggyback`. Flow sinks go through core's delivery planner:
  manifest `digest_max` and the recipient's wake policy may downgrade the action.
- **Rescan age.** Preserve the router's six-hour guard on
  `completed_at ?? updated_at`. Every stale drop is visible with
  `suppressed_reason: "stale"`. Core does not age-drop otherwise deliverable
  events; its pruning targets undeliverable rows
  (runtime.rs:904-915; external_events.rs:778-799). This is intentional.
- **Clock zone.** The operator's core host is Europe/Madrid. `madridHHMM`
  computes the EU last-Sunday March/October 01:00 UTC transitions using UTC
  getters because local Date getters are removed from the deterministic
  sandbox. Tests cover both sides of both 2026 transitions and summer/winter.
- **Human activity.** Plexus v2 has no `type` field or separate review event.
  Its identified `comment_author` is the available human-comment/review evidence;
  `actor_logins` alone does not prove a comment on an own-bot-authored subject.
- **Releases.** No release body schema was found in this repository's docs.
  Assume `tag_name` (or `tag`) is the release identity, put it in `number`, and
  use kind `release`. Releases are updates, not new issues/PRs. Missing tags
  do not earn an item. This is the explicit exception to core's numeric identity.
- **Bounded history.** `replace_key` is `github:<repo>:<kind>:<number>` and
  replaces an undelivered item within this flow (docs/script.md:145-155).
  The flow cannot observe core delivery acknowledgements, so observation history
  spans runs rather than resetting upon delivery. It remembers the last
  64 event keys per item and caps fleet KV at 48,000 serialized bytes, evicting
  oldest histories first. Replays beyond retained history can be observed anew.
  Identities exceeding the sink's 200-byte key limit or containing control
  characters are reported as `invalid_identity`, never silently truncated into
  a colliding key. Digest bounding reuses the incremental helper from
  `templates/github-ci-watch/flow.js:3-69,83-101`: title under 200 characters,
  body at most 4096 bytes, data at most 8192 bytes, item at most 16 KiB.
