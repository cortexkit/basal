use super::*;

fn boards_present(value: &Value) -> bool {
    value["boards"].as_array().is_some_and(|boards| {
        !boards.is_empty()
            && boards.iter().all(|board| {
                board["agent_id"].as_str().is_some_and(|id| !id.is_empty())
                    && board["board"].is_object()
            })
    })
}

fn read_journaled(flow: &Flow, run: &Run, calls: &[Call]) -> bool {
    let hash = basal_core::ids::hex(&basal_core::ids::code_hash(&flow.script, &flow.manifest));
    let calls: Vec<_> = calls
        .iter()
        .filter(|call| call.run_id == run.run_id)
        .collect();
    calls.len() == 1
        && calls[0].position == 0
        && calls[0].dispatch == "sent"
        && calls[0].attempts > 0
        && calls[0].settlement.as_deref() == Some("fulfilled")
        && calls[0].request.as_ref()
            == Some(&json!({"limit":16,"stamp":{
            "flow_id":flow.id,"flow_version":flow.version,"code_hash":hash}}))
        && calls[0].value.as_ref().is_some_and(boards_present)
        && calls[0].value == run.result
}

pub(super) async fn run(rig: &Rig, flow: &Flow) -> Vec<Case> {
    let mut negative = Case::new("board.read: missing board_read is refused by basal at install");
    let mut manifest = flow.manifest_value();
    manifest.as_object_mut().unwrap().remove("board_read");
    let missing = Flow {
        manifest: manifest.to_string(),
        ..flow.clone()
    };
    let refused = rig
        .client
        .basal_as_operator("flow.install", install_params(&missing))
        .await;
    negative.check(
        "basal refuses the unusable manifest and names board_read",
        refused
            .as_ref()
            .is_err_and(|e| e.code == "install_refused" && e.message.contains("board_read")),
        evidence(&refused),
    );
    negative.check("no install reaches core and no run is admitted",
        rig.core.installs(&flow.id).is_ok_and(|installs| installs.as_array().is_some_and(Vec::is_empty))
            && rig.basal.runs(&flow.id).is_ok_and(|runs| runs.is_empty()),
        json!({"core":rig.core.installs(&flow.id),"basal_runs":rig.basal.runs(&flow.id).map(|r|r.len())}));

    let mut positive =
        Case::new("board.read: approved global audience reads live boards with a journaled stamp");
    if install_with_route(
        rig,
        &mut positive,
        flow,
        InstallRoute::Operator,
        "approve",
        false,
    )
    .await
    .is_some()
    {
        let enabled = rig.wait_enabled(flow).await;
        positive.check(
            "the card-approved board reader is enabled",
            enabled.is_some(),
            json!(enabled),
        );
        let run = rig.first_run(flow).await;
        positive.check(
            "the run succeeds and reads at least one populated board reply",
            run.as_ref().is_some_and(|r| {
                r.state == "succeeded" && r.result.as_ref().is_some_and(boards_present)
            }),
            json!(run.as_ref().map(
                |r| json!({"run_id":r.run_id,"state":r.state,"result":r.result,"error":r.error})
            )),
        );
        let calls = rig.basal.calls(&flow.id);
        positive.check("the board read and approved stamp are journaled as a sent, fulfilled query with the returned boards",
            run.as_ref().zip(calls.as_ref().ok()).is_some_and(|(r,c)|read_journaled(flow,r,c)),
            json!(calls.as_ref().ok().map(|calls|calls.iter().map(|c|json!({"run_id":c.run_id,"position":c.position,"dispatch":c.dispatch,"attempts":c.attempts,"settlement":c.settlement,"request":c.request,"value":c.value})).collect::<Vec<_>>())));
        let disabled = rig.disable(&flow.id).await;
        positive.check(
            "disable the board reader after observing its first run",
            disabled.is_ok(),
            evidence(&disabled),
        );
    }
    vec![negative, positive]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn board_reader_fixture_declares_both_grants_and_missing_grant_is_invalid() {
        let flow = flows::board_read("rig-boards");
        let parsed = basal_core::Manifest::parse(&flow.manifest).unwrap();
        assert_eq!(parsed.board_read, ["$audience"]);
        assert!(parsed.lists_op(CORE, "board.read"));
        assert!(matches!(
            parsed.audience,
            Some(basal_core::manifest::Audience::Global {})
        ));
        assert_eq!(
            flow.script,
            "return await ops.call('prefrontal-core', 'board.read', {limit:16});"
        );
        let mut manifest = flow.manifest_value();
        manifest.as_object_mut().unwrap().remove("board_read");
        assert!(
            basal_core::Manifest::parse(&manifest.to_string())
                .unwrap_err()
                .to_string()
                .contains("board_read")
        );
    }

    #[test]
    fn board_read_evidence_requires_a_sent_stamped_read_and_nonempty_boards() {
        let flow = flows::board_read("rig-boards");
        let result = json!({"boards":[{"agent_id":"agent_a","board":{"roomId":"board_a"}}],"next_cursor":null,"page_cap":16});
        let mut run = Run {
            run_id: "run-a".into(),
            state: "succeeded".into(),
            result: Some(result.clone()),
            error: None,
        };
        let call = Call {
            run_id: run.run_id.clone(),
            position: 0,
            kind_code: 0,
            dispatch: "sent".into(),
            attempts: 1,
            settlement: Some("fulfilled".into()),
            value: Some(result),
            request: Some(json!({"limit":16,"stamp":{
                "flow_id":flow.id,"flow_version":flow.version,"code_hash":basal_core::ids::hex(&basal_core::ids::code_hash(&flow.script,&flow.manifest))}})),
        };
        assert!(read_journaled(&flow, &run, std::slice::from_ref(&call)));
        assert!(!read_journaled(&flow, &run, &[]));
        for change in [
            "stamp",
            "dispatch",
            "settlement",
            "value",
            "attempts",
            "run",
        ] {
            let mut changed = call.clone();
            match change {
                "stamp" => changed.request.as_mut().unwrap()["stamp"]["code_hash"] = json!("wrong"),
                "dispatch" => changed.dispatch = "not_sent".into(),
                "settlement" => changed.settlement = Some("rejected".into()),
                "value" => changed.value = Some(json!({"boards":[]})),
                "attempts" => changed.attempts = 0,
                _ => changed.run_id = "other".into(),
            }
            assert!(!read_journaled(&flow, &run, &[changed]), "{change}");
        }
        for value in [
            json!({}),
            json!({"boards":[]}),
            json!({"boards":[{}]}),
            json!({"boards":[{"agent_id":"agent_a"}]}),
        ] {
            assert!(!boards_present(&value), "{value}");
        }
        run.result = Some(json!({"boards":[]}));
        assert!(!read_journaled(&flow, &run, &[call]));
    }
}
