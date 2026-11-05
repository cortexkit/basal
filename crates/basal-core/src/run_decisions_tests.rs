use super::*;
use crate::{
    decisions::{self, Answered, CardState},
    reconcile::Resolution,
};
use basal_host::{DecisionAnswer, DecisionKind, DecisionPath};

fn unknown(f: &Fixture) {
    f.seed(true, false);
    f.run()
        .enter()
        .unwrap()
        .finish(|r| {
            r.append(
                1,
                Body::Unknown {
                    position: 0,
                    reason: "basal_restarted".into(),
                },
            )?;
            Ok(())
        })
        .unwrap();
}
fn raised(
    f: &Fixture,
    card: &decisions::DecisionRecord,
    choice: &str,
) -> (DecisionPath, DecisionAnswer) {
    let path = DecisionPath::Consent("consent-provider".into());
    f.store
        .write(|tx| {
            tx.execute(
                "UPDATE decision_cards SET consent_path=?2 WHERE seq=?1",
                params![card.seq, path.key()],
            )?;
            decisions::mark_raised(tx, card.seq, card.revision, "decision-id")
        })
        .unwrap();
    (
        path,
        DecisionAnswer {
            grant_context: None,
            elicitation_id: "decision-id".into(),
            choice: Some(choice.into()),
            dedup_key: Some(card.dedup_key.clone()),
            flow_id: card.flow_id.clone(),
            version: card.version,
            decision: DecisionKind::Reconcile,
            run_id: card.run_id.clone(),
            call_key: card.call_key.clone(),
        },
    )
}

#[test]
fn leave_only_declines_the_card_and_does_not_block_a_later_resolution() {
    let f = Fixture::new();
    unknown(&f);
    let cards = f.hosts.decisions_due(100).unwrap();
    assert_eq!(cards.len(), 1);
    let (path, answer) = raised(&f, &cards[0], decisions::LEAVE);
    let before = f.log.records.lock().unwrap().len();
    assert_eq!(
        f.hosts.answer_decision(&path, &answer, 101).unwrap().0,
        Answered::Now(CardState::Declined)
    );
    assert_eq!(f.log.records.lock().unwrap().len(), before);
    assert!(f.state().calls[&0].unknown.is_some());
    assert!(f.hosts.decisions_due(102).unwrap().is_empty());
    assert_eq!(
        f.hosts
            .resolve(
                &f.live.run_id,
                0,
                Resolution::NotApplied,
                "operator",
                None,
                103
            )
            .unwrap(),
        RunState::Pending
    );
    let state = f.state();
    assert!(state.calls[&0].unknown.is_none());
    assert_eq!(state.calls[&0].resolutions.len(), 1);
    assert_eq!(state.calls[&0].resolutions[0].attempts, 1);
}

#[test]
fn live_resolution_syncs_exact_attempt_once_and_replays_observed_usage() {
    let f = Fixture::new();
    unknown(&f);
    let cards = f.hosts.decisions_due(100).unwrap();
    let (path, answer) = raised(&f, &cards[0], decisions::APPLIED);
    assert_eq!(
        f.hosts.answer_decision(&path, &answer, 101).unwrap().0,
        Answered::Now(CardState::Applied)
    );
    assert_eq!(
        f.hosts.answer_decision(&path, &answer, 102).unwrap().0,
        Answered::AlreadyAnswered(CardState::Applied)
    );
    assert_eq!(f.state().calls[&0].resolutions.len(), 1);
    assert!(
        f.state().calls[&0]
            .arrival
            .as_ref()
            .unwrap()
            .value
            .as_str()
            .contains(crate::reconcile::RECONCILED_AS_APPLIED)
    );
    let recovered = RecoveredHosts::recover(
        f.store.clone(),
        f.log.clone(),
        f.log.clone(),
        &f.log.recovered(),
        budgets(),
        Arc::new(NoHooks),
        f.clock.clone(),
    )
    .unwrap();
    assert!(recovered.decisions_due(104).unwrap().is_empty());
    let state = recovered
        .registry()
        .get(&f.live.run_id)
        .unwrap()
        .enter()
        .unwrap()
        .finish(|r| Ok(r.state().clone()))
        .unwrap();
    assert_eq!(state.calls[&0].arrival, f.state().calls[&0].arrival);

    let f = Fixture::new();
    unknown(&f);
    f.hosts
        .resolve(
            &f.live.run_id,
            0,
            Resolution::ObservedResult(f.completion().outcome),
            "operator",
            None,
            100,
        )
        .unwrap();
    let arrival = f.state().calls[&0].arrival.clone().unwrap();
    assert_eq!(
        arrival.usage,
        Some(Usage {
            input_tokens: 4,
            output_tokens: 2
        })
    );
    assert_eq!(arrival.cost_micros, Some(1));
}

