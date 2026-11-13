# Architecture

basal is the fleet's flow runtime, and it also provides the agent-facing `codemode` tool. A **flow** is a small JavaScript script with a JSON manifest. It runs on a schedule (recurring, or once at a set time) or on a module event. It reads module data and facts, decides what to do, and sends its output to agents through prefrontal-core ("core"). A **codemode run** is a short JavaScript program an agent sends; it calls that agent's own tools and returns one result.

Both run in the same kind of confined JavaScript worker, and both follow one rule: the worker computes, and the parent decides. The worker cannot authorize a call, record an outcome, or reach a file, socket or module except through the parent.

`STRUCTURE.md` maps the directories and says where new code goes. The `docs/` folder holds the contracts (manifest, script runtime, built-ins, ops, codemode, sandbox, deploy).

## Processes

| Process | Crate | Role |
|---|---|---|
| `ck-basal` | `basal-module` | The supervised subc module, id `basal`, principal `reserved:basal`. It owns the store, the run log, the worker pools, the scheduler loop, the event reader, every op and both tools. |
| `ck-basal-worker` | `basal-worker` | A stateless QuickJS engine. It confines itself before it reads its first frame (`docs/sandbox.md`), then runs one activation at a time over `basal-proto` frames on its stdio. |

The parent finds the worker by name, `<own executable name>-worker` in its own directory, so a `ckdev-basal` finds `ckdev-basal-worker`. Production binaries are placed by SUBC from staged cards (`docs/deploy.md`), never by basal.

Two pools hold workers (`basal-module/src/pool.rs`, `module.rs`):

- **Flow pool:** up to 16 workers, 2 warm unbound spares. A worker is bound to the first flow it serves and is only ever reused for that flow, until it has run 256 activations or sat idle 10 minutes. A compromised engine therefore keeps nothing it can use against another flow. A dry run's worker serves one activation and is retired, because a dry run executes unapproved code.
- **Codemode pool:** up to 16 workers, no spares, one activation each. Every program gets a fresh worker.

Spares exist because on macOS the first launch of a freshly signed binary has been measured stalling for over a minute.

## Trust boundaries

- **The worker is untrusted.** It runs approved code that is still untrusted input to a C engine. Confinement differs per OS: seccomp plus Landlock on Linux, a deny-default Seatbelt profile on macOS, and on Windows a Less Privileged AppContainer launch that the worker lowers to an Untrusted-integrity token before reading input. The parent refuses a worker whose confinement report does not pass. Workers start with an empty environment and three anonymous pipes (`process.rs`).
- **Callers are who the daemon says they are.** `caller.rs` decides every caller from the principal and scope the daemon stamped on the inbound route, never from request fields. The operator is the attested `reserved:callosum`; core is `reserved:prefrontal-core`; an agent is a head scope carrying its `agent_id`; a direct unscoped key-holder is a local caller. Each op's table of who may call it is in `ops.rs`.
- **basal carries authority; it does not mint it.** Core registers each flow's route scope (keyed by flow id and version) and each codemode run's scope. basal opens routes on those scopes, and the target module checks its own grants against the daemon's stamp. Core's sinks, `board.read` and `work.unsettled` are the exception: they go over basal's own `reserved:basal` route carrying the flow's `{flow_id, flow_version, code_hash}`, which core checks against the approved install (`basal-host/src/core_host.rs`).
- **Approval binds to the exact bytes.** A flow version's code hash is BLAKE3 over its script and manifest bytes (`ids.rs`). Core renders the install card from the hashed manifest, and before every activation basal asks core whether it still stands behind that hash (`gate.rs`).

## Terms

