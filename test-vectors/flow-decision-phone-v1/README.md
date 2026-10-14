# Flow decision phone fixture vectors

These three JSON files contain the exact `consent.request` parameters for basal's three operator decisions:

- **Reconcile:** a call may have reached its destination, but basal did not receive a reply. The operator decides whether to retry, accept, cancel, or leave the call unresolved.
- **Re-enable:** basal limits how many host calls each flow can dispatch in a 60-second window. When a flow exceeds that limit for the configured number of consecutive windows, basal disables it and asks whether to turn it back on. This fixture uses a one-call limit and disables after one saturated window.
- **Grant loss:** a provider reports that a flow's grant is missing, so basal disables the flow and asks whether to check the existing grant again.

Each file is built by running the corresponding production path with a fixed clock and store identity, then calling `DecisionRecord::to_card` and `decision_request`. The title, prompt, options, default, deduplication key, argument digest (when present), and expiry are basal's generated values, not hand-written examples.

In each request, `flow_decision` is the structured payload describing the decision. `facts` is a separate optional list of display details. The prefrontal-core service derives that list from `flow_decision`, so basal omits `facts` instead of sending a duplicate that could disagree with the decision payload.

Regenerate the JSON and checksums from the repository root with:

```sh
BASAL_WRITE_VECTORS=1 cargo test --locked -p basal-module --test decisions -- the_phone_fixture_vectors_are_generated_by_the_engine_card_builders -- --exact
```

Without `BASAL_WRITE_VECTORS=1`, the test `the_phone_fixture_vectors_are_generated_by_the_engine_card_builders` rebuilds the requests through `DecisionRecord::to_card` and `decision_request`, compares their bytes with the committed JSON, and verifies `SHA256SUMS`.
