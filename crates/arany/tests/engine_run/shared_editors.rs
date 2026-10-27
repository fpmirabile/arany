use super::*;
use arany::test_support::goals::{assert_action_bounds, facts_workspace, replayed};

#[tokio::test]
async fn foreground_worker_goal_refuses_editor_authority_without_a_write_capable_parent() {
    use arany::worker::{EditorAssignment, WorkerMode};
    for (case, calls, refused_count) in [
        ("foreground", 3, 3),
        ("background", 3, 3),
        ("mixed", 3, 3),
        ("wave", 3, 3),
        ("append_failed", 1, 0),
        ("append_unknown", 1, 1),
        ("cancelled", 1, 1),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "Fact");
        let state_path = temp.path().join("state");
        let mut assignments = vec![arany::ChildAssignment {
            objective: "Edit notes".into(),
            profile: None,
            editor: Some(Box::new(EditorAssignment {
                commands: Vec::new(),
                definition: "editor".into(),
                definition_digest: [1; 32],
                mission: "Edit notes".into(),
                direction: String::new(),
                lease: vec!["notes".into()],
                criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/a".into(),
                    content: "done".into(),
                },
                mode: if case == "background" {
                    WorkerMode::Background
                } else {
                    WorkerMode::Foreground
                },
                max_turns: 17,
                wave: if case == "wave" { 2 } else { 1 },
                resumes: None,
                supersedes: None,
            })),
        }];
        if case == "mixed" {
            assignments.push(arany::ChildAssignment {
                editor: None,
                objective: "Research".into(),
                profile: None,
            });
        }
        let provider = Arc::new(ScriptedProvider::new(
            (0..3)
                .map(|_| {
                    ProviderOutcome::Delegate(Delegate {
                        agent: None,
                        children: assignments.clone(),
                    })
                })
                .collect(),
        ));
        let engine = engine_support::open(
            StateRoot::admit(&state_path).unwrap(),
            EngineConfig::default(),
        )
        .unwrap();
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        let refused = |event: &Event| matches!(event, Event::ProviderCallRecorded { record, .. } if record.refusal.is_some());
        let gate = if case == "append_unknown" || case == "cancelled" {
            Some(arany::FaultPlan::gate_goal_append_acknowledgment(
                session, refused,
            ))
        } else {
            if case == "append_failed" {
                arany::FaultPlan::fail_goal_append(session, refused);
            }
            None
        };
        let mut input = request(&workspace, "Edit notes");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Auto {
            max_active_children: 3,
        };
        let cancellation = arany::RunCancellation::new();
        let result = {
            let run = run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    cancellation: cancellation.clone(),
                    ..Default::default()
                },
            );
            tokio::pin!(run);
            if let Some((entered, release)) = gate {
                tokio::select! {
                    result = &mut run => panic!("Run ended before refusal acknowledgment: {result:?}"),
                    entered = entered => entered.unwrap(),
                }
                if case == "cancelled" {
                    cancellation.cancel();
                    release.send(()).unwrap();
                } else {
                    drop(release);
                }
            }
            run.await
        };
        if case.starts_with("append_") {
            assert!(
                matches!(result, Err(EngineError::Store(arany::StoreError::Closed))),
                "{case}: failed or unknown append must stop"
            );
        } else {
            let outcome = result.unwrap();
            assert_eq!(
                outcome.run.status,
                if case == "cancelled" {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                },
                "{case}"
            );
        }
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            calls,
            "{case}: no child egress"
        );
        assert_eq!(provider.responses.lock().unwrap().len(), 3 - calls);
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, session).await;
        assert_eq!(view.runs[0].agents.len(), 1, "{case}: no child spawn");
        assert!(view.runs[0].tools.is_empty(), "{case}: no effect");
        let refusals = view.runs[0].agents[0]
            .provider_calls
            .iter()
            .filter_map(|r| r.refusal.as_ref())
            .collect::<Vec<_>>();
        assert_eq!(refusals.len(), refused_count);
        if calls == 3 {
            assert_eq!(view.goals[0].status, arany::GoalStatus::Failed);
        }
        assert!(
            refusals
                .iter()
                .all(|r| r.cause == arany::ProposalRefusalCause::EditorGrant)
        );
        assert!(
            view.runs[0]
                .agents
                .iter()
                .all(|agent| agent.assignment.is_none())
        );
        assert_action_bounds(&events, false, false);
    }
}