#[test]
fn decisions_due_revalidates_registry_generation_after_its_sqlite_transaction() {
    let f = Fixture::new();
    unknown(&f);
    let cards = f
        .hosts
        .decisions_due_interleave(100, || {
            f.run()
                .claim(1, |generation, a| {
                    a.records().append(generation, Body::Activated)?;
                    Ok(())
                })
                .unwrap();
        })
        .unwrap();
    assert!(
        cards.is_empty(),
        "a candidate from a previous registry generation must not be raised"
    );
    assert_eq!(f.hosts.decisions_due(101).unwrap().len(), 1);
}

#[test]
fn recovered_durable_resolution_finishes_an_open_card_without_reasking() {
    let f = Fixture::new();
    unknown(&f);
    let card = f.hosts.decisions_due(100).unwrap().remove(0);
    f.run()
        .enter()
        .unwrap()
        .finish(|r| {
            r.append(
                1,
                Body::Resolution(crate::run_records::Resolution {
                    position: 0,
                    attempts: 1,
                    idempotency_key: None,
                    choice: "not_applied".into(),
                    observed_outcome: None,
                    usage: None,
                    elicitation_id: "lost-commit".into(),
                }),
            )?;
            Ok(())
        })
        .unwrap();
    let recovered = RecoveredHosts::recover(
        f.store.clone(),
        f.log.clone(),
        f.log.clone(),
        &f.log.recovered(),
        budgets(),
        Arc::new(NoHooks),
        f.clock.clone(),
    )
    .unwrap();
    assert!(recovered.decisions_due(101).unwrap().is_empty());
    let cards = f.store.read(decisions::all).unwrap();
    assert_eq!(cards[0].seq, card.seq);
    assert_eq!(cards[0].state, CardState::Applied);
}

#[test]
fn cancel_card_commits_with_terminal_history_and_recovers_before_attach() {
    let f = Fixture::new();
    unknown(&f);
    f.hosts
        .configure_decisions(Arc::new(Effects), Arc::new(Effects))
        .unwrap();
    let card = f.hosts.decisions_due(100).unwrap().remove(0);
    let (path, answer) = raised(&f, &card, decisions::CANCEL);
    assert_eq!(
        f.hosts.answer_decision(&path, &answer, 101).unwrap().0,
        Answered::Now(CardState::Applied)
    );
    assert_eq!(
        f.store
            .read(|c| sql::history(c, &f.live.run_id))
            .unwrap()
            .unwrap()
            .state,
        RunState::Cancelled
    );
    assert!(f.hosts.registry().get(&f.live.run_id).is_none());
    assert_eq!(
        f.store.read(decisions::all).unwrap()[0].state,
        CardState::Applied
    );

    let f = Fixture::new();
    unknown(&f);
    f.hosts.decisions_due(100).unwrap();
    f.run()
        .enter()
        .unwrap()
        .finish(|r| {
            r.append(
                1,
                Body::Resolution(crate::run_records::Resolution {
                    position: 0,
                    attempts: 1,
                    idempotency_key: None,
                    choice: "cancel".into(),
                    observed_outcome: None,
                    usage: None,
                    elicitation_id: "lost-commit".into(),
                }),
            )?;
            Ok(())
        })
        .unwrap();
    let recovered = RecoveredHosts::recover(
        f.store.clone(),
        f.log.clone(),
        f.log.clone(),
        &f.log.recovered(),
        budgets(),
        Arc::new(NoHooks),
        f.clock.clone(),
    )
    .unwrap();
    recovered
        .configure_decisions(Arc::new(Effects), Arc::new(Effects))
        .unwrap();
    assert!(recovered.registry().get(&f.live.run_id).is_none());
    assert_eq!(
        f.store.read(decisions::all).unwrap()[0].state,
        CardState::Applied
    );
    assert_eq!(
        f.store
            .read(|c| sql::history(c, &f.live.run_id))
            .unwrap()
            .unwrap()
            .state,
        RunState::Cancelled
    );
}

