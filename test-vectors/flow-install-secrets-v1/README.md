# Flow install secrets v1

`install-request.json` is captured from basal's real `flow.install` request path,
through `CoreConsent`, by `crates/basal-module/tests/flow_secrets.rs`. The fixture
pins the persisted install id before retrying a capability-refused install; it
does not rewrite the captured request.

The statement is illustrative canonical JSON, encoded as unpadded base64url,
**not vault-signed bytes**. Its flow identity matches the request's flow id,
version, exact code hash and install id. All names, ids and timestamps are
synthetic, and the sample contains no secret value.

The request carries `flow_install.secrets: [{request_id, statement_b64url}]`
beside the existing install identity. Core constructs the card identity from
its own records and must reject mismatched statement bindings.

Verify with `sha256sum -c SHA256SUMS`. To regenerate intentionally, run
`BASAL_WRITE_VECTORS=1 cargo test -p basal-module --test flow_secrets -- --nocapture`
on Linux. A normal test run compares both the request and checksums to these
committed artifacts.
