# Repository flows

These are operator-installed flows, not templates or automatically installed services.

## Board hygiene

`board-hygiene/manifest.json` and `board-hygiene/flow.js` define one global flow
that scans live boards every five minutes through core's `board.read` operation.
It sends per-item digest nudges for ended waits, overdue ETAs, active items left
unchanged while their owner is active, and completed lanes awaiting retirement.
It uses activity facts only for candidate stale items. No network or LLM is used.

Since version 12 it also reads every page of core's `work.unsettled` list each
run and nudges owners about deliveries that are merged into their integration
ref but not settled, which block the campaign slices waiting on them. Only
`merged` items with an owner count; items whose `agent_id` is null are counted
in the run result as `skipped_unsettled_unowned`. Core does not say when a
delivery merged, so the flow keeps the time it first saw each one merged in a
single bounded KV value (at most 384 items) and nudges once the delivery has
stayed merged and unsettled for 20 minutes past it. An item that leaves the
list, or that core reports as `unmerged` or `none`, is forgotten. Each owner
gets one `unsettled_merged` list naming each delivery's title, id, merged ref
and waiting slices, with the action
`work(action="settle", id="<item_id>", score=<0-100>, merged_ref="<ref>")`;
the score is left to the owner. A refused or failed read is recorded in the run
result (`unsettled_read_error` and the refusal log), leaves this rule's lists and
memory unchanged, and never stops the board rules.

The flow keeps bounded, sharded indexes of its per-rule KV entries so it can
remove memory for vanished agents, lanes, and items. If memory is full, it skips
unrecordable nudges rather than sending reminders it cannot rate-limit. Digest
refusals are retained in a bounded run-result log without interrupting the rest
of the scan. The manifest's four-minute deadline fits within the five-minute
schedule. Run and dispatch rate limits are configured by the operator on basal's
runtime (`basal_core::Config::rate`), not as manifest fields.

Installation and audience selection belong to the operator. The offline tests
in `crates/basal-module/tests/board_hygiene.rs` exercise the shipped script and
validate its manifest with basal's parser.

Reminders use a recipient-scoped digest replacement key when core supports it.
If core does not advertise replacement support, basal refuses the keyed write
before dispatch with `digest_replace_key_unsupported`. The flow retries the
same reminder without the key and records the fallback once in the run-result
log. Keys over 200 UTF-8 bytes are omitted. KV repeat throttling remains active
in both modes.
Item nudges carry current state and wait metadata; one-time copies also carry
`own_*` identifiers for the waiting owner's item so the renderer targets the
item to re-plan rather than the peer's item whose ETA passed.

## CI failure router

`ci-failure-router/` is one operator-installed global flow for Plexus v2
`github_ci_job_failed` and `github_ci_run_completed` notices in `cortexkit/*`.
It wakes the repository's owner for the first newly reported failed job, on any
branch; later jobs silently update the same item, and repeated job IDs within an
attempt are deduplicated. At run completion it wakes once more only when additional
job IDs arrived after the first notice, naming those new jobs. A non-cancelled red
or unknown run with no reported jobs wakes as the only available report. A
cancellation with no recorded failed job is ignored; cancellation after a failed
job silently updates its item. Success only silently updates an already-reported
attempt. A null conclusion is reported as unknown, never as a successful (green)
run. Exact workflow names can be ignored per repository; a job event without
`workflow_name` remains reportable. Version 5 also ignores configured job names
(AFT's `design-gate`), remembering their failures so a completion with only ignored
failures sends nothing rather than reporting missing jobs. Before an attempt's
first wake it checks Plexus `github_read` action `github.actions.run.superseded`
once: superseded attempts send nothing, including later events, while refusals or
unreadable replies fail open and log the refusal code in the run result. Green,
quiet, ignored and stale events never make this read.
Merge-queue jobs on `gh-readonly-queue/` branches silently update the digest when
the same repo, workflow and job name is already failing on the default branch
(`default_branch` in the body, otherwise `main` or `master`). A green completion
on that branch clears the workflow's failing-job flags. Repositories in `QUIET_REPOS` still update
their digest items silently, and their entries should be removed when CI returns.
Titles identify the repository and branch; the body carries the run attempt and
failed jobs. Version 15 also names failing tests when a job-log tail is readable
and recognizable. For each newly reported job it calls Plexus `github_read` with
`action: 'github.actions.job_log'`, `repository`, and
`args: { job_id, max_bytes: 65536 }`. Rust libtest and nextest (including timeouts),
bun, Go, pytest, and vitest/jest failure lines are recognized, including GitHub's
timestamp prefixes. Names are deduplicated in log order, stripped of control
characters, and shortened to 160 bytes; at most five appear per job in a
`Failing tests: a, b (+N more)` line. The extra count covers only distinct matches
in the tail, not failures outside that window. Multi-job summaries label each
job, and whole summaries are omitted if the digest's body limit would be exceeded.
Green updates label retained names as `Previously failing tests`.

Parsed names are retained with their job in run state, so ordinary silent updates
after the first wake and the second completion alert keep the relevant names
without rereading logs. Duplicate jobs, ignored jobs/workflows, superseded runs,
stale notices, never-started jobs, quiet repos, contributor branches and repeated
merge-queue failures make no job-log call. An unreadable reply, thrown error, or
refusal (including `job_not_finished` and `github_actions_quota_exhausted`) never
prevents the digest: it adds no names line or unavailable-names filler, logs the
refusal code, and does not retry or wait for a quota reset. Log enrichment does
not change the two-wake limit, replacement keys, or other routing rules.

The clearly marked `OWNERS` block maps lowercase repositories to stable agent IDs
(display names are comments, not runtime recipients). Update the map and publish
a new flow version when ownership changes. Repositories absent from the map have
no fallback recipient and write no digest. A single KV counter counts unmapped
notices and saturates at JavaScript's maximum safe integer instead of growing a
per-repository diagnostic index.
Core checks recipients at delivery time, and refusal codes are logged in the run
result without failing the run. Replacement support is required to maintain one
item per attempt: keyed writes refused by core are logged, not retried unkeyed.

The script adapts `templates/github-ci-watch/flow.js`, rechecks verified body repo
and branch fields instead of trusting notice headers, deduplicates job IDs within
each attempt, and uses `ci:<repo>:<run_id>:<run_attempt>` replacement keys.
One shared KV aggregate retains repo-namespaced attempts for fourteen days with a
48,000-byte cap across the whole fleet, not a separate quota per repository;
default-branch failure flags share that cap and fourteen-day retention.
Offline worker tests and a dry run with a synthetic CI trigger live in
`crates/basal-module/tests/ci_failure_router.rs`. The dry run captures sink calls
without delivering notifications. No per-agent watcher is needed.
