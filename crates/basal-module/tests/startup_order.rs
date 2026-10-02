//! Guard the production startup boundary without requiring a live daemon.

#[test]
fn serve_connects_before_constructing_worker_pool() {
    let source = include_str!("../src/main.rs");
    let serve = source
        .split("fn serve() -> ExitCode {")
        .nth(1)
        .expect("serve");
    let connect = serve
        .find("basal_host::transport::SubcTransport::connect")
        .expect("startup connect");
    let pool = serve
        .find("PoolConfig::beside_current_exe()")
        .expect("pool config");
    let handler = serve
        .find("let handler = BasalHandler::new(")
        .expect("handler");
    assert!(
        connect < pool && pool < handler,
        "connect must consume the nonce before pool startup"
    );
    let before_connect = &serve[..connect];
    for spawn in [
        "Module::start",
        "WorkerPool::",
        "ProcessSpawner::",
        ".spawn(",
        "run(handler)",
    ] {
        assert!(
            !before_connect.contains(spawn),
            "worker spawn before connect: {spawn}"
        );
    }
    assert!(
        serve[connect..pool].contains("return ExitCode::FAILURE;"),
        "failed connect must refuse startup"
    );
}
