# File, git and network built-ins

A flow's script runs in a sandboxed QuickJS worker with no file or socket access, and that does not change. When a flow needs to read a file, look at a git repository or fetch a page, it asks basal through a built-in. The built-ins run in basal's parent process, outside the sandbox. Each one is granted by a line in the manifest and shown on the install card. basal checks every call against the approved manifest before it is sent, and journals it like any other host call, so a replay returns the recorded answer and never reads or fetches again.

The code is `crates/basal-host/src/builtins/` (the built-ins themselves), `crates/basal-core/src/manifest.rs` and `install.rs` (the manifest lines and install checks) and `crates/basal-core/src/authorize.rs` (the check before dispatch).

## Manifest lines

All three lines are optional. A line that is absent grants nothing, and every call of that kind is refused.

```json
"fs":  { "read": ["/abs/root", "~/notes"], "write": ["/abs/out"] },
"git": { "read": ["/abs/repo"] },
"net": { "fetch": [ { "host": "api.github.com" },
                    { "host": "hooks.example.com", "methods": ["POST"] } ] }
```

| Line | Grants |
|---|---|
| `fs.read` | `fs.read`, `fs.list` and `fs.stat` on paths under these roots. |
| `fs.write` | `fs.write` on paths under these roots. A write root grants no reads; list it under `fs.read` too if the flow reads what it writes. |
| `git.read` | Every git built-in, on exactly these repositories (the directory holding `.git`). A directory inside a repository is not the repository. |
| `net.fetch` | `net.fetch` to these hosts. `methods` defaults to `GET` and `HEAD`; any other method must be listed. |

Install refuses:

- a root that is not absolute after `~` expansion, that holds a `..` component, or that does not exist;
- a wildcard host (`*.example.com`);
- an IP-literal host, in any spelling (`127.0.0.1`, `[::1]`, `2130706433`, `127.1`);
- a host that is not a lower-case DNS name;
- a method other than `GET`, `HEAD`, `POST`, `PUT`, `PATCH` or `DELETE`, and an empty `methods` list.

The install card shows each line, with a host's default methods spelt out. A flow that reads private facts text (`facts` with `text: true`) and holds any `net.fetch` grant gets a warning on its card, once for each host: such a flow could send that text out in a URL, a header or a body.

## Script API

Every built-in is an async function that returns a promise. A refusal or a failure rejects it with an error whose `data.code` names the reason (below).

### Files

- **`fs.read(path, {maxBytes?})`** returns `{text}`. It refuses a file that is not UTF-8 text, and one larger than `maxBytes`. That cap defaults to 1 MiB and can be raised to 8 MiB at most.
- **`fs.list(dir)`** returns `[{name, kind}]`, sorted by name, where `kind` is `"file"`, `"dir"`, `"symlink"` or `"other"`. A directory with more than 1000 entries is refused.
- **`fs.stat(path)`** returns `{exists: true, kind, size, mtime_ms}`. A path that does not exist returns `{exists: false}`, not an error, but only if its parent directory is inside a root.
- **`fs.write(path, text)`** replaces the whole file atomically: basal writes a temporary file in the same directory, syncs it, and renames it over the target. It returns `{bytes}`. The text is at most 1 MiB. This version has no append and no delete.

### Git

- **`git.log(repo, {ref?, path?, sinceMs?, maxCount?})`** returns `[{sha, subject, author_ms}]`, newest first. `ref` defaults to `HEAD`, and `maxCount` to 20 (at most 500).
- **`git.revParse(repo, ref)`** returns the commit sha that `ref` names. An annotated tag gives the commit it points at.
- **`git.describeTags(repo, {pattern?})`** returns `{nearest, tags}`: the nearest tag reachable from `HEAD` (or `null`), and every tag, newest first. With `pattern` (a glob such as `"v1.*"`), both are limited to matching tags.
- **`git.show(repo, rev, path)`** returns the text of `path` at `rev`.
- **`git.diff(repo, from, to, {path?})`** returns the unified diff text from `from` to `to`.

A ref, revision or pattern cannot start with `-`, and a path inside a repository must be relative with no `..`. Git output is capped at 1 MiB, and a git command may run for at most 20 seconds.

### Network

- **`net.fetch(url, {method?, headers?, body?})`** returns `{status, headers, body}`. `method` defaults to `GET`. `body` is text. The response's header names are lower-cased, and `Set-Cookie` is dropped. The response body is returned as text and capped at 1 MiB.

What happens when basal can't use the answer depends on whether the server may already have acted on the request. Nothing can have changed while every request so far was a `GET` or `HEAD`. In that case an oversized body, a body that isn't UTF-8, or an answer basal can't read rejects the call with the matching error code. Once a request with any other method has started to leave, the server may have acted on it. From then on, no answer is reported as a refusal. Instead:

- **The body is refused** (over the cap, or not UTF-8): the call returns `{status, headers, body: null, body_error}`. `body_error` is `"too_large"` or `"not_utf8"`, so the script still sees what the server said.
- **The answer can't be read** (no HTTP head, a head over its cap, a malformed `Content-Length` or chunk): the outcome is unknown. The call fails as a transport failure that may have been sent, with the reason `reply_unreadable`. As with any unknown outcome of a call that is not safe to repeat, the run stops in `needs_reconcile`. An operator then records what happened through a decision card, and the run continues.