- **Version:** one exact script plus manifest under a flow id. Versions are immutable; an edit is a new version and a new approval.
- **Install card / decision card:** an install card asks the operator to approve a version; core owns it. A decision card asks the operator something only they may answer about a running flow (an unknown call outcome, re-enabling an auto-disabled flow, a lost grant); basal raises it on Cingulate's `consent/v1` (`decisions.rs`, `basal-host/src/cingulate_consent.rs`).
- **Trigger:** what admits a run: a schedule fire (`cron`, `interval`, or a template instance's one-shot `at`) or a module event. A dry run executes against synthetic triggers in a scratch store and admits nothing real.
- **Run / activation:** a run is one execution of a version for one trigger. An activation is one stretch of that run on one worker; a run that waits on remote work, or whose worker died, needs another activation.
- **Call:** one host call a script makes: a module op, a fact, a sink write, a model call, a built-in (`fs`, `git`, `net.fetch`), or a local one (`kv`, `Date.now()`, `Math.random()`). Its **class** (`query`, `keyed_mutation`, `mutation`, `local`, `sync`; `model.rs`) decides whether it may be sent again: only a query or a keyed mutation can never cause a second effect.
- **Intent / arrival:** the record of a call before it leaves basal, and the record of its outcome when it comes back.
- **Unknown outcome / reconcile:** a call that may have reached its target but whose outcome basal cannot prove. Basal never resends it; the operator settles it with `flow.reconcile` or a decision card.
- **Template / package / instance:** a template is an approved flow with bounded parameters; an instance is one agent's binding of it, created without another card. A package is the same mechanism with `$self` as its only parameter, instanced per agent by core's persona reconciler (`docs/templates.md`, `docs/packages.md`).
- **Sink:** a write to an agent's output through core: a digest item, a board lane, a board wait resolution, or a room post.

## State on disk

The daemon names the store path in HELLO_ACK. On this deployment it is `~/.local/share/cortexkit/basal/`:

| Path | What it is |
|---|---|
| `store.db` | SQLite, WAL, `synchronous=FULL` with `fullfsync`, 1 KiB pages. The control plane: flows, versions, installs and cards, schedules, event receipts, live-run rows, finished-run history, obligations, token and dollar ledgers, decision cards, outboxes, template instances, and all codemode records. Migrations are in `basal-core/src/run_cutover/schema.rs`. |
| `<hash>.lease` | The single-writer lease beside the store. Every write transaction checks it, so a replaced process cannot write. Backups copy it with `store.db`. |
| `run-log/` | The append-only run log: `e<epoch>/<sequence>.seg` segments and a directory `.lock`. It holds the in-progress state of live flow runs. It is never restored from a backup. |
| `dry-run/` | Scratch stores for dry runs, which never touch the real store. |
| `engram-catalog.json` | The backup catalog basal writes at startup for ENGRAM. |

SQLite and the run log split the work. Live flow runs are in-memory actors (`run_store/`) whose every record is appended to the run log; SQLite keeps a control row per live run (`live_runs`), and every run points at its version's script and manifest, stored once per version (`run_snapshots`). When a run ends, one SQLite transaction writes its summary to `run_history` and `run_calls` and settles its accounting. A successful run keeps no call payloads. Codemode runs do not use the run log; they live in the `codemode_*` tables.

Retention (`retention.rs`, `run_retention.rs`, `codemode/retention.rs`): run payloads 24 hours after a run ends, run summaries and call metadata 90 days, other history 30 days, codemode runs 24 hours. Trigger and idempotency tombstones stay forever, so a pruned trigger or run id can never run again. Anything an unsettled call, reservation or open card still needs is kept. The same pass deletes sealed run-log segments once no live run starts in them (`run_log/gc.rs`), on the first pass after start and then hourly.

## A flow, end to end

**Install.** `flow.install` (or the agent-facing `flow` tool, which calls the same handlers) decodes the manifest, refusing unknown fields and duplicate keys (`manifest.rs`), then validates it against the daemon catalog: every op exists, is served to flows, and the loop rule holds (`install.rs`). It records the version under its code hash, runs a capture-only dry run, and sends core `flow.install_request`. Core shows the card. When core reports a decision through `flow.install_decided`, basal treats it only as a hint and reads `flow.install_status` once before it approves anything (`cards.rs`, `install_policy.rs`).

**Triggers and admission.** Schedules are rows in `schedules`. A tick plans due fires and admits them in separate commits, so a crash can neither mint a fire twice nor lose one (`schedule/tick.rs`). Module events come from the JetStream consumer `m_basal`: each batch is committed to `event_receipts` and then acknowledged, every message, matched or not; a flow's runs queue oldest first (`basal-module/src/events.rs`, `basal-core/src/events.rs`). Admission creates exactly one run per trigger under the flow's currently approved version and snapshots that version for the run (`run_admission.rs`).

**The engine.** One loop (`basal-module/src/engine.rs`) ticks the scheduler, fails runs past their deadline, runs maintenance and retention, tops up the pools, raises decision cards, and starts runnable runs: at most one per flow, and at most 4 activations at once. Between passes it sleeps until the earliest due time computed in `maintenance.rs` (with a 30 s fallback), or until new work is signalled.

**An activation.** The install gate asks core whether the version is still `active` with this code hash. Then the driver (`run_driver/`) hands the worker the script and the run's recorded prefix to replay, and serves new calls. For each call:

1. The parent authorizes it against the run's approved manifest, before anything is sent (`authorize.rs`). A refusal is recorded too.
2. The call's intent is appended to the run log and synced before it is dispatched. Nothing leaves basal without a durable record of it (`driver.rs`).
3. It is dispatched on its own thread to a host (`basal-host`): module ops over the flow's scope, sinks and facts through core, model calls through Broca, built-ins in the parent itself.
4. The outcome is recorded as an arrival, and the worker receives outcomes in a recorded delivery order. Arrivals and delivery order may be written lazily, but they are synced before any later effect that depends on them leaves basal. Replay of a crashed activation follows the durable prefix exactly; past it, the script runs again.

Local calls (`kv`, clock, random) are served in the parent and recorded in the run log; values past the durable prefix may differ when a crashed run re-executes. Rate, dispatch and token limits are checked per flow (`rate.rs`, `tokens.rs`); dollar caps per run (`classify_cost.rs`).

**The end.** The terminal committer writes the run's history, settles its accounting, and closes the actor (`run_store/committer.rs`, `run_driver/terminal.rs`). Repeated failures latch an owner notice to the flow's author (`owner_notice.rs`). Sustained saturation auto-disables a flow until the operator re-enables it (`rate.rs`).

**Crash and restart.** Startup opens the store, runs migrations, and recovers before anything else can read or drive a run (`module.rs`). Recovery rebuilds every live run's actor from the run log and puts it back in line. A call with an intent but no arrival is resolved by its class: a query can be asked again, a keyed mutation is resent under the same idempotency key, and anything else becomes an unknown outcome for the operator. If the log looks restored from an older copy (`run_log/restore.rs`), affected runs are marked history-lost and wait for the operator; they are never resumed.

## Model calls

A flow's `llm` call, and codemode's `tools.model`, select under `basal.flow` and `basal.codemode` through the `routing/v1` provider's `route.select`. Basal declares both consumers and the `basal.flow.classify` / `basal.codemode.classify` consumers used for classification, admits Broca as a selector for all four, and records the selected runner and price inside the frozen request JSON before sending, so a resend reuses them (`basal-host/src/selector.rs`). Each call opens its own Broca session (`basal-host/src/broca/`). Completion arrives as Broca's `run.ended` bus event, matched by `run_id`; a polling fallback bounded by the run's deadline covers a missed event. Broca reports usage, which basal prices at routing's metered or subscription-equivalent rates; unpriced selections refuse before sending. Direct model calls and Broca's `llm_classify` tool (tag `llm.classify/v1`) share a dollar budget: $0.50 per codemode run by default, and a manifest-declared cap for flows. A flow without a dollar cap still records the cost. Existing token caps remain additional limits.

## Codemode, end to end

`codemode({code, description?, limits?})` arrives on basal's tool route from an agent head or from a core-vouched worker (`basal-module/src/codemode.rs`, `basal-core/src/codemode/`).

1. **Admission.** basal records the run, refusing it beyond 2 concurrent runs per agent (or per worker scope) and 16 across basal. It then asks core to open a run scope (`codemode.run_scope.open`) and checks the opened scope's daemon stamp with `scope.describe` (`codemode/admission.rs`).
2. **Catalog.** For each provider that opts in (`call-modes/v1`, or the older `agent-run-scopes/v1`), basal reads `role.describe` and `tool.catalog` on the run's own scope. A provider it cannot read is dropped with a warning, never the whole run (`basal-host/src/run_scope.rs`).
3. **Execution.** One blocking supervisor per run drives a fresh worker (`codemode/supervisor.rs`). Each tool call goes out on the run scope with the caller's own bind identity, so every provider applies the agent's own allow, ask and deny rules. Limits: 10 s of JavaScript CPU, 30 minutes wall time, 200 tool calls, 64 KiB console output, and a 16 KiB return value.
4. **Waiting.** A call held for a person (`held-reply/v1`, reason `consent`) pauses the wall clock and spends a 10-minute person-wait allowance. A program still running after 25 s answers the agent with a held reply, and the result is delivered later through `late_results` (`codemode/held.rs`, `codemode/custody.rs`).

A codemode call is never replayed. A worker crash, deadline or cancel ends the run, and a call whose outcome is unproven reports `outcome_unknown`.

## The fleet around basal

| Peer | What basal uses it for |
|---|---|
| subc daemon | HELLO and route binding, the catalog, scope stamps and `scope.describe`, supervision. |
| ck-bus (NATS JetStream) | The `m_basal` consumer for module events, and publishing basal's own template lifecycle events. |
| prefrontal-core | Install cards and status, revocation hints, flow and run scopes, sinks, `board.read`, `work.unsettled`, agent lookup, owner notices. |
| Cingulate | Decision cards (`consent/v1`). |
| The routing module (the `routing/v1` provider) | Model choice (`route.select`). |
| Broca | Model calls, `run.ended` events, `llm_classify`. |
| Plexus, AFT, Cerebellum and other providers | Flow ops and codemode tools, each over the caller's scope. AFT's `file.read_exact` also reads script files for the `flow` tool. |
| The vault | Secret approval and injection for `net.fetch` (`basal-host/src/secrets.rs`). |

## Load-bearing rules

1. **The parent decides, the worker computes.** Never let worker output authorize a call, choose a scope, or write an outcome.
2. **Record before send.** A call's intent is synced before it is dispatched, and everything an outgoing effect depended on is durable before that effect leaves basal (`driver.rs`). A mutation whose outcome is unproven is never resent automatically.
3. **No network round trip under the SQLite write lock.** Resolve agents, scopes and catalogs first, then open the transaction. The run-store lock order (`run_store/lock_order.rs`) applies to every path.
4. **Caller identity comes from the daemon's stamp.** Never from arguments.
5. **The worker protocol is versioned.** A new frame or primitive bumps `PROTOCOL_VERSION` in `basal-proto/src/limits.rs`, and the deploy must stop basal while both binaries are replaced.
6. **Store migrations are one-way.** The older build refuses a newer schema, so a rollback needs the store backup taken before placement.
7. **Idle means asleep.** The engine sleeps until the next due time, and no idle pass opens a write transaction.
8. **Tool definitions are cached by heads.** A change to the `codemode` or `flow` tool's description or schema waits for a host restart window.
