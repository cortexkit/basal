//! Guard the production startup boundary without requiring a live daemon.

#[test]
fn serve_reads_launch_inputs_before_constructing_worker_pool() {
    let source = include_str!("../src/main.rs");
    let serve = source
        .split("fn serve() -> ExitCode {")
        .nth(1)
        .expect("serve")
        .split("\nfn production_config(")
        .next()
        .expect("serve body");
    let nonce = serve
        .find("subc_client_rs::launch_nonce()")
        .expect("explicit launch-nonce read");
    let channel = serve
        .find("subc_os::registration_channel::supervised()")
        .expect("registration-channel decision");
    assert_eq!(
        serve.matches("SubcTransport::deferred(").count(),
        1,
        "startup must build only the connection used by the hosts"
    );
    let transport = serve
        .find("basal_host::transport::SubcTransport::deferred")
        .expect("startup transport");
    let pool = serve
        .find("PoolConfig::beside_current_exe()")
        .expect("pool config");
    let handler = serve
        .find("let handler = BasalHandler::new(")
        .expect("handler");
    assert!(
        nonce < pool && channel < pool && pool < handler,
        "launch inputs must be read before pool startup"
    );
    assert!(
        serve[nonce..channel].contains("return ExitCode::FAILURE;"),
        "an unreadable launch nonce must refuse startup"
    );
    assert!(
        serve[handler..].contains("let transport = transport.clone();"),
        "the hosts must reuse the one transport built before pool startup"
    );
    let before_inputs = &serve[..nonce.min(channel)];
    for spawn in [
        "Module::start",
        "WorkerPool::",
        "ProcessSpawner::",
        ".spawn(",
        "privacy_identity::probe",
        "run(handler",
    ] {
        assert!(
            !before_inputs.contains(spawn),
            "child spawn before launch inputs are read: {spawn}"
        );
    }
    #[cfg(target_os = "macos")]
    assert!(
        channel < serve.find("privacy_identity::probe").expect("probe"),
        "the privacy probe spawns a child"
    );
    assert!(transport < handler);
}

#[test]
fn run_connects_the_hosts_only_after_registration_with_serving_running() {
    let source = include_str!("../src/main.rs");
    let run = source
        .split("async fn run(")
        .nth(1)
        .expect("run")
        .split("\nmod ")
        .next()
        .expect("run body");
    let registered = run
        .find("serve_from_env_with_handle(")
        .expect("registration");
    let serving = run.find("tokio::spawn(serving)").expect("serving spawned");
    let attach = run.find("transport.attach()").expect("hosts connect");
    assert!(
        registered < serving && serving < attach,
        "the consumer is bound through the registered connection, which must be served first"
    );
    assert!(
        run[attach..].contains("std::process::exit(1)"),
        "a failed connect must stop the module"
    );
}
