# Flow install status v1

Replies of core's `flow.install_status` for a flow package installed from the
catalog, produced by the real handler (`ManagerRuntime::flow_op`) in
`crates/prefrontal-core-module/tests/it/catalog_flow_packs.rs`
(`install_status_states_from_the_real_handler_match_published_samples`).
Nothing here is hand-written.

`request.json` is the management request envelope: no `version`, so core
answers for the pack's pinned version, the newest one the person approved. A
newer version that was prefetched but still waits for its card is not pinned.
Reply files are the operation result, without the transport's `result`
envelope:

- `bound.json`: approved and fresh. `pack` names the install (`install_id`,
  catalog `name` and semver `version`, Basal's integer `flow_version`) and
  lists its ability bindings, empty until Basal's flow.json ability grammar
  and Cerebellum's ability ops are wired in.
- `held.json`: Entorhinal could not confirm the pin's status for 15 minutes.
  `state` is `held`, so a reader that starts runs only on `active` starts
  none; `catalog` carries the reason (`catalog_status_stale`) and
  Entorhinal's `stale_since_ms`. The hold is not terminal: the next confirmed
  read returns `state` to `active`.
- `revoked.json`: the pin was revoked, so core revoked that package version;
  `catalog` names the registry state and reason.
- `unknown.json`: a flow id with no pinned catalog version.

A request with `version` keeps today's reply for that version; for an approved
catalog version it adds the same `pack` and `catalog` members. Basal, Cerebellum
(read-only) and operators may call the op.

Fixture values: package `@example/hello-flow@1.0.0`, Basal id
`example--hello-flow`, flow version 3, and an install id fixed by the test
because core mints package install ids at random. Files are compact sorted-key
JSON plus one LF; `SHA256SUMS` lists their digests. Regenerate with
`PREFRONTAL_WRITE_VECTORS=1` on that test and review the diff.
