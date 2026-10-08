//! Opt-in production-process measurement against the e2e wire daemon.
#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;
use subc_protocol::{Flags, Frame, FrameType, PROTOCOL_VERSION, Priority};
use subc_transport::connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION, write_atomic};
use subc_transport::{authenticate_server, read_frame, write_frame};

struct ChildGuard(Child);
impl Drop for ChildGuard {
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

fn cpu(pid: u32) -> String {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "time="])
        .output()
        .unwrap();
    assert!(output.status.success(), "ps failed");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

async fn sample(source: &Path, label: &str) -> Value {
    let dir = Directory(
        std::env::temp_dir().join(format!("basal-idle-cpu-{}-{label}", std::process::id())),
    );
    std::fs::create_dir_all(&dir.0).unwrap();
    let worker = basal_testkit::channel::worker_binary();
    let placed = basal_testkit::DevBinaries::new(&[source, &worker]).unwrap();
    let binary = placed.path(source).unwrap();
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let file = dir.0.join("connection.json");
    let key = [0x5a; 32];
    let daemon_id = [0x2b; 16];
    write_atomic(
        &file,
        &ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: key.to_vec(),
            daemon_id,
            pid: std::process::id(),
            daemon_ver: "idle-cpu-test".into(),
        },
    )
    .unwrap();
    let child = ChildGuard(
        Command::new(binary)
            .args(["--subc".as_ref(), file.as_os_str()])
            .env_clear()
            .env("SUBC_MODULE_ID", "basal")
            .env("SUBC_CONNECTION_FILE", &file)
            .env("SUBC_LAUNCH_NONCE", "idle-cpu-test")
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
        &key,
        &daemon_id,
        "idle-cpu-test",
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    let answers = Arc::new(AtomicU64::new(0));
    let answered = answers.clone();
    let provider = tokio::spawn(async move {
        while let Ok(Some(frame)) = read_frame(&mut consumer).await {
            if frame.header.ty != FrameType::Request {
                continue;
            }
            let body: Value = serde_json::from_slice(&frame.body).unwrap();
            let reply = if body["op"] == "route.open" {
                json!({"op":"route.open","route_channel":11,"route_epoch":1})
            } else if body["method"] == "elicitation.answers" {
                answered.fetch_add(1, Ordering::Relaxed);
                json!({"result":{"records":[],"cursor":0}})
            } else if body["method"] == "catalog" {
                json!({"result":{"modules":[]}})
            } else {
                json!({"result":{"ok":true}})
            };
            let response = Frame::build(
                FrameType::Response,
                Flags::new(false, Priority::Passive, false),
                frame.header.channel,
                frame.header.epoch,
                frame.header.corr,
                serde_json::to_vec(&reply).unwrap(),
            )
            .unwrap();
            if write_frame(&mut consumer, &response).await.is_err() {
                break;
            }
        }
    });
    let (mut stream, _) = listener.accept().await.unwrap();
    authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        "idle-cpu-test",
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
    let ack = Frame::build(FrameType::HelloAck, Flags::new(false, Priority::Passive, false), 0, 0, hello.header.corr,
        serde_json::to_vec(&json!({"negotiated_ver":PROTOCOL_VERSION,"subc_ops":[],"subc_capabilities":[],"storage":storage,"machine_id":null})).unwrap()).unwrap();
    write_frame(&mut stream, &ack).await.unwrap();
    let module_wire =
        tokio::spawn(async move { while let Ok(Some(_)) = read_frame(&mut stream).await {} });
    // Exclude process startup and worker warm-up from the sample.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let start = cpu(child.0.id());
    let start_answers = answers.load(Ordering::Relaxed);
    tokio::time::sleep(Duration::from_secs(120)).await;
    let end = cpu(child.0.id());
    let result = json!({"label":label,"sample_seconds":120,"ps_time_start":start,"ps_time_end":end,
        "consent_answer_rpcs":answers.load(Ordering::Relaxed)-start_answers});
    println!("IDLE_CPU {result}");
    drop(child);
    provider.abort();
    module_wire.abort();
    result
}

#[tokio::test]
#[ignore = "two two-minute production CPU samples; requires BASAL_IDLE_BEFORE_BIN"]
async fn production_idle_cpu_before_and_after() {
    let before =
        PathBuf::from(std::env::var_os("BASAL_IDLE_BEFORE_BIN").expect("baseline ck-basal path"));
    let after = PathBuf::from(env!("CARGO_BIN_EXE_ck-basal"));
    let samples = tokio::time::timeout(Duration::from_secs(600), async {
        let before = sample(&before, "before").await;
        let after = sample(&after, "after").await;
        json!({"samples":[before,after],"method":"ps -p PID -o time=; production ck-basal, wire-shaped empty core, zero flows"})
    }).await.expect("CPU measurement hang bound");
    if let Some(path) = std::env::var_os("BASAL_IDLE_CPU_REPORT") {
        std::fs::write(path, serde_json::to_string_pretty(&samples).unwrap() + "\n").unwrap();
    }
}
