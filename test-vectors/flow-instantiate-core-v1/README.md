# Core template instantiation v1

These samples show basal's `flow.instantiate` and `flow.instance.end`
management operations as called by the daemon-attested `reserved:prefrontal`
principal. Each file contains `name`, `request` (`method` and `params`), and
either the management `reply` or the error body (`code`, `message` and, when
present, `detail`). The principal is on the route, not in the request body.

Basal's real handlers generate the samples against a fresh test store. The
fixture registers version 1 of `templates/docs-proposals/` using the exact
checked-in manifest and script. Core selects an approved version before making
these calls; basal registration does not itself approve the package. The
fixture uses synthetic agent IDs and starts at `2026-05-01T00:00:00Z`.

- `success`: creates an instance for the named agent and project.
- `repeat_extends`: sends the identical request one day later; the ID is
  unchanged and the expiry advances by one day.
- `param_missing`: leaves `project_id` out of `params`.
- `lifetime_too_long`: requests a lifetime longer than the template allows.
- `instance_limit_reached`: after filling the named agent's 100 live slots for
  this template, requests an instance for a different project.
- `agent_unknown`: names an agent that the registry does not know.
- `agent_moving`: names a known agent frozen at the start of a move.
- `end_success`: ends the instance created in `success` and renewed in
  `repeat_extends`.

The test compares the generated files byte for byte. `SHA256SUMS` lists this
README and all eight JSON files, not itself. To regenerate on Linux:

```sh
BASAL_WRITE_VECTORS=1 cargo test --locked -p basal-module --test flow_instantiate_core_vectors flow_instantiate_core_vectors_are_generated_and_pinned -- --exact
```
