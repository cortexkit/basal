# Codemode

Basal provides one agent-facing tool, `codemode({code, description?, limits?})`, on its tool route, where the daemon verifies the caller's principal and stamps its agent and scope identities. A short JavaScript program can await the agent's own tools, capture console output and return one JSON result. prefrontal-core, which manages agents and their tool grants, grants access to codemode and opens the run scope; basal owns the description, run records, budgets and rendered result. Tool schemas come from the daemon catalog, and each provider decides allow, ask and deny using the daemon-recorded agent identity and run scope, just as for a direct call. Basal also provides `tools.model`, which calls Broca within the existing per-run budget of 20 calls and 200,000 tokens. Classification uses Broca's catalog tool `tools.llm_classify`, not a Basal built-in.

## Classifier dollar budget

Tools carrying the shared contract's `llm.classify/v1` tag share **$0.50 per program run**.
`limits: {model_cost_usd: 0.30}` may lower that budget (including to zero), never raise it.
The caller supplies `{model, questions, item, max_cost_usd?}`; `model` is its chosen
`provider/model` id. Basal supplies an omitted ceiling from the remaining budget,
or clamps a supplied ceiling to that remainder before sending it to Broca.
Concurrent calls reserve their whole ceilings, so they cannot spend the same remainder.
With nothing left, the promise rejects before sending with `budget_exhausted` and
`budget: "model_cost_usd"` in its structured error.

A successful `{answers, usage, cost_usd}` charges the reported `cost_usd`, including
zero-rated calls. If Broca omits a field from `usage`, Basal leaves it absent in
the result. `cost_unmeasured` charges the
reserved ceiling; known refusals before any model call charge nothing. Unknown
outcomes retain their reservation and are never automatically retried. Broca's
ceiling bounds its estimate, **not its actual bill**: reported cost can exceed the
ceiling and run budget, and Basal records that overrun rather than clipping it.
Result text lists dollars spent and remaining; structured results carry
`budgets.model_cost_usd: {limit, spent, remaining}` (remaining may be negative).

