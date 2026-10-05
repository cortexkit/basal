# Live flow-scope admission evidence

The isolated ckdev-flows rig ran the production stack revisions below, with
basal's rig-only hooks enabled. No production daemon or credential was changed.
The suite passed **268 checks in 21 cases**, with zero failures and zero cases
not run. All **38 new named checks** are reproduced below, grouped as stored in
`contract.json` (the tools case checks all three sending flows).

## Stack and commands

Basal code built: `04d0dd174d8d896903b2aad8f7e0856d815e8f5f`, including the merge of
main `540aee0a5cd70b3997f08e2c108692bdebad1661`. The subsequent evidence commit
changes documentation only.

```sh
script/flows-rig.sh build --prefrontal-rev cba528a11 --broca-rev 603c06cb --entorhinal-rev f2a4ba19 --credentials-rev 336876ee --fusiform-rev 8a0448aa --subc-rev 1a14993c --sibling-lock claustrum
script/flows-rig.sh place
script/flows-rig.sh config
script/flows-rig.sh start
script/flows-rig.sh test --models
script/flows-rig.sh stop
```

All commands exited zero; the rig was stopped after the suite. Commons resolved
to `a8f14b08b667115df7571b7e449dcc20de86e283`. The mutation runner, independently,
was `cortexkit-mutate 0.3.0` installed from commons
`0b1004452fb2b673fdb2bc010a850b430e3e945f`.

Full live artifacts on the test machine:
`~/.local/share/cortexkit/ckdev-flows/results/20261005T122030Z/{stack.json,contract.json,contract.log}`.
These hold the registration, ownership, bind and refusal details read from core's and Broca's own stores and from the daemon's replies, not from basal's account of them.
The direct probe received `scope_not_carrier` for core's selector
`44d719ae26ada904da08f278ec7408de`, epoch 1. Its bind had no meta row, run-index
entry or WAL record. The unscoped basal send received `flow_scope_required`; its
per-call session `basal:flow-rig-model-unscoped-0c023671:run-33a365d4d14b5aa1-85:0`
had no run-index entry or WAL record (including no `RunStarted`).

The carrier probe proves that a non-carrier is refused, **not** that the carrier
list is exactly `[reserved:basal]`. Exactness rests on prefrontal's own tests at
`cba528a11`, in `crates/prefrontal-core-module/src/scope_owner.rs`:

- `flow_scope_registration_is_basal_only`
- `registered_flow_scope_wire_vector_pins_attributes_and_the_entire_carrier_list`
- `registered_global_flow_scope_wire_vector_pins_no_agent_and_the_entire_carrier_list`

Staged production bytes were not exercised in this run. The suite records the
unscoped-send and crash-hook cases as not run by name on staged bytes, rather
than counting them as passes. The additional negative flow raises the finite
daily suite allowance from 3,088 to 4,112 tokens, covering a broken refusal gate
as well as normal execution.

## Every new named PASS line

```text
== models: first minimal Luna call
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: attested first principal is reserved:basal
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca's scope stamp carries this flow_id
== models: routing selection journaled before dispatch
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: attested first principal is reserved:basal
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca's scope stamp carries this flow_id
== models: a direct non-carrier cannot open the registered flow scope
  PASS the direct probe bind is fresh in Broca before opening
  PASS only an attested carrier can open the flow's scope; a direct client is refused
  PASS the refused non-carrier open reaches no Broca session: no meta row, run_index entry or WAL record
== models: run.result text and run.status usage settlement
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: attested first principal is reserved:basal
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca's scope stamp carries this flow_id
== models: classify returns a label
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: attested first principal is reserved:basal
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: Broca's scope stamp carries this flow_id
== models: crash recovers Broca result without a second run
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: attested first principal is reserved:basal
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: Broca's scope stamp carries this flow_id
== models: flow calls carry no tools
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: attested first principal is reserved:basal
  PASS rig-model-first-0c023671 run-33a365d4d14b5aa1-81#0: Broca's scope stamp carries this flow_id
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: attested first principal is reserved:basal
  PASS rig-model-classify-0c023671 run-33a365d4d14b5aa1-82#0: Broca's scope stamp carries this flow_id
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: Broca evidence belongs to this call's per-call session
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: scope owner is reserved:prefrontal-core and ref/epoch match core's registration
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: attested first principal is reserved:basal
  PASS rig-model-crash-0c023671 run-33a365d4d14b5aa1-84#0: Broca's scope stamp carries this flow_id
== models: Broca refuses an unscoped basal send without starting a run
  PASS the named flow consumed the one-shot unscoped-send arm
  PASS the running Broca refuses basal's unscoped send with flow_scope_required
  PASS the refused unscoped send writes no RunStarted or run_index entry for its per-call session
```

## Offline gates

- Rust/cargo 1.99.0; rustfmt 1.10.0-stable: `cargo fmt --all --check` passed.
- Both workspace `cargo clippy --workspace --all-targets -- -D warnings` runs
  passed, one with `--features basal-module/rig-kill-hook`.
- `cargo test --workspace --locked`: 404 tests passed across 74 targets.
- `cargo test -p basal-host --lib --features rig-kill-hook --locked`: 24 tests
  passed, including actual scoped/unscoped transport wiring.
- `ck-mutate check`: 432 catalogue rows validated after merging main.
- ShellCheck 0.11.0: `shellcheck -x script/*.sh` passed on four scripts.
- Python 3.9.6: `python3 -m unittest script.tests.flows_rig script.tests.stage_card`
  passed all 18 tests.

Five new cargo mutation controls were proved one at a time by the pinned runner:
separate ref and epoch mismatches, successful non-carrier open, unrefused send,
and unarmed-flow hook firing. Under each mutation, only its named test failed,
and every other test in its target passed. Reports and live/restore diff-stat evidence are under
the worktree's gitignored `target/mutations/`. The existing worker dependency-fence
control also remained caught after updating its lockfile anchor for rig's sha2
dependency. No mutant remained in the committed sources.
