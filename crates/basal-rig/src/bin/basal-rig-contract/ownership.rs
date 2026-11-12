use super::*;

/// An agent-owned install naming another agent would be refused by basal before
/// execution. Approve a global flow with a foreign grant, then change only core's
/// stored author while core is stopped. The unchanged script/grants now reach
/// core's own owner-ID comparison, matching the setup of its sink unit tests.
pub(super) async fn cross_agent(rig: &Rig, agent: &Agent, tag: &str, machine_id: &str) -> Case {
    let mut case =
        Case::new("runtime sinks: seeded authorship fixture reaches core's cross-agent refusal");
    let session = format!("ses_rig_foreign_{tag}");
    let created = rig.client.call(CORE,&BindIdentity::new("/","opencode",&session),"agent.create",json!({
        "role":"assistant","name":format!("RigForeign{tag}"),"tag":"isolated sink recipient",
        "residence":{"machine_id":machine_id,"harness":"opencode","address_json":{"version":1,"server":"local","session":session}}
    })).await;
    let foreign = created
        .as_ref()
        .ok()
        .and_then(|v| find_str(v, "agent_id"))
        .unwrap_or_default();
    if !case.check(
        "register a different live digest recipient",
        !foreign.is_empty() && foreign != agent.id,
        evidence(&created),
    ) {
        return case;
    }
    let flow = flows::writer(
        &format!("rig-owner-{tag}"),
        &foreign,
        "Refuse a granted recipient when stable authorship belongs to another agent.",
    );
    if install_with_pause(rig, &mut case, &flow, None, "approve", true)
        .await
        .is_none()
    {
        return case;
    }
    let paused = rig.disable(&flow.id).await;
    if !case.check(
        "pause the approved global flow before seeding authorship",
        paused.is_ok(),
        evidence(&paused),
    ) {
        return case;
    }
    let stopped = super::seeded::ck(&["module", "stop", "prefrontal-core"]);
    if !case.check(
        "stop only ckdev-prefrontal-core before the isolated fixture write",
        stopped.is_ok(),
        json!(stopped),
    ) {
        return case;
    }
    let seeded = seed_authorship(rig, &flow.id, &agent.id);
    case.record("authorship_fixture", json!(&seeded));
    let started = super::seeded::ck(&["module", "start", "prefrontal-core"]);
    case.check(
        "restart core after closing the fixture connection",
        started.is_ok(),
        json!(started),
    );
    if !case.check(
        "only one approved row's author changes; all other stored values remain identical",
        seeded.is_ok(),
        json!(&seeded),
    ) || !case.passed()
    {
        return case;
    }
    let before = rig.core.digest_rows(&foreign, &flow.id);
    case.check(
        "read the foreign digest before the runtime probe",
        before == Ok(json!([])),
        json!(&before),
    );
    let prior = rows(
        &mut case,
        "read runs before the runtime probe",
        rig.basal.runs(&flow.id),
    );
    let enabled = rig
        .client
        .basal_as_operator("flow.enable", json!({"flow_id":flow.id}))
        .await;
    if case.check(
        "resume the approved script and unchanged foreign grants",
        enabled.is_ok(),
        evidence(&enabled),
    ) {
        let run = poll(RUN_WAIT, || async {
            rig.basal
                .runs(&flow.id)
                .ok()?
                .into_iter()
                .find(|r| r.ended() && !prior.iter().any(|old| old.run_id == r.run_id))
        })
        .await;
        case.record(
            "disable_after_probe",
            evidence(&rig.disable(&flow.id).await),
        );
        let calls = rows(
            &mut case,
            "read the live sink journal",
            rig.basal.calls(&flow.id),
        );
        let result = run
            .as_ref()
            .and_then(|r| r.result.clone())
            .unwrap_or(Value::Null);
        case.record("run_result", result.clone());
        case.check("both calls reached the live host, not basal's install or manifest refusal",[KIND_SINK_DIGEST,KIND_SINK_STATUS].iter().all(|kind| calls.iter().any(|c| c.kind_code == *kind && c.dispatch == "sent" && c.attempts > 0 && c.request.as_ref().is_some_and(|r| r["agent"] == foreign))),json!(calls.iter().map(|c| json!({"kind":c.kind_code,"dispatch":c.dispatch,"attempts":c.attempts,"request":c.request,"value":c.value})).collect::<Vec<_>>()));
        case.check(
            "core refuses both real sink writes with sink_target_not_owner",
            refused_by_core(&result),
            result,
        );
        case.check(
            "nothing landed in the foreign agent's digest",
            rig.core.digest_rows(&foreign, &flow.id) == before && before == Ok(json!([])),
            json!(rig.core.digest_rows(&foreign, &flow.id)),
        );
        let receipts = rows(
            &mut case,
            "read core's sink receipts",
            rig.core.receipts(&flow.id),
        );
        case.check(
            "core committed no receipt for the refused probe run",
            run.as_ref()
                .is_some_and(|run| !receipts.iter().any(|r| r.key[2] == run.run_id)),
            json!(receipts.iter().map(|r| &r.key).collect::<Vec<_>>()),
        );
    }
    case
}

