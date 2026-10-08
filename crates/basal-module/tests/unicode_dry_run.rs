mod common;

use basal_module::caller::Caller;
use common::{Options, fixture, install, schedule_manifest};
use serde_json::json;

#[test]
fn dry_run_multibyte_windows_are_typed_refusals() {
    let f = fixture(
        "unicode-window",
        Options {
            warm_spares: 0,
            ..Options::default()
        },
    );
    install(
        &f,
        &Caller::Operator,
        "return 1",
        &schedule_manifest("unicode-window", json!({"interval":"1h"})),
    );
    for window in ["é", "10é", "10中", "10🦀", "é1s", "١s", "1sé"] {
        let reply = f.module.handle(
            &Caller::Operator,
            "flow.dry_run",
            json!({"flow_id":"unicode-window", "window":window}),
        );
        assert_eq!(reply.unwrap_err().code, "invalid_params", "{window:?}");
    }
}
