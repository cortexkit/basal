# How a flow script runs

A flow script runs in `ck-basal-worker`, a separate process that confines
itself before it reads anything from basal. The script reaches the world only
through host calls: `sink.*`, `facts`, `ops.call`, `llm`, `classify`, `kv` and
the built-ins. basal checks each one against the approved manifest, in the
parent process, and journals it there. This note records two behaviours of
the script runtime that are deliberate limits, not guarantees.

## The script body is not a syntactic boundary

basal places the script text inside an async function before evaluating it
(`crates/basal-worker/src/engine.rs`, the `(async function () { ... })`
wrapper). That wrapper is built by joining text. A script can close the
function early and put statements outside it. Those statements run when the
source is evaluated, just before the flow function starts.

They gain nothing. They run in the same confined worker, under the same
lockdown, memory budget and JavaScript CPU budget as the rest of the script.
Any host call they make is checked and journaled like any other. The code
hash, and so the consent card, covers the script's exact text, so the
approved code is exactly what runs. Treat the wrapper as a convenience for
`await` and `return`, not as a sandbox boundary.

## A script can make its failure look like a stack overflow

QuickJS reports call-stack exhaustion as a plain `RangeError` with the message
`Maximum call stack size exceeded`. It has no flag that says the engine raised
it. So when a run ends with an uncaught `RangeError` carrying exactly that
message, basal reports the run as having exhausted its stack budget, whether
the engine raised it or the script threw it itself.

Only that label is affected. The memory and CPU budgets are detected from
signals inside the worker that no script can produce: the allocator refusing
an allocation, and the thread's CPU clock. A script therefore can't fake
either of those. A script that throws the stack message only misreports why
its own run failed.
