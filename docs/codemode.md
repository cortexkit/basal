# Codemode

Basal provides one agent-facing tool, `codemode({code, description?, limits?})`, on its tool route, where the daemon verifies the caller's principal and stamps its agent and scope identities. A short JavaScript program can await the agent's own tools, capture console output and return one JSON result. prefrontal-core, which manages agents and their tool grants, grants access to codemode and opens the run scope; basal owns the description, run records, budgets and rendered result. Tool schemas come from the daemon catalog, and each provider decides allow, ask and deny using the daemon-recorded agent identity and run scope, just as for a direct call. Model tools are a later extension.

A keyed call derives its run ID from the stamped agent and call key; retries attach without replay. A keyless call executes once under an OS-random ID and warns that its reply cannot be recovered. Basal's parent process (outside the confined worker) caps wall time at 30 minutes plus a separate cumulative 10 minutes waiting only on people: the worker must be blocked and every in-flight tool call waiting for a person's approval or decision. It closes the scope before cancellation kills its worker, and interrupts leftover runs at startup. Long keyed calls can return before the program finishes; `late_results` lets the caller retrieve the finished result later, and `tool.withdraw` cancels the run. Codemode runs on macOS, Linux and Windows under each OS's full worker confinement. See [the tool and role operations](ops.md#codemode).

## Results

`codemode` opts into the fleet tool-provider `content/v1` contract through its
catalog entry's `result_format: "content/v1"`. Basal validates every result with
`cortexkit_role_tool_provider::result::validate` before exposing it. Direct
responses, completed withdrawal replies and retained late results use the same
result body:

```json
{
  "content": [{"type": "text", "text": "completed · 10 ms · 1 calls · person wait 0 ms\n..."}],
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
`structuredContent`. Codemode emits one text part, byte-for-byte the previous
rendered text for a run that fits: status/duration/call-count/person-wait line,
error if present, JSON return value if present, console output, call-summary
table, then warnings. The JSON return value is capped at 16 KiB and console
output at 64 KiB by the worker. No text is synthesized from the machine fields
by the consumer.

`structuredContent` retains the previous machine-readable fields, including
status, return value, console output, call summaries, warnings, optional error
and provider detail. It adds the run id and `termination_reason` (the terminal
status, including any budget suffix). Duration, call count and person-wait time
are the existing budget accounting fields, not a new CPU/token measurement.

A keyed call that exceeds the foreground wait returns a valid content result
with `structuredContent: {"status": "running", "run_id": "codemode:..."}` and the
text `running: the final result will be retained in late_results.` The final
result is retrieved through `late_results`. Role-operation envelopes are not
themselves content results; their nested completed results are.

## Errors and refusals

Protocol errors remain protocol Error frames: unserved call modes
(`call_mode_unsupported`), malformed requests, schema/semantics pin failures and
admission refusals preserve their existing codes, messages and optional detail.
They do not acquire a ToolResult in the Error frame's detail.

A terminal program failure, uncaught tool refusal, budget stop or cancellation
is a content result with `isError: true` and a stable `errorCode`. Its model text
is unchanged, including partial output and known call outcomes. A caught tool
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
