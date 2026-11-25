# llm-extract-1125: llm_extract tool replies (llm-tools 0.4.0)

Real reply bodies of broca's `llm_extract` tool, written through the model-tools route by its
route test (worker commit 8781a971, on broca train-0140 + llm-tools 0.4.0), against a local
Clairic stand-in. The answer values are invented by the stand-in; the shapes are broca's.

- `extract-success.ok.json`: a tool result (`result_format` tool-result/v1): `content` text plus
  `structuredContent` `{answers, truncated, state_text, usage, cost_usd}`. Answer numbers keep the
  provider's spelling (`1.2300e+2`) and unknown members (`new_member`).
- `extract-replay.ok.json`: the same call sent again under the same `call_key`; byte-identical to
  the success.
- `extract-paid.refused.json`: a refusal after a paid provider call (`answer_too_large`), carrying
  `detail.cost_usd`.
- `extract-ceiling.refused.json`: a `max_cost_usd` refusal before any provider call
  (`cost_exceeded`), carrying `detail.no_spend: true`.
