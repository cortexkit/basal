# Wrapper-level stack-budget diagnosis

A wrapper-escape regression placed a recursive function declaration and its call
at global scope:

```js
function f(n) { return n ? 1 + f(n - 1) : 0; } f(50);
```

That is not a portable recursion fixture. Lockdown freezes the global object
before evaluating the script. Both measured engines report the global object as
non-extensible and its `f` property as absent. QuickJS's `JS_CheckDefineGlobalVar`
therefore rejects the declaration with `cannot define variable 'f'`, before
`f(50)` can execute. The relevant condition in the pinned rquickjs-sys 0.14.0
QuickJS source is `!prs && !p->extensible`, where `prs` is the global object's own
property entry. A new global function must not bypass that restriction.

## Native isolation evidence

The diagnostic example runs the same request directly through `run_activation`
with an in-memory host link, without calling confinement. It can also send the
request through the actual worker and print its decoded `Welcome.confinement`.
The engine does not take `Confinement` as an input; that report belongs to the
handshake, not to global-object hardening or stack accounting.

```sh
cargo run -p basal-worker --example wrapper_stack -- direct
cargo run -p basal-worker --example wrapper_stack -- required
cargo run -p basal-worker --example wrapper_stack -- optional
```

On native Linux x86_64, direct engine entry, required mode and optional mode all
rejected the original declaration at both 32 KiB and 1 MiB. Both worker modes
reported seccomp enabled and Landlock ABI 8. Direct entry never ran the Linux
startup code, including random-state priming. Thus Landlock, seccomp and priming
are not the source of the declaration refusal.

On native macOS aarch64, direct entry and the Seatbelt worker both returned stack
exhaustion at 32 KiB, even for an empty, non-recursive script. At 1 MiB they rejected
the original declaration with the same error as Linux. Temporary phase logging
inside `Activation::setup` localized the 32 KiB failure to prelude/lockdown,
before wrapper evaluation. That logging was removed after measurement.

The old macOS regression passed for the wrong reason: its tiny budget prevented
setup from finishing, rather than proving that the recursive script was stopped.
QuickJS counts native stack bytes, whose frame costs depend on architecture and
compiler; equal byte limits need not permit the same setup prefix on every host.
This is not a different global lockdown policy on the two operating systems.

## Corrected regression

The stack case keeps its `BudgetExhausted(Stack)` expectation, but uses a named
function expression, whose name is local to that function rather than a new
property of the frozen global object:

```js
(function f(n) { return n ? 1 + f(n - 1) : 0; })(256);
```

The function expression and its recursive call remain wrapper-level work,
outside the final async function. The 128 KiB control calls the same function
with argument zero, proving prelude, compilation and one call fit without
recursion. The identical recursive work completes
under a 4 MiB control budget, then must exhaust the 128 KiB budget. These controls
separate actual recursive stack consumption from setup failure and declaration
refusal. A separate generous-budget regression preserves the refusal of the
original global declaration. An in-process unit test also verifies the frozen
global state and declaration refusal without OS confinement.

The Linux probe's `--engine-stack-fixture=<bytes>` runs only the fixed original
fixture, not arbitrary supplied JavaScript. It is accepted only after
`--confinement-probe`; `--no-sandbox` can isolate it from all confinement and
random-state priming. Allowed byte limits are 32768, 131072, 1048576 and 4194304.
This diagnostic does not create an unconfined engine launch mode.

## Follow-up validation

Native Linux x86_64 and macOS aarch64 both passed the corrected regression with
its non-recursive 128 KiB control and identical-program 4 MiB control. The Linux
plain probe was also run unconfined, required and optional at 32 KiB and 1 MiB:
all six children reached readiness and reported the same global-declaration
refusal. Their successful exits are probe results, not successful activations.

Replacing `rt.set_max_stack_size(request.budgets.stack_bytes as usize)` with a
fixed 4 MiB limit made `top_level_wrapper_code_has_activation_budgets` fail with
`Completed(1)` instead of `BudgetExhausted(Stack)` on both native macOS and Linux.
The other four sandbox-integrity tests passed on each host. The temporary mutant
was restored; the engine's requested-budget enforcement is unchanged.