### Error codes

| `data.code` | Meaning |
|---|---|
| `denied` | Outside what the manifest grants: a path outside the roots, a repository or host not listed, a method not allowed, a private address, a forbidden header. |
| `invalid_arguments` | The arguments are malformed: a relative path, an unknown option, a value out of range. |
| `not_found` | The file, repository or revision does not exist. |
| `too_large` | Over a cap: file size, listing length, output or response body. |
| `not_utf8` | The file or response is not UTF-8 text. |
| `io_error`, `git_failed`, `tls_failed`, `net_failed`, `timeout` | The operation itself failed. |

## Scope and safety rules

### Paths

- Every path is resolved with `realpath`, which follows symlinks and removes `..`. The resolved path must lie under an approved root of the call's kind (`fs.read` for reads, `fs.write` for writes), itself resolved the same way. A `..` escape and a symlink that points outside a root are both refused.
- For `fs.write`, and for `fs.stat` of a path that does not exist, basal resolves the parent directory instead and keeps the last name as given. The parent must lie inside a root.
- A symlink can be swapped in between the check and the open. basal closes that window as far as macOS allows. It opens with `O_NOFOLLOW`, so a symlink swapped into the last component makes the open fail. After the open, it asks the kernel where the opened file really is (`F_GETPATH`) and refuses the file if that is outside the roots, which catches a directory higher up swapped for a symlink. What stays open: between the open and that second check, a read may already have the file open. basal reads nothing before the check passes, so the contents are never returned. The open itself can still block briefly on a FIFO planted in place of the file, which is why reads open non-blocking.
- `fs.write` creates its temporary file with `O_CREAT | O_EXCL | O_NOFOLLOW` in a parent directory opened and checked the same way. It refuses a target that is already a symlink, so a write never lands outside a root through a link.

### Git

basal runs the `git` program rather than an in-process reader such as gix. git gives exactly git's own answers for revision syntax, `describe`, pathspecs and diffs, and the parent gains no large dependency tree. The cost is that a repository's own `.git/config` can make git run a program, and whoever controls that file is not necessarily the operator who approved the flow. So every git command runs:

- with the environment cleared except `PATH` and `HOME`, plus `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0` and `GIT_NO_LAZY_FETCH=1`, and with a null stdin;
- with `--no-optional-locks --no-pager --literal-pathspecs -c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.pager=cat -c log.showSignature=false -c protocol.allow=never`;
- with `--no-ext-diff --no-textconv` wherever git would otherwise run a diff driver or text filter.

A test plants a repository whose config names an fsmonitor program, an external diff, a pager and a hooks directory, each writing a marker file. It shows that plain git runs the planted fsmonitor and that no built-in runs any of them.

### Network

- HTTPS only. The host must be approved, and the method allowed for that host.
- Redirects are followed by hand, at most 5. Every hop is checked again from the top: scheme, host, method and address. A script's own headers go only to the host it named.
- A redirect that may not be followed refuses the call only while nothing has changed. Once a request with a method other than `GET` or `HEAD` has reached a server, the server may already have acted on it. If basal then stops following redirects (for example, because the next host is not approved), the call ends with the 3xx response basal did not follow (`{status, headers, body}`) as its answer, not with a refusal. The rules above for a refused body apply to that response too.
- Before connecting, basal resolves the host once. If any address it resolves to is private, loopback, link-local, multicast, unspecified or otherwise reserved, the fetch is refused. An IPv4-mapped IPv6 address is judged by the IPv4 address it carries. The connection then goes to an address from that same checked answer, so a second DNS answer cannot swap in another. TLS still verifies the certificate against the host name, using the bundled web roots and never the system store.
- Connecting times out after 10 seconds, and a whole fetch, redirects included, after 30 seconds. The response head is capped at 64 KiB and the body at 1 MiB.
- No cookies are sent or kept, and no credentials or proxy settings are read from anywhere. A script may not set `Authorization`, `Proxy-Authorization`, `Cookie`, `Host` or the headers that frame the request.

### Authorization

- Every call is checked in the parent against the manifest of the version the run was approved under, before it is journaled for dispatch. A refusal is journaled as a rejection, as a refused op is, and the script can catch it.
- The scope that was checked travels with the journaled request, and the built-in host checks it again before acting. A run therefore keeps the grant it was approved under, and a recovered run is checked against the same grant.
- Audit rows record the built-in's name and a digest of its arguments. They never record file contents, diffs or bodies.
- A dry run in capture mode sends no built-in. A live dry run, which only the operator may start, sends only the built-ins that read, and only within the manifest's scope. `fs.write` and a `net.fetch` with a method other than `GET` or `HEAD` are captured.

## Journal classes

Each built-in uses one of the dispatch classes that module ops already use. What recovery does after a crash depends on the class:

| Built-in | Class | After a crash with no recorded outcome |
|---|---|---|
| `fs.read`, `fs.list`, `fs.stat` | query | Sent again. |
| every git built-in | query | Sent again. |
| `net.fetch` with `GET` or `HEAD` | query | Sent again. |
| `fs.write` | keyed mutation | Sent again under the same idempotency key. Writing the same bytes again leaves the same file. |
| `net.fetch` with any other method | mutation | Not sent again. The run stops in `needs_reconcile`, and the operator resolves the call through the existing decision cards. |
