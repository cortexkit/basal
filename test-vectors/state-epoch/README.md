# State-epoch vectors

`vectors.json` pins exact manifest JSON text alongside the outcomes basal produces through `flow.install`, `package.register`, and `flow.dry_run`. The manifest strings are passed to basal unchanged because their bytes are part of the approval hash. The `basal-module` integration test recreates each install, records the reset preview or refusal returned by basal, and compares the complete generated dataset with this file.

Template instance labels in `instance_state_epochs` map to the runtime's instance records; `reset_instances` names the instances whose recorded epoch differs from the new template epoch. Refusal entries preserve basal's exact error code and message.