pub(super) fn refused_by_core(result: &Value) -> bool {
    ["digest", "status"]
        .iter()
        .all(|sink| result[*sink]["refused"]["data"]["code"] == "sink_target_not_owner")
}

/// Compare every stored table value except the single permitted authorship cell.
/// Hashing both sides of the stopped-store write catches accidental grant, hash,
/// manifest or unrelated-row changes without copying private database contents.
pub(super) fn other_values_fingerprint(
    c: &rusqlite::Connection,
    flow_id: &str,
) -> Result<String, String> {
    let mut names=c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name").map_err(|e|e.to_string())?;
    let tables = names
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let mut hash = blake3::Hasher::new();
    let mut omitted = 0;
    for table in tables {
        hash.update(&(table.len() as u64).to_le_bytes());
        hash.update(table.as_bytes());
        let quoted = table.replace('"', "\"\"");
        let mut s = c
            .prepare(&format!("SELECT * FROM \"{quoted}\""))
            .map_err(|e| e.to_string())?;
        let columns = s
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        hash.update(
            serde_json::to_string(&columns)
                .map_err(|e| e.to_string())?
                .as_bytes(),
        );
        let mut rows = s.query([]).map_err(|e| e.to_string())?;
        let mut encoded = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let target = table == "flow_install"
                && columns
                    .iter()
                    .position(|c| c == "flow_id")
                    .is_some_and(|i| row.get::<_, String>(i).ok().as_deref() == Some(flow_id))
                && columns
                    .iter()
                    .position(|c| c == "version")
                    .is_some_and(|i| row.get::<_, i64>(i).ok() == Some(1));
            let mut values = Vec::new();
            for (i, column) in columns.iter().enumerate() {
                if target && column == "author_agent_id" {
                    values.push("permitted authorship cell".to_owned());
                    omitted += 1;
                } else {
                    values.push(format!(
                        "{:?}",
                        row.get::<_, rusqlite::types::Value>(i)
                            .map_err(|e| e.to_string())?
                    ));
                }
            }
            encoded.push(serde_json::to_string(&values).map_err(|e| e.to_string())?);
        }
        encoded.sort();
        for row in encoded {
            hash.update(&(row.len() as u64).to_le_bytes());
            hash.update(row.as_bytes());
        }
    }
    if omitted != 1 {
        return Err(format!(
            "expected one permitted authorship cell, found {omitted}"
        ));
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn seed_authorship(rig: &Rig, flow_id: &str, author: &str) -> Result<Value, String> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("missing isolated HOME")?);
    let path = super::seeded::fixture_path_for(&home, &rig.core.path, "prefrontal-core")?;
    let status = super::seeded::ck(&["module", "status", "prefrontal-core"])?;
    if !super::seeded::fully_stopped(&status, None) {
        return Err(format!("core is not terminally stopped: {status}"));
    }
    super::seeded::no_store_handles(&path)?;
    let mut c =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| e.to_string())?;
    let tx = c.transaction().map_err(|e| e.to_string())?;
    let before=tx.query_row("SELECT author_agent_id FROM flow_install WHERE flow_id=?1 AND version=1 AND revoked_at_ms IS NULL",[flow_id],|r|r.get::<_,Option<String>>(0)).map_err(|e|e.to_string())?;
    if before.is_some() {
        return Err("fixture requires an approved global flow".into());
    }
    let before_hash = other_values_fingerprint(&tx, flow_id)?;
    let changed=tx.execute("UPDATE flow_install SET author_agent_id=?2 WHERE flow_id=?1 AND version=1 AND revoked_at_ms IS NULL AND author_agent_id IS NULL",rusqlite::params![flow_id,author]).map_err(|e|e.to_string())?;
    if changed != 1 {
        return Err(format!("expected one authorship row, changed {changed}"));
    }
    let after = tx
        .query_row(
            "SELECT author_agent_id FROM flow_install WHERE flow_id=?1 AND version=1",
            [flow_id],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| e.to_string())?;
    let after_hash = other_values_fingerprint(&tx, flow_id)?;
    if before_hash != after_hash || after != author {
        return Err("the fixture changed values beyond the permitted authorship cell".into());
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(
        json!({"flow_id":flow_id,"author_before":before,"author_after":after,"other_table_values_before":before_hash,"other_table_values_after":after_hash,"open_store_handles":0,"core_status":status}),
    )
}
