//! The supervised binary must leave on socket EOF, not wait for a signal.
#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
use subc_transport::{authenticate_server, read_frame, write_frame};

#[path = "common/binary_staging.rs"]
mod binary_staging;

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn frame(kind: FrameType, corr: u64, body: serde_json::Value) -> Frame {
    Frame::build(
        kind,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[test]
fn supervised_binary_exits_cleanly_after_daemon_eof() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(300), async {
            let dir = Directory(basal_testkit::harness::scratch("daemon-eof"));
            let binary = basal_testkit::dev_binary(env!("CARGO_BIN_EXE_ck-basal"));
            let worker = basal_testkit::channel::worker_binary();
            let (binary, _) = binary_staging::stage_module_worker(&binary, &worker);
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let file = dir.0.join("connection.json");
            let info = ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: None,
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: vec![0x5a; 32],
                daemon_id: [0x2b; 16],
                pid: std::process::id(),
                daemon_ver: "eof-test".into(),
            };
            write_atomic(&file, &info).unwrap();
            let mut module = Process(
                Command::new(binary)
                    .args(["--subc".as_ref(), file.as_os_str()])
                    .env_clear()
                    .env("SUBC_MODULE_ID", "basal")
                    .env("SUBC_CONNECTION_FILE", &file)
                    .env("SUBC_LAUNCH_NONCE", "eof-test-nonce")
                    .env("XDG_DATA_HOME", &dir.0)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let (mut consumer, _) = listener.accept().await.unwrap();
            authenticate_server(
                &mut consumer,
                &info.key,
                &info.daemon_id,
                "eof-test",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            authenticate_server(
                &mut stream,
                &info.key,
                &info.daemon_id,
                "eof-test",
                Duration::from_secs(30),
            )
            .await
            .unwrap();
            let hello = read_frame(&mut stream).await.unwrap().unwrap();
            assert_eq!(hello.header.ty, FrameType::Hello);
            let storage = cortexkit_store_types::StorageDescriptor {
                module_id: "basal".into(),
                storage_namespace: "core".into(),
                isolation: cortexkit_store_types::Isolation::Module,
                backend: cortexkit_store_types::StorageBackend::Sqlite {
                    path: dir.0.join("store.db").to_string_lossy().into_owned(),
                },
            };
            write_frame(
                &mut stream,
                &frame(
                    FrameType::HelloAck,
                    hello.header.corr,
                    json!({
                        "negotiated_ver": subc_protocol::PROTOCOL_VERSION,
                        "subc_ops": [], "subc_capabilities": [],
                        "storage": storage
                    }),
                ),
            )
            .await
            .unwrap();
            loop {
                write_frame(
                    &mut stream,
                    &frame(FrameType::Request, 100, json!({"op":"health.check"})),
                )
                .await
                .unwrap();
                let reply = read_frame(&mut stream).await.unwrap().unwrap();
                let health: serde_json::Value = serde_json::from_slice(&reply.body).unwrap();
                if !health["metrics"].is_null() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // The idle engine probes core capabilities through catalog.list.
            // Leave that RPC unanswered so EOF must release a genuine join
            // dependency, rather than racing an engine that has not started yet.
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let request = read_frame(&mut consumer).await.unwrap().unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                    if body["op"] == "catalog.list" {
                        break;
                    }
                }
            })
            .await
            .expect("engine did not reach its catalog probe");
            let eof = Instant::now();
            drop((stream, consumer, listener));
            loop {
                if let Some(status) = module.0.try_wait().unwrap() {
                    let elapsed = eof.elapsed();
                    eprintln!("daemon EOF to ck-basal exit: {elapsed:?}; {status}");
                    assert!(status.success(), "EOF must be a clean exit: {status}");
                    break;
                }
                assert!(
                    eof.elapsed() < Duration::from_secs(5),
                    "ck-basal still alive {:?} after daemon EOF",
                    eof.elapsed()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("fake-daemon startup stalled");
    });
}

const TRANSACTION_CHILD: &str = "BASAL_EOF_TRANSACTION_CHILD";

#[test]
fn daemon_eof_waits_for_a_store_transaction_to_commit() {
    let dir = Directory(basal_testkit::harness::scratch("eof-transaction"));
    let mut child = Process(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "startup_transaction_child", "--nocapture"])
            .env(TRANSACTION_CHILD, &dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let startup = Instant::now();
    while !dir.0.join("eof").exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "serve child exited before EOF"
        );
        assert!(
            startup.elapsed() < Duration::from_secs(60),
            "transaction did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The transaction has executed its INSERT but has not returned to COMMIT.
    // Give an incorrect early exit time to become observable before releasing it.
    std::thread::sleep(Duration::from_millis(250));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "EOF cut an in-progress write"
    );
    std::fs::write(dir.0.join("release"), "commit").unwrap();
    let released = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            eprintln!(
                "transaction release to serve-process exit: {:?}; {status}",
                released.elapsed()
            );
            assert!(status.success());
            break;
        }
        assert!(
            released.elapsed() < Duration::from_secs(5),
            "serve child did not drain"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let store =
        basal_core::Store::open(dir.0.join("store.db"), basal_core::Durability::throwaway())
            .expect("shutdown released the writer lease");
    let value: i64 = store
        .read(|conn| Ok(conn.query_row("SELECT value FROM eof_commit", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(
        value, 73,
        "the transaction committed before the process exited"
    );
}

/// Invoked in a subprocess so returning from serve really can cut a detached
/// startup writer. A future-only test would keep that writer's process alive.
#[test]
fn startup_transaction_child() {
    let Some(root) = std::env::var_os(TRANSACTION_CHILD).map(PathBuf::from) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION, wire_version: None,
            endpoints: vec![Endpoint { host:"127.0.0.1".into(), port:listener.local_addr().unwrap().port() }],
            key:vec![8;32], daemon_id:[9;16], pid:std::process::id(), daemon_ver:"transaction-eof-test".into(),
        };
        let file = root.join("connection.json");
        write_atomic(&file, &info).unwrap();
        let initialized_root = root.clone();
        let handler = basal_module::serve::BasalHandler::new(
            Box::new(|store_path| {
                let mut pool = basal_module::pool::PoolConfig::new("unused", basal_module::process::WorkerLaunch::Plain);
                pool.warm_spares = 0;
                basal_module::module::ModuleConfig {
                    dry_run:basal_module::dryrun::DryRunConfig::new(store_path.parent().unwrap().join("dry-run")),
                    store_path, pool, durability:basal_core::Durability::throwaway(),
                    runtime:Default::default(), engine:Default::default(),
                }
            }),
            Box::new(|| basal_module::module::Hosts {
                transport:std::sync::Arc::new(basal_module::unconfigured::UnconfiguredTransport),
                host:std::sync::Arc::new(basal_host::mock::MockHost::new()),
                catalog:std::sync::Arc::new(basal_host::MockCatalog::standard()),
                consent:std::sync::Arc::new(basal_host::MockConsent::new()),
                hooks:std::sync::Arc::new(basal_core::NoHooks),
            }),
        ).with_store_initializer(Box::new(move |store| {
            store.write(|tx| {
                tx.execute_batch("CREATE TABLE eof_commit(value INTEGER NOT NULL); INSERT INTO eof_commit VALUES(73);")?;
                std::fs::write(initialized_root.join("entered"), "uncommitted").unwrap();
                let deadline = Instant::now() + Duration::from_secs(60);
                while !initialized_root.join("release").exists() {
                    assert!(Instant::now() < deadline, "parent did not release transaction");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(())
            }).map_err(|error| error.to_string())
        }));
        let daemon = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            authenticate_server(&mut stream, &info.key, &info.daemon_id, "transaction-eof-test", Duration::from_secs(5)).await.unwrap();
            let hello = read_frame(&mut stream).await.unwrap().unwrap();
            let storage = cortexkit_store_types::StorageDescriptor {
                module_id:"basal".into(), storage_namespace:"core".into(),
                isolation:cortexkit_store_types::Isolation::Module,
                backend:cortexkit_store_types::StorageBackend::Sqlite { path:root.join("store.db").to_string_lossy().into_owned() },
            };
            write_frame(&mut stream, &frame(FrameType::HelloAck, hello.header.corr, json!({
                "negotiated_ver":subc_protocol::PROTOCOL_VERSION, "subc_ops":[], "subc_capabilities":[], "storage":storage
            }))).await.unwrap();
            while !root.join("entered").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            drop(stream);
            std::fs::write(root.join("eof"), "daemon closed").unwrap();
        });
        let (_, serving) = subc_client_rs::serve_with_handle(&file, basal_module::manifest::manifest(), handler).await.unwrap();
        serving.await.unwrap();
        daemon.await.unwrap();
    });
}
