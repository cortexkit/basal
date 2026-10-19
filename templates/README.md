# Watches and reminders

Each directory is a flow template: register its exact `manifest.json`
and `flow.js` bytes with `package.register`, approve that version once, then
create watches with `flow.instantiate`. Each instance writes only to the agent
that created it. The four one-shot watches/reminders use `wake` and best-effort
resolve waits naming their flow as `met` when the condition is satisfied, or
`failed` when a terminal condition makes success impossible. Resolution is
issued before the digest and end, but never delays either. `github-ci-watch` is
a standing watch, described below. There are no model calls or script-side
network fetches. Template versions have their own approval card; instances do
not go through ordinary flow-install approval.

The four one-shot templates require `note`: a string of at most **4096 UTF-8 bytes**, without
control characters. The digest body is exactly that note, including for a
stopped watch. The short title explains what happened or why the watch stopped.
`data.params` contains the other parameters; `data.observed` records the result.
The note is not duplicated in metadata. Metadata strings are shortened if
needed to keep canonical `data` within 8192 bytes and the whole item within
16384 bytes. `data.truncated` names each shortened field and its original UTF-8
byte length. Oversized links are omitted rather than shortened and marked too.

Every declared parameter is required, even if a particular condition ignores
it. Each template allows at most 100 live instances per agent and an
`expires_in` of at most `365d`; watches expire even when they never fire.
For the one-shot watches, transient check errors leave the watch live for its
next trigger. Permanent configuration or access failures produce a stopped
item and end the watch.
Capture-mode `flow.dry_run` accepts sample parameters and makes no live calls;
captured reads cannot prove that a condition is met.

## `watch-file`

Checks one canonical `path` under `~/Work`, using only `fs.stat` and `fs.read`.
`~/Work` must exist when registering the template. Symlink escapes are refused.

| Parameter | Meaning |
| --- | --- |
| `path` | File or future file path under `~/Work`. |
| `condition` | `exists`, `contains`, or `lacks`. |
| `text` | Case-sensitive literal substring, at most 1024 UTF-8 bytes. Ignored for `exists` (pass `""`). Empty text for `contains` or `lacks` stops as misconfigured. |
| `every` | Check interval, from `5m` through `1d`. |

A missing file never satisfies `lacks`: the file must exist and be readable.
`exists` requires only a successful stat, so it also detects directories.
Content checks stop for a non-file target. Oversized files (the read cap is
106 KiB), non-UTF-8 contents, and temporary read errors retry: the contents may
change. A denied path or malformed built-in argument stops the watch.

Example values:
`{"note":"Review the generated report","path":"~/Work/project/report.txt","condition":"contains","text":"READY","every":"5m"}`.

## `watch-git`

Checks one canonical Git `repo` under `~/Work`, using only Git read built-ins.

