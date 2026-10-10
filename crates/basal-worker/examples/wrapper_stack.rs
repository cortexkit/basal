//! Compare a new global function declaration with locally scoped recursion after
//! the engine freezes its global object, both directly and through the worker.
//! Frozen globals reject new names before a call can run; scoped functions can
//! recurse. Direct versus worker entry separates those engine costs from OS confinement.

use basal_proto::{ActivationRequest, ActivationResult, Budgets, JsonText, Profile};
use basal_worker::{engine, link::Channel};
use std::cell::RefCell;
use std::rc::Rc;

fn request(work: &str, stack: u64) -> ActivationRequest {
    ActivationRequest {
        activation_id: 1,
        profile: Profile::Flow,
        prelude_hash: engine::prelude_hash(),
        tools: vec![],
        script: format!("return 1\n}}); {work} (async function () {{ return 1;"),
        trigger: JsonText::null(),
        self_input: JsonText::null(),
        budgets: Budgets {
            stack_bytes: stack,
            ..Default::default()
        },
        prefix: vec![],
    }
}
fn direct(request: &ActivationRequest) -> ActivationResult {
    let link = Rc::new(RefCell::new(Channel::new(
        std::io::Cursor::new(Vec::<u8>::new()),
        Vec::<u8>::new(),
    )));
    engine::run_activation(request, link)
}
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "direct".into());
    println!(
        "os={} arch={} mode={mode}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let mut worker = if mode == "required" || mode == "optional" {
        #[cfg(target_os = "linux")]
        let args = if mode == "required" {
            vec!["--landlock=required"]
        } else {
            vec!["--landlock=optional"]
        };
        #[cfg(not(target_os = "linux"))]
        let args = Vec::new();
        let mut worker =
            basal_testkit::WorkerProcess::spawn_with_args(&basal_testkit::worker_binary(), &args)
                .unwrap();
        let welcome = worker
            .handshake(std::time::Duration::from_secs(10))
            .unwrap();
        println!("worker confinement={:?}", welcome.confinement);
        Some(worker)
    } else {
        assert_eq!(mode, "direct");
        None
    };
    for (name, work, stack) in [
        ("trivial-small", "", 32 * 1024),
        ("trivial-medium", "", 128 * 1024),
        ("global-only-small", "function f() {}", 32 * 1024),
        (
            "original-small",
            "function f(n) { return n ? 1 + f(n - 1) : 0; } f(50);",
            32 * 1024,
        ),
        (
            "original-large",
            "function f(n) { return n ? 1 + f(n - 1) : 0; } f(50);",
            1024 * 1024,
        ),
        (
            "local-recursion-medium",
            "(function f(n) { return n ? 1 + f(n - 1) : 0; })(256);",
            128 * 1024,
        ),
        (
            "local-recursion-large",
            "(function f(n) { return n ? 1 + f(n - 1) : 0; })(256);",
            4 * 1024 * 1024,
        ),
        (
            "local-recursion-small",
            "(function f(n) { return n ? 1 + f(n - 1) : 0; })(50);",
            32 * 1024,
        ),
    ] {
        let request = request(work, stack);
        let result = if let Some(worker) = &mut worker {
            worker
                .send(&basal_proto::ParentMessage::Activate(Box::new(request)))
                .unwrap();
            match worker.recv(std::time::Duration::from_secs(10)).unwrap() {
                basal_proto::WorkerMessage::Finished { result, .. } => result,
                other => panic!("unexpected message: {other:?}"),
            }
        } else {
            direct(&request)
        };
        println!("{name} stack={stack}: {result:?}");
    }
    let mut info = request("", 1024 * 1024);
    info.script = "return [Object.isExtensible(globalThis), Object.getOwnPropertyDescriptor(globalThis, 'f') === undefined];".into();
    println!("unconfined global-state: {:?}", direct(&info));
    if let Some(mut worker) = worker {
        worker.send(&basal_proto::ParentMessage::Shutdown).unwrap();
        worker
            .wait_exit(std::time::Duration::from_secs(10))
            .unwrap();
    }
}