Identification uses the contract tag only, never a provider or tool-name guess.
Until Broca advertises the tag, an untagged `llm_classify` remains an ordinary
program tool call **without Basal's dollar accounting or ceiling injection**;
Broca still requires and enforces the caller's `max_cost_usd` on each call.
Flows instead refuse installation of untagged tools whose schema requires that
ceiling; see [manifest dollar caps](manifest.md#classifier-dollar-caps).

A keyed call derives its run ID from the stamped agent and call key; retries attach without replay. A keyless call executes once under an OS-random ID and warns that its reply cannot be recovered. Basal's parent process (outside the confined worker) caps wall time at 30 minutes plus a separate cumulative 10 minutes waiting only on people: the worker must be blocked and every in-flight tool call waiting for a person's approval or decision. It closes the scope before cancellation kills its worker, and interrupts leftover runs at startup. Long keyed calls can return before the program finishes; `late_results` lets the caller retrieve the finished result later, and `tool.withdraw` cancels the run. Codemode runs on macOS, Linux and Windows under each OS's full worker confinement. See [the tool and role operations](ops.md#codemode).

## Pauses

The worker has no timers. A program pauses with two globals, defined before
program code runs and not on `tools`, so no provider's tool name can clash
with them:

- `await sleep(ms)` waits `ms` milliseconds, an integer from 0 to 10,000;
  anything else throws a `RangeError` naming the limit. The worker sends the
  request to basal's parent, which completes it when the time has passed on
  the run's clock. A sleep counts against the run's 30-minute wall time, never
  against the 10 s JavaScript CPU budget or the 200-call budget, and has no
  row in the call summary. If the wall deadline comes first, the run ends at
  the deadline as `budget_exhausted:wall`; cancelling the run ends a sleep at
  once. Time while a program is sleeping is never person-wait time, even when
  its tool calls are all held on a person.
- `await waitFor(condition, {timeout, interval})` calls `condition()` (which
  may be async) and returns its first truthy value; an error `condition`
  throws propagates unchanged. Between attempts it sleeps `interval` ms
  (default 500; lower values are raised to 250), with the last sleep cut short
  to end at the timeout. When an attempt returns a falsy value once `timeout`
  ms have passed (default 30,000, at most 30,000; a larger value throws a
  `RangeError`), it throws an `Error` whose `code` is `wait_timeout` and whose
  message names the timeout and the number of attempts. Elapsed time is the larger of the time the
  worker's clock shows has passed and the total time slept. Tool calls made inside `condition` count against
  the call budget as usual.

```js
const view = await waitFor(async () => {
  const snapshot = await tools.browser_use({action: "snapshot"});
  return snapshot.title === "Done" && snapshot;
}, {timeout: 20000, interval: 1000});
```

Flows have neither helper: a flow activation is journaled and replayed, and a
pause has no outcome to replay. A flow worker that asks for a sleep is stopped
with `profile_violation`.

## Results

`codemode` opts into the fleet tool-provider `tool-result/v1` contract through its
catalog entry's `result_format: "tool-result/v1"`. Basal validates every result with
`cortexkit_role_tool_provider::result::validate` before exposing it. Direct
responses, completed withdrawal replies and retained late results use the same
result body:

```json
{
  "content": [{"type": "text", "text": "completed · 10 ms · 1 calls · person wait 0 ms\n\nReturn value:\n{\n  \"answer\": 42\n}\n\nTool | Outcome | Waited on person | Duration (ms)\n--- | --- | --- | ---\necho | completed | false | 8\n"}],
  "structuredContent": {
    "run_id": "codemode:...",
    "status": "completed",
    "termination_reason": "completed",
    "duration_ms": 10,
    "person_wait_ms": 0,
    "call_count": 1,
    "calls": [{"tool": "echo", "outcome": "completed", "waited_on_person": false, "duration_ms": 8}],
    "value": {"answer": 42},
    "output": "",
    "warnings": [],
    "keyless": false,
    "catalog_digest": "..."
  }
}
```

## Model text and program data

Consumers build model input from ordered `content` parts only, never from
`structuredContent`. Codemode emits one text part. For a run that fits the
size bound it holds, in order: the status/duration/call-count/person-wait line,
the classifier dollar budget line,
the error if present (`Error (<code>): <message>`, plus an `Error detail:` line
with the provider detail as JSON when there is one), the return value if
present, console output verbatim under `Output:` when there is any, the
call-summary table (a header, a `--- | --- | --- | ---` separator and one row per call) when
the program made calls, then warnings.

The return value is rendered for reading: a string verbatim, without quotes and
with real line breaks; an object or array as JSON pretty-printed with a 2-space
indent (strings inside it are still JSON-escaped); a number or boolean as JSON;
and null, which is also what returning `undefined` gives, as `Return value:
none`. `structuredContent.value` always holds the raw JSON value, unchanged.
The JSON return value is capped at 16 KiB and console output at 64 KiB by the
worker. No text is synthesized from the machine fields by the consumer.

`structuredContent` carries the machine-readable fields: status, return value,
console output, call summaries, warnings, optional error and provider detail,
the run id, and `termination_reason` (the terminal status, including any budget
suffix). Duration, call count and person-wait time are the run's budget
accounting fields, not a separate CPU or token measurement.

A keyed call that exceeds the 25-second foreground wait returns an interim content
result with `structuredContent: {"status": "running", "run_id": "codemode:..."}`.
On a route that declared both `tool-result/v1` and `held-reply/v1`, it also carries
`held: {"reason": "running"}` so the custodian retrieves the final answer through
`late_results`. When person-held calls have provider reasons, basal instead mirrors
the first available reason in call order, including open provider reasons such
as `consent`, `login_handoff` and `login_capture`; if none has a reason, it uses `running`. `running` means work continues
without asking the person or agent to act. Without both route declarations,
basal keeps the unmarked running reply (and the legacy shape without `tool-result/v1`).
Keyless calls wait for the final answer and never receive a held reply. The interim
text names each call held on a person, for example `Waiting on the person:
browser_use (call 2) needs their consent. The program resumes when they answer;
a run waits on people for at most 10 minutes in total. When the program
finishes, basal retains its final result for this call.` Without held calls it
reads `Still running: the program had not finished when this reply was due.`
followed by the same last sentence. Call numbers count from 1 in the order the
program made its tool calls; sleeps are not counted. The final result is retrieved through `late_results`. Role-operation envelopes are not
themselves content results; their nested completed results are.

## Errors and refusals

Protocol errors remain protocol Error frames: unserved call modes
(`call_mode_unsupported`), malformed requests, schema/semantics pin failures and
admission refusals preserve their existing codes, messages and optional detail.
They do not acquire a ToolResult in the Error frame's detail.

A provider that does not serve a tool in codemode, or whose tool catalog can't be
read or is invalid, does not refuse the run. Admission drops the affected tools
(one tool for `provider_not_opted_in` or `call_mode_unsupported`, every tool from
that provider for `catalog_unreadable` or `invalid_answer`) and records a
`tools_unavailable` warning naming the provider, the dropped tools and the reason.
A program call to a dropped tool rejects with `tool_unavailable` and
`detail: {"reason": "<reason>", "module": "<provider>"}`; nothing is sent.

An invalid individual catalog entry also does not refuse the run. Admission
drops entries with malformed fields, invalid names or uncompileable input
schemas and records a `tool_entry_invalid` warning for each, with the readable
module, tool name (or zero-based entry index), reason and a plain-language
message. Duplicate names drop every claim to that name and produce one warning
per name; basal never picks an arbitrary provider. A dropped entry with a safe
identifier remains callable only to reject with `tool_unavailable`, naming the
reason. This also applies when a provider claims `model`: the
provider entry and basal's built-in entry are both dropped, and that name only
rejects as unavailable. Invalid identifiers and reserved names (`__proto__`, `constructor`,
`prototype`, `tools`, `console`) are never exposed, even as unavailable tools.
Unavailable-provider names receive the same checks. These warnings appear in
both result text and `structuredContent.warnings`; the catalog digest covers
only retained usable entries. A catalog that is not an array still refuses the
whole run with `invalid_catalog`.

A terminal program failure, uncaught tool refusal, budget stop or cancellation
is a content result with `isError: true` and a stable `errorCode`. Its model text
keeps partial output and known call outcomes.

When a program fails because it left a tool call's rejection uncaught, the run
error carries that call's code (the provider's own, such as `no_session`, or
basal's, such as `tool_unavailable`) and a one-line plain-text message naming
the tool and call: `browser_use (call 1) failed: No session. Call
browser.launch, then retry the browser action. Reason: no browser session is
open.` Any other thrown error is code `script` with the JavaScript `Name:
message`; codemode hides stack traces, so no line is given. Engine failures keep
their codes with a plain-text message.

A program that reads a name `tools` does not have, such as `tools.work`, gets a
`TypeError` naming it and listing the run's tools, for example `tools.work is not
available in this codemode run. Available tools: bash, echo (2). Not every tool
is available inside programs yet.` `Object.keys(tools)`, `name in tools` and
`typeof tools.<listed name>` work as usual, and `then`, `toJSON` and symbol keys
read as undefined, so `await tools` and `JSON.stringify(tools)` don't throw. A caught tool
refusal does not turn a successfully completed program into a failure; the call
summary still records the refusal. Persisted terminal run-scope failures also
retain their codes and details in the content result.

Budget stops map the existing `budget_exhausted:<budget>` code to
`errorCode: "budget_exhausted"`, since tool-result/v1 requires a snake_case code.
The original code remains in `structuredContent.error.code`, the status stays
unchanged, and `structuredContent.budget` identifies the specific limit (for
example `"js_cpu"` or `"tool_calls"`) without requiring string parsing. Other
terminal error codes are unchanged. A success has neither `isError` nor
`errorCode`.

## Size bound

The entire serialized result, including machine data and JSON escaping, is
strictly **under 3 MiB**. Normal runs retain all text and machine data. As a
backstop for oversized console output or summaries, model text is bounded to
the wire budget minus 4 KiB of framing and metadata headroom using its serialized
JSON size, with a UTF-8-safe prefix
and an explicit `[Model text truncated for the 3 MiB result limit.]` marker.

If the complete reply still exceeds the bound, the largest reducible machine
fields (such as call summaries, provider detail or output) are replaced one at
a time with `{"omitted": true, "original_bytes": n}`. `original_bytes` counts
the field's original serialized JSON bytes. An oversized error message may be
replaced, but its code is preserved. The model text gains one line:
`Structured fields were omitted for size.` Run id, status, termination reason,
error code, specific budget and existing budget accounting are never omitted.
Size reduction never changes a successful run into an error: the program may
already have made writes, and reporting a failure could encourage unsafe
retries.

See [operations](ops.md#results-and-long-calls) for late-result custody,
retention, withdrawal and scope closure.

## Call outcomes

Per-call outcomes include `ok`, `denied`, `consent_unavailable`, `tool_unavailable`,
`error`, `refused`, `outcome_unknown`, `cancelled` and `withdrawn`. `cancelled` means the
call was never sent. `withdrawn` means it was sent and held, but the provider proved
its action never started (by `tool.withdraw` or a late `not_started` entry). Dispatch
and entry timestamps remain recorded. A started action or an uncertain withdrawal
stays `outcome_unknown`; basal never resends it. Held replies are interim, not values:
the program resumes only on a final late result. Onward content results return
`structuredContent` when present, otherwise their joined text parts; `isError: true`
rejects the promise.

## Consent, then a fresh call

Basal decodes held replies with tool-provider 0.13.0 only when that specific tool
opts into `result_format: "tool-result/v1"`. A route capability is not a format guess:
legacy tools retain their original JSON bodies. If a provider catalog is unreadable,
admission drops that provider's tools with a retry-may-succeed warning and the run
starts with its other tools; there is no guessed raw fallback. `consent`, `login_handoff`,
`login_capture` and unknown well-formed held reasons all keep the await pending.
Transient transport failures retry custody reads, not tool actions; a reconnect
reopens only the same run-scoped custody route. Definitive scope end still ends
the unanswered call as unknown.

Cerebellum does not execute an approval-gated action after the person wait: the
page, coordinates or other state may have changed. Approval instead arrives as
`not_started` with provider detail `{consent: "approved"}` (Cerebellum's other values are `denied`, `withdrawn` and `expired`). Basal records
`withdrawn` and rejects the promise with `error.code === "not_started"` and that
object in `error.detail`. The grant is now active, so the program can choose a new
call without a second approval card. Denial is a terminal refusal; do not treat it
as approval. Login handoff and capture settle with their actual result.

For example, if `browser_use` is in the admitted catalog, take a fresh snapshot
rather than replaying old coordinates:

```js
let view;
try {
  view = await tools.browser_use({action: "snapshot"});
} catch (error) {
  if (error.code !== "not_started" || error.detail?.consent !== "approved") {
    throw error;
  }
  // This new call observes the current page under the newly approved grant.
  view = await tools.browser_use({action: "snapshot"});
}
return view;
```

Every explicit call has a new journal position and call key. Basal never retries
the original action automatically. One provider intake worker serves every run
holding calls there; final results and their acknowledgements are coordinated
across runs, so one run ending cannot interrupt another.
