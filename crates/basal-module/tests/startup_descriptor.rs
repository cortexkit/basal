use basal_module::serve::BasalHandler;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use subc_client_rs::ModuleHandler;

#[test]
fn invalid_store_descriptor_exits_for_supervised_restart() {
    const CHILD: &str = "BASAL_DESCRIPTOR_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let handler = BasalHandler::new(
            Box::new(|_| panic!("invalid descriptor must not configure the module")),
            Box::new(|| panic!("invalid descriptor must not construct hosts")),
        );
        let ack = serde_json::from_value(serde_json::json!({
            "negotiated_ver": subc_protocol::PROTOCOL_VERSION,
            "subc_ops": [], "subc_capabilities": [], "storage": 7, "machine_id": null,
        }))
        .unwrap();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(handler.on_hello_ack(&ack));
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "invalid_store_descriptor_exits_for_supervised_restart",
        ])
        .env(CHILD, "1")
        // Only the parent's test is a libtest case. The child exits inside
        // HELLO_ACK and cannot produce its own complete test summary.
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("invalid descriptor child did not terminate");
        }
        std::thread::yield_now();
    };
    assert_eq!(status.code(), Some(basal_module::fatal::EXIT_STORE_FAILURE));
}