#[test]
fn closed_resolution_is_sqlite_only_and_settles_usage_cost_and_exact_card() {
    let f = Fixture::new();
    unknown(&f);
    f.close();
    let card = f.hosts.decisions_due(100).unwrap().remove(0);
    let before = f.log.records.lock().unwrap().len();
    assert_eq!(
        f.hosts
            .resolve(
                &f.live.run_id,
                0,
                Resolution::ObservedResult(f.completion().outcome),
                "operator",
                Some(&card),
                101
            )
            .unwrap(),
        RunState::Cancelled
    );
    assert_eq!(f.log.records.lock().unwrap().len(), before);
    assert_eq!(f.obligation().state, ObligationState::Settled);
    f.store
        .read(|c| {
            let usage: (u64, u64, u64, u64) = c.query_row(
                "SELECT reserved,input_tokens,cache_write_tokens,output_tokens FROM token_windows",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            assert_eq!(usage, (0, 3, 1, 2));
            assert_eq!(
                c.query_row("SELECT charged FROM classify_costs", [], |r| r
                    .get::<_, f64>(0))?,
                0.000001
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        f.store.read(decisions::all).unwrap()[0].state,
        CardState::Applied
    );
    assert!(
        f.hosts
            .resolve(
                &f.live.run_id,
                0,
                Resolution::NotApplied,
                "operator",
                None,
                102
            )
            .is_err()
    );
    assert!(
        f.hosts
            .resolve(
                &f.live.run_id,
                0,
                Resolution::ObservedResult(f.completion().outcome),
                "operator",
                Some(&card),
                102
            )
            .is_err()
    );
}

#[test]
fn history_lost_is_not_recovered_and_close_keeps_saved_broca_obligations_without_fabricated_calls()
{
    let f = Fixture::new();
    f.store
        .write(|tx| {
            tx.execute("UPDATE live_runs SET history_lost=1", [])?;
            Ok(())
        })
        .unwrap();
    let recovered = RecoveredHosts::recover(
        f.store.clone(),
        f.log.clone(),
        f.log.clone(),
        &[],
        budgets(),
        Arc::new(NoHooks),
        f.clock.clone(),
    )
    .unwrap();
    assert!(recovered.registry().snapshot().is_empty());
    assert!(
        f.store
            .read(|c| Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM live_runs WHERE flow_id='flow')",
                [],
                |r| r.get::<_, bool>(0)
            )?))
            .unwrap()
    );
    // Production still references journal here; the log-backed schema will
    // instead reference run_identities. This test table accepts a saved
    // snapshot without manufacturing an unrelated legacy journal row.
    f.store.write(|tx| { tx.execute("DROP TABLE broca_calls", [])?; tx.execute_batch("CREATE TABLE broca_calls(send_id TEXT PRIMARY KEY,run_id TEXT,position INTEGER,snapshot TEXT)")?;
        tx.execute("INSERT INTO broca_calls VALUES('send','run-host',7,?1)", [json!({"params":"{\"frozen\":true}","handle":"late-handle","acknowledged":false}).to_string()])?; Ok(()) }).unwrap();
    recovered
        .cancel_history_lost(&f.live.run_id, "operator", 101)
        .unwrap();
    assert!(
        f.store
            .read(|c| sql::live(c, &f.live.run_id))
            .unwrap()
            .is_none()
    );
    let history = f
        .store
        .read(|c| sql::history(c, &f.live.run_id))
        .unwrap()
        .unwrap();
    assert!(history.history_lost);
    assert!(history.error_detail.unwrap().contains("under-count"));
    assert_eq!(
        f.store
            .read(
                |c| Ok(c.query_row("SELECT count(*) FROM run_calls", [], |r| r.get::<_, i64>(0))?)
            )
            .unwrap(),
        0
    );
    let obligation = f
        .store
        .read(|c| sql::obligation(c, &f.live.run_id, 7))
        .unwrap()
        .unwrap();
    assert_eq!(obligation.handle.as_deref(), Some("late-handle"));
    assert_eq!(obligation.detail["params"], "{\"frozen\":true}");
    assert!(
        recovered
            .cancel_history_lost(&f.live.run_id, "operator", 102)
            .is_err()
    );
    let completion = Completion {
        run_id: f.live.run_id.clone(),
        position: 7,
        handle: "late-handle".into(),
        outcome: HostOutcome::fulfilled(JsonText::null()),
    };
    assert_eq!(
        recovered.complete(&completion).unwrap(),
        CompletionAck::Accepted
    );
    assert_eq!(
        recovered.complete(&completion).unwrap(),
        CompletionAck::Duplicate
    );
    let mut missing = completion;
    missing.position = 99;
    assert_eq!(
        recovered.complete(&missing).unwrap(),
        CompletionAck::Quarantined
    );
}

