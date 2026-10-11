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

/// Generation orders core's ensure/remove requests for one package/agent pair.
/// With basal stopped, set its generation to N = core's generation + 1000.
/// Basal refuses the next older ensure with generation_stale and current = N.
/// Require persona.check to show generation N + 1 with version 1 confirmed,
/// and basal's instance to agree. Core starts below N and only the stale refusal
/// supplies N, so this exact recovery proves it learned the refusal's current
/// generation. This is an inference; no refusal reply or receipt is captured.
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
    let seeded_row = rig.basal.instance(id);
    let mut expected = before.clone();
    expected["generation"] = json!(seeded);
    case.check(
        "basal stores exactly the distinctive seeded generation while stopped",
        seeded_row
            .as_ref()
            .is_ok_and(|row| row.as_ref() == Some(&expected)),
        json!({"seeded_generation":seeded,"basal":seeded_row}),
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
    // Core's independent recovery timer can advance its stored generation from N
    // to N + 1 and re-ensure basal's removed instance before either state is read.
    // Do not require a glimpse of core at N or basal still removed after refusal.
    // Instead, require the eventual confirmed generation N + 1: only basal's stale
    // refusal supplies N. The polling bound stops a hang; elapsed time proves nothing.
    let recovered = poll(Duration::from_secs(30), || async {
        let v = super::package_cases::view(rig, agent).await.ok()?;
        let i = super::package_cases::instance_view(&v, package)?;
        confirmed_recovery(&i, seeded).then_some(v)
    })
    .await;
    case.check(
        "core reached exactly seeded+1, which only basal's stale refusal could have taught it (inferred, not captured)",
        recovered.is_some(),
        json!({"persona_check":recovered,"seeded_generation":seeded}),
    );
    let corrected = rig.basal.instance(id);
    case.check(
        "basal ends at seeded+1, enabled and not removed, with its enable switch untouched",
        corrected.as_ref().is_ok_and(|instance| {
            instance.as_ref().is_some_and(|i| {
                i["generation"] == seeded + 1
                    && i["operation"] == "ensure"
                    && i["removed"] == false
                    && i["state"] == "enabled"
                    && basal_rig::packages::same_enable_state(&before, i)
            })
        }),
        json!(&corrected),
    );
    case.record(
        "after",
        json!({"persona_check":recovered,"basal":corrected}),
    );
    case
}

fn confirmed_recovery(instance: &Value, seeded: i64) -> bool {
    instance["generation"] == seeded + 1
        && instance["confirmed_version"] == 1
        && instance.get("marker") == Some(&Value::Null)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_recovery_requires_exact_next_generation_and_confirmation() {
        assert!(confirmed_recovery(
            &json!({"generation":1003,"confirmed_version":1,"marker":null}),
            1002
        ));
        for wrong in [
            json!({"generation":1002,"confirmed_version":1,"marker":null}),
            json!({"generation":1004,"confirmed_version":1,"marker":null}),
            json!({"generation":1003,"confirmed_version":null,"marker":null}),
            json!({"generation":1003,"confirmed_version":1,"marker":{}}),
            json!({"generation":1003,"confirmed_version":1}),
        ] {
            assert!(!confirmed_recovery(&wrong, 1002));
        }
    }
}
