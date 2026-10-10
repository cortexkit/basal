use super::*;

/// The rig PATH excludes CortexKit directories; never discover a fleet CLI there.
pub(super) fn module_cli_path(home: &std::path::Path) -> Result<PathBuf, String> {
    let root = home.parent().ok_or("HOME has no rig parent")?;
    if !home.is_absolute()
        || root.file_name().and_then(|s| s.to_str()) != Some("ckdev-flows")
        || home != root.join("home")
    {
        return Err("module control requires the isolated ckdev-flows home".into());
    }
    Ok(root.join("bin/ckdev-ck"))
}

pub(super) fn ck(args: &[&str]) -> Result<Value, String> {
    let connection =
        std::env::var_os("SUBC_CONNECTION_FILE").ok_or("no isolated connection file")?;
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("missing isolated HOME")?);
    let output = std::process::Command::new(module_cli_path(&home)?)
        .arg("--subc")
        .arg(connection)
        .arg("--json")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "ckdev-ck {args:?}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&output.stdout)))
}

pub(super) fn fixture_path_for(
    home: &std::path::Path,
    path: &std::path::Path,
    module: &str,
) -> Result<PathBuf, String> {
    let root = home.parent().ok_or("HOME has no parent")?;
    if root.file_name().and_then(|s| s.to_str()) != Some("ckdev-flows") || home != root.join("home")
    {
        return Err("fixture writes require the ckdev-flows isolated home".into());
    }
    if !["basal", "prefrontal-core"].contains(&module) {
        return Err("unsupported fixture module".into());
    }
    let expected = root.join(format!("data/cortexkit/{module}/store.db"));
    if path != expected || expected.canonicalize().map_err(|e| e.to_string())? != expected {
        return Err("fixture store is not the rig's own unsymlinked basal store".into());
    }
    Ok(expected)
}

pub(super) fn fully_stopped(status: &Value, pid: Option<String>) -> bool {
    status["module"]["state"] == "disabled"
        && status["module"]["enabled"] == false
        && status["module"]["live"] == false
        && pid.is_none()
}

fn seed_stopped_generation(
    rig: &Rig,
    package: &str,
    agent: &str,
    generation: i64,
) -> Result<(), String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("no isolated HOME")?);
    let expected = fixture_path_for(&home, &rig.basal.path, "basal")?;
    let status = ck(&["module", "status", "basal"])?;
    if !fully_stopped(&status, basal_pid()) {
        return Err(format!("basal is not fully stopped: {status}"));
    }
    no_store_handles(&expected)?;
    let mut c = rusqlite::Connection::open_with_flags(
        &expected,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let changed = tx.execute("UPDATE instance_generations SET generation=?3 WHERE package=?1 AND agent_id=?2 AND generation < ?3", rusqlite::params![package,agent,generation]).map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err(format!("expected one generation row, changed {changed}"));
    }
    tx.commit().map_err(|e| e.to_string())
}

/// With basal stopped, set its generation to N = core's generation + 1000.
/// A subsequent lower-generation ensure is refused with generation_stale and
/// detail.current = N. Core must persist exactly N as its new generation counter.
/// Read that counter through persona.check; this is not a capture of the reply.
/// Core's store and all production module bytes remain untouched.
pub(super) async fn stale(
    rig: &Rig,
    agent: &Agent,
    package: &str,
    id: &str,
    repin: impl FnOnce() -> Result<(), String>,
) -> Case {
    let mut case = Case::new(
        "packages: seeded stale ensure verified by core's exact observable floor (no wire capture)",
    );
    let before_core = super::package_cases::view(rig, agent).await;
    let core = before_core
        .as_ref()
        .ok()
        .and_then(|v| super::package_cases::instance_view(v, package))
        .unwrap_or(Value::Null);
    let before = rig.basal.instance(id).ok().flatten().unwrap_or(Value::Null);
    let Some(generation) = core["generation"]
        .as_i64()
        .filter(|g| *g > 0 && *g < i64::MAX - 1000)
    else {
        case.check(
            "read a valid core generation",
            false,
            evidence(&before_core),
        );
        return case;
    };
    let seeded = generation + 1000;
    case.record(
        "before",
        json!({"persona_check":evidence(&before_core),"basal":before,"seeded_generation":seeded}),
    );
    if !case.check(
        "stop only ckdev-basal before fixture seeding",
        ck(&["module", "stop", "basal"]).is_ok(),
        Value::Null,
    ) {
        return case;
    }
    let changed = seed_stopped_generation(rig, package, &agent.id, seeded);
    case.check(
        "seed only the stopped isolated pair's generation",
        changed.is_ok(),
        json!(changed),
    );
    let started = ck(&["module", "start", "basal"]);
    if !case.check(
        "restart ckdev-basal after closing the fixture connection",
        started.is_ok(),
        json!(started),
    ) || !case.passed()
    {
        return case;
    }
    let written = repin();
    case.check(
        "restore the persona pin to require a real core ensure",
        written.is_ok(),
        json!(written),
    );
    let flushed = super::package_cases::flush(rig, agent).await;
    case.check(
        "persona.flush queues the ensure from real core",
        flushed.is_ok(),
        evidence(&flushed),
    );
    let learned = poll(Duration::from_secs(10), || async {
        let v = super::package_cases::view(rig, agent).await.ok()?;
        let i = super::package_cases::instance_view(&v, package)?;
        (i["generation"] == seeded && i["marker"].is_null()).then_some(v)
    })
    .await;
    let after = rig.basal.instance(id).ok().flatten().unwrap_or(Value::Null);
    case.check(
        "core learned exactly the distinctive current floor from the live generation_stale refusal",
        learned.is_some(),
        json!({"persona_check":learned,"seeded_generation":seeded}),
    );
    let mut expected = before.clone();
    expected["generation"] = json!(seeded);
    case.check(
        "the stale ensure left basal's instance and enable switch unchanged",
        after == expected,
        json!({"before":before,"after":after}),
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    let stable = super::package_cases::view(rig, agent).await;
    case.check(
        "core has no retry loop: the floor and cleared marker stay stable",
        stable.as_ref().is_ok_and(|v| {
            super::package_cases::instance_view(v, package)
                .is_some_and(|i| i["generation"] == seeded && i["marker"].is_null())
        }),
        evidence(&stable),
    );
    case.record(
        "after",
        json!({"persona_check":evidence(&stable),"basal":rig.basal.instance(id)}),
    );
    case
}

pub(super) fn no_store_handles(path: &std::path::Path) -> Result<(), String> {
    let mut command = std::process::Command::new("lsof");
    command.arg("-F").arg("p").arg(path);
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            command.arg(sidecar);
        }
    }
    let output = command.output().map_err(|e| e.to_string())?;
    if output.status.code() != Some(1) || !output.stdout.is_empty() || !output.stderr.is_empty() {
        return Err(format!(
            "cannot prove the isolated SQLite store and sidecars have no open handles: {output:?}"
        ));
    }
    Ok(())
}