#[test]
fn competing_effective_answers_have_one_exact_attempt_winner() {
    let f = Fixture::new();
    unknown(&f);
    let card = f.hosts.decisions_due(100).unwrap().remove(0);
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            f.hosts.resolve(
                &f.live.run_id,
                0,
                Resolution::NotApplied,
                "operator",
                Some(&card),
                101,
            )
        });
        let b = scope.spawn(|| {
            f.hosts.resolve(
                &f.live.run_id,
                0,
                Resolution::ReconciledAsApplied,
                "operator",
                Some(&card),
                101,
            )
        });
        [a.join().unwrap(), b.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(f.state().calls[&0].resolutions.len(), 1);
    assert_eq!(
        f.store.read(decisions::all).unwrap()[0].state,
        CardState::Applied
    );
}

#[test]
fn an_old_card_cannot_resolve_a_new_unknown_attempt() {
    let f = Fixture::new();
    unknown(&f);
    let card = f.hosts.decisions_due(100).unwrap().remove(0);
    f.hosts
        .resolve(
            &f.live.run_id,
            0,
            Resolution::NotApplied,
            "operator",
            None,
            101,
        )
        .unwrap();
    f.run()
        .enter()
        .unwrap()
        .finish(|r| {
            let mut intent = r.state().calls[&0].intent.clone().unwrap();
            intent.attempts = 2;
            r.append(1, Body::Intent(intent))?;
            r.append(
                1,
                Body::Unknown {
                    position: 0,
                    reason: "basal_restarted".into(),
                },
            )?;
            Ok(())
        })
        .unwrap();
    let before = f.log.records.lock().unwrap().len();
    assert!(
        f.hosts
            .resolve(
                &f.live.run_id,
                0,
                Resolution::ReconciledAsApplied,
                "operator",
                Some(&card),
                102
            )
            .is_err()
    );
    assert_eq!(f.log.records.lock().unwrap().len(), before);
    let new_card = f.hosts.decisions_due(103).unwrap().remove(0);
    assert_eq!(new_card.instance, 2);
    assert!(f.state().calls[&0].unknown.is_some());
}

#[test]
fn decisions_due_revalidates_callbacks_and_claims_that_write_no_record() {
    for recordless_claim in [false, true] {
        let f = Fixture::new();
        unknown(&f);
        let due = f
            .hosts
            .decisions_due_interleave(100, || {
                if recordless_claim {
                    f.run().claim(1, |_, _| Ok(())).unwrap();
                } else {
                    f.run()
                        .enter()
                        .unwrap()
                        .finish(|r| {
                            r.append(
                                1,
                                Body::Unknown {
                                    position: 0,
                                    reason: "basal_restarted".into(),
                                },
                            )?;
                            Ok(())
                        })
                        .unwrap();
                }
            })
            .unwrap();
        assert!(due.is_empty());
        assert_eq!(f.hosts.decisions_due(101).unwrap().len(), 1);
    }
}
