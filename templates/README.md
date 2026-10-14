# One-shot watches and reminders

Each directory is a flow template: register its exact `manifest.json`
and `flow.js` bytes with `package.register`, approve that version once, then
create watches with `flow.instantiate`. Each instance writes only to the agent
that created it, with the `wake` digest action, and ends after its notification.
There are no model calls or network fetches. Template versions have their own
approval card; instances do not go through ordinary flow-install approval.

All templates require `note`: a string of at most **4096 UTF-8 bytes**, without
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
Transient check errors leave the watch live for its next trigger. Permanent
configuration or access failures produce a stopped item and end the watch.
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

Template version **2** uses Plexus **v1** `github_pr_changed`, `github_issue_changed`, and
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
