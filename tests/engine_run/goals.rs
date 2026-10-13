use super::*;

#[tokio::test]
async fn goal_lifecycle_bounds_report_and_requires_goal_replay_linkage() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "ࠀ".repeat(2500),
        result: "accepted result".into(),
    })]);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut input = request(&workspace, "Complete a Goal");
    input.include_paths.clear();
    let outcome = engine.run(input).await.expect("Goal Run");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.expect("engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("Events");
    let view = SessionView::replay(outcome.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    let [goal] = view.goals.as_slice() else {
        panic!("one Goal is linked to the accepted task");
    };
    assert_eq!(goal.id, outcome.goal_id);
    assert_eq!(goal.run_ids, [outcome.run.id]);
    assert_eq!(goal.status, arany::GoalStatus::Completed);
    let report = goal.report.as_ref().expect("durable report");
    assert!(report.summary.len() <= 2048);
    assert_eq!(report.summary.len() % 3, 0, "UTF-8 truncation boundary");
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("summary was shortened"))
    );
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("token usage is incomplete"))
    );

    let without_goal: Vec<_> = events
        .iter()
        .filter(|event| {
            !matches!(
                &event.event,
                Event::GoalStarted { .. }
                    | Event::GoalRunLinked { .. }
                    | Event::GoalActiveTimeReserved { .. }
                    | Event::GoalActiveTimeSettled { .. }
                    | Event::GoalProviderCallReserved { .. }
                    | Event::GoalFinished { .. }
            )
        })
        .cloned()
        .collect();
    assert!(
        SessionView::replay(outcome.session_id, &without_goal).is_err(),
        "current-format replay rejects accepted tasks without Goal linkage"
    );
    store.close().await.expect("Store shutdown");
}

#[tokio::test]
async fn goal_failure_and_cancellation_have_distinct_terminal_receipts() {
    for cancel in [false, true] {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_path = temp.path().join("state");
        let cancellation = RunCancellation::new();
        let mut provider = if cancel {
            ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: "answer".into(),
            })])
        } else {
            ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
                summary: String::new(),
                result: "invalid finish".into(),
            })])
        };
        if cancel {
            provider.cancel_at_completion = Some((AgentPhase::RootPlan, cancellation.clone()));
        }
        let mut engine =
            Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
        let mut input = request(&workspace, "Finish or cancel");
        input.include_paths.clear();
        let outcome = engine
            .run_with_cancel(input, cancellation)
            .await
            .expect("terminal Run");
        assert_eq!(
            outcome.run.status,
            if cancel {
                RunStatus::Cancelled
            } else {
                RunStatus::Failed
            }
        );
        engine.close().await.expect("engine shutdown");

        let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
            .expect("read-only Store");
        let view = store
            .load_view(outcome.session_id)
            .await
            .expect("replayed Session")
            .expect("Session");
        assert_eq!(
            view.goals[0].status,
            if cancel {
                arany::GoalStatus::Cancelled
            } else {
                arany::GoalStatus::Failed
            }
        );
        assert!(view.goals[0].report.is_some());
        store.close().await.expect("Store shutdown");
    }
}

#[tokio::test]
async fn goal_call_budget_blocks_a_later_run_before_provider_dispatch() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "first Run complete".into(),
        result: "continue later".into(),
    })]);
    let requests = Arc::clone(&provider.requests);
    let mut engine =
        Engine::open(StateRoot::admit(&state_path).expect("state"), provider).expect("engine");
    let mut first = request(&workspace, "Use one Provider call");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_provider_calls: 1,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    first.goal.finish_after_run = false;
    let initial = engine.run(first).await.expect("first Goal Run");

    let mut second = request(&workspace, "Try to use another Provider call");
    second.session_id = Some(initial.session_id);
    second.include_paths.clear();
    second.goal =
        arany::GoalRequest::resume(initial.goal_id, arany::GoalPhase::Implementation, true);
    assert!(matches!(
        engine.run(second).await,
        Err(EngineError::GoalBudgetExhausted)
    ));
    assert_eq!(requests.lock().expect("Provider requests").len(), 1);
    engine.close().await.expect("engine shutdown");
}

#[tokio::test]
async fn interrupted_goal_resumes_only_by_exact_id_without_replaying_the_old_call() {
    let temp = tempfile::tempdir().expect("temporary root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("workspace");
    let state_path = temp.path().join("state");
    let first_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "first Run complete".into(),
        result: "continue after review".into(),
    })]);
    let first_requests = Arc::clone(&first_provider.requests);
    let mut first_engine = Engine::open(
        StateRoot::admit(&state_path).expect("state"),
        first_provider,
    )
    .expect("first engine");
    let mut first = request(&workspace, "Inspect current state");
    first.include_paths.clear();
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_runs: 2,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Research,
    );
    first.goal.finish_after_run = false;
    let initial = first_engine.run(first).await.expect("first Goal Run");
    assert_eq!(first_requests.lock().expect("Provider requests").len(), 1);
    first_engine.close().await.expect("first engine shutdown");

    let second_provider = ScriptedProvider::new(vec![ProviderOutcome::Finish(Finish {
        summary: "resumed Run complete".into(),
        result: "the Goal is complete".into(),
    })]);
    let second_requests = Arc::clone(&second_provider.requests);
    let mut second_engine = Engine::open(
        StateRoot::open_existing(&state_path).expect("reopened state"),
        second_provider,
    )
    .expect("second engine");
    let mut mismatched = request(&workspace, "Must not select a different Goal");
    mismatched.session_id = Some(initial.session_id);
    mismatched.include_paths.clear();
    mismatched.goal =
        arany::GoalRequest::resume(arany::GoalId::new(), arany::GoalPhase::Implementation, true);
    assert!(matches!(
        second_engine.run(mismatched).await,
        Err(EngineError::GoalUnavailable)
    ));
    assert!(
        second_requests
            .lock()
            .expect("Provider requests")
            .is_empty()
    );

    let mut resumed = request(&workspace, "Complete the reviewed Goal");
    resumed.session_id = Some(initial.session_id);
    resumed.include_paths.clear();
    resumed.goal =
        arany::GoalRequest::resume(initial.goal_id, arany::GoalPhase::Implementation, true);
    let final_run = second_engine.run(resumed).await.expect("exact Goal resume");
    assert_eq!(second_requests.lock().expect("Provider requests").len(), 1);
    second_engine.close().await.expect("second engine shutdown");

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state"))
        .expect("read-only Store");
    let events = store
        .load_session(initial.session_id)
        .await
        .expect("Events");
    let view = SessionView::replay(initial.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.goals[0].id, initial.goal_id);
    assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
    assert_eq!(view.goals[0].run_ids, [initial.run.id, final_run.run.id]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(&event.event, Event::GoalResumed { .. }))
            .count(),
        1
    );
    assert_eq!(
        view.runs.len(),
        2,
        "old Run remains history; only one new Run executes"
    );
    store.close().await.expect("Store shutdown");
}