| Parameter | Meaning |
| --- | --- |
| `repo` | An existing Git repository under `~/Work`. |
| `condition` | `ref_exists` or `commit_matching`. |
| `ref` | Branch, tag, or commit ref, such as `main`, `refs/tags/v1`, or `HEAD~1`. Required; `""` selects the current `HEAD`. Only ASCII letters, digits and `._/-~^@{}+` are accepted, and a nonempty ref cannot start with `-`. See [Git built-ins](../docs/builtins.md#git). |
| `pattern` | Case-sensitive literal substring, at most 256 UTF-8 bytes, matched against **commit subjects**, not bodies. Ignored for `ref_exists` (pass `""`); an empty pattern for `commit_matching` matches any new commit. |
| `every` | Check interval, from `5m` through `1d`. |

For `commit_matching`, the first successful run stores the starting commit in
the instance's `kv` and does not notify for pre-existing matching commits.
Later checks scan commits reachable from the current head but not the stored
baseline. A complete scan with no match advances the baseline to that head.
This uses commit ancestry, not author dates. Rewritten history or a lost
baseline stops rather than counting older history again. If exactly 500 new
commits are returned without a match, the scan may be incomplete: the watch
stops with a history-limit item rather than silently dropping a possible match.
A match among those 500 still fires normally. Missing refs retry; missing or
no-longer-authorized repositories stop. Other Git failures retry.

The digest identifies the commit SHA and repository. There is no commit URL:
the read built-ins do not expose remote hosting information, so the template
does not guess a GitHub address for a local repository.

Example values:
`{"note":"Start the release review","repo":"~/Work/project","condition":"commit_matching","ref":"main","pattern":"Release","every":"5m"}`.

## `watch-github`

Template version **4** uses Plexus **v1** `github_pr_changed`, `github_issue_changed`, and
`github_release_published` events, not polling. Basal fetches and verifies the
body before running the script. V1 bodies are raw GitHub items; repository
identity comes from the notice's `repo` header, compared case-insensitively.
Missing fields do not invent a state or identity. A later template version can
move to Plexus's planned normalized v2 envelope.

All three subscriptions bind a `repo` admission filter to the instance's `repo`
parameter, so unrelated notices create no run or body lookup. The script retains
its own repository check. Headers are publisher hints, not authorization or
verified body evidence; a publisher can label incorrectly. This v1 watch cannot
prove repository identity from a raw item alone. Access to bodies remains scoped
to the receiving flow.

| Parameter | Meaning |
| --- | --- |
| `repo` | `owner/name`, at most 200 UTF-8 bytes. |
| `kind` | `pr`, `issue`, or `release`. |
| `number` | PR or issue number, from 1 to 10000000. Ignored for releases (pass `1`). |
| `state` | `merged`, `closed`, or `any_change`. Ignored for releases (pass `any_change`). |

PRs count as merged when `merged` is true or `merged_at` is non-null; closed
means `state` is `closed` (including merged PRs). An issue cannot merge, so
`kind: "issue"` with `state: "merged"` stops as misconfigured on its first run.
Because this is event-triggered, that run happens on its first subscribed
event for its repository, not immediately on creation. `any_change` fires on the first event for
the selected number; a release watch fires on its first matching repository
release event. Basal admits only events published after the instance's first
approval, so it does not notify for older releases. When present and valid,
`html_url` becomes a link to the PR, issue, or release. No event in this v1
contract reports repository disappearance; without events the watch expires.

Example values:
`{"note":"The fix can ship now","repo":"owner/project","kind":"pr","number":42,"state":"merged"}`.

## `remind-at`

Requires `at`: an RFC 3339 UTC timestamp, at most three fractional digits,
no more than `365d` ahead. Past timestamps are refused at creation. Use an
`expires_in` that lasts through the requested time. The one-shot scheduler runs
once when due, or once after downtime (`missed: "once"`). The script writes the
note and requests its own end; basal also ends one-shot instances after the run
regardless of outcome. Unlike interval watches, a failed one-shot run has no
next scheduled check.

Example values:
`{"note":"Check the release rollout","at":"2027-01-01T09:00:00Z"}`.

## `github-ci-watch`

A standing watch with no instance-approval card replaces `gh run list` pollers. It consumes Plexus
**v2** `github_ci_job_failed` and `github_ci_run_completed` events. Both use
`repo` and `branch` header admission filters; nonmatching notices create no run
or body fetch. Basal checks the fetched body bytes against the event's digest;
Plexus v2 supplies the same CI fields for webhook and polling sources. The script
independently checks the body's repository and branch using the same patterns. Headers are hints,
not authority. Unknown body branches match only an all-branches `*` watch.

| Parameter | Meaning |
| --- | --- |
| `note` | A short label, at most **1024 UTF-8 bytes**, preserved in full beside CI details in the body. |
| `repo` | Repository pattern, at most 200 UTF-8 bytes, ASCII-case-insensitive. Only `*` is a wildcard, including across `/`, e.g. `cortexkit/*`. |
| `branch` | Branch pattern, at most 256 UTF-8 bytes, case-sensitive; `*` watches all branches. |
| `main_branch` | Exact branch whose failures wake the owner, at most 256 UTF-8 bytes, e.g. `main`. Other branches use `piggyback`. |

Every failed/timed-out job reports immediately, without waiting for a roughly
45-minute workflow to end. Job IDs are deduplicated in instance `kv`, and each
attempt uses `ci:<repo>:<run_id>:<run_attempt>` as its `replace_key`. Further
failed jobs update the same undelivered digest item. A rerun gets a separate
item. The summary lists failed jobs; the body includes a short head SHA, the
note, and **test names unavailable**. V2 has neither step/test names nor a
workflow name on job events. Links include failed jobs and the run URL when
known, up to eight total (the run link takes the final slot if needed).

A `failure`, `cancelled`, or `startup_failure` run completion updates the item
to say the run finished red. Without reported jobs, it creates a “run failed
with no failed job reported” item. A `success` completion updates an already
reported attempt to “now green” with `silent`; an unreported green is ignored.
Null conclusions and branches are displayed as `unknown`, never green. An
unknown conclusion on a run completion reports uncertainty rather than success.

State expires after about 14 days relative to the newest event `completed_at`;
older late deliveries are ignored. A 48,000-byte state cap also evicts oldest
attempts during bursts or when timestamps are unknown. If the run identity is
unknown, events are deduplicated by event key and use a plain digest rather than
merging unrelated attempts. Keys too large for core's 200-byte replacement
limit also use a plain digest. Unsupported replacement falls back to an
explicit plain write; it may therefore append instead of updating. The watch
never ends itself: it stands until expiry or an explicit end by its owner.

Example values:
`{"note":"Watch CI for the release","repo":"cortexkit/*","branch":"*","main_branch":"main"}`.
Capture-mode `flow.dry_run` can supply a synthetic v2 event with headers and the
normalized body; no live lookup or sink delivery occurs.
