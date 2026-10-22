# Codemode

Basal provides one agent-facing tool, `codemode({code, description?, limits?})`, on its tool route, where the daemon verifies the caller's principal and stamps its agent and scope identities. A short JavaScript program can await the agent's own tools, capture console output and return one JSON result. prefrontal-core, which manages agents and their tool grants, grants access to codemode and opens the run scope; basal owns the description, run records, budgets and rendered result. Tool schemas come from the daemon catalog, and each provider decides allow, ask and deny using the daemon-recorded agent identity and run scope, just as for a direct call. Basal also provides `tools.model` and `tools.classify`, which call Broca on the run's scope within a per-run model budget.

A keyed call derives its run ID from the stamped agent and call key; retries attach without replay. A keyless call executes once under an OS-random ID and warns that its reply cannot be recovered. Basal's parent process (outside the confined worker) caps wall time at 30 minutes plus a separate cumulative 10 minutes waiting only on people: the worker must be blocked and every in-flight tool call waiting for a person's approval or decision. It closes the scope before cancellation kills its worker, and interrupts leftover runs at startup. Long keyed calls can return before the program finishes; `late_results` lets the caller retrieve the finished result later, and `tool.withdraw` cancels the run. Codemode runs on macOS, Linux and Windows under each OS's full worker confinement. See [the tool and role operations](ops.md#codemode).

## Results

`codemode` opts into the fleet tool-provider `content/v1` contract through its
catalog entry's `result_format: "content/v1"`. Basal validates every result with
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

A keyed call that exceeds the foreground wait returns a valid content result
with `structuredContent: {"status": "running", "run_id": "codemode:..."}`. Its
text names each call held on a person, for example `Waiting on the person:
browser_use (call 2) needs their consent. The program resumes when they answer;
a run waits on people for at most 10 minutes in total. When the program
finishes, basal retains its final result for this call.` Without held calls it
reads `Still running: the program had not finished when this reply was due.`
followed by the same last sentence. Call numbers count from 1 in the order the
program made its calls. The final result is retrieved through `late_results`. Role-operation envelopes are not
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
`errorCode: "budget_exhausted"`, since content/v1 requires a snake_case code.
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

Basal decodes held replies with tool-provider 0.9.3 only when that specific tool
opts into `result_format: "content/v1"`. A route capability is not a format guess:
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
