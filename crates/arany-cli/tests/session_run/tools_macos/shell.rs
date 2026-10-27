use super::*;

const REFUSAL_TIMEOUT: Duration = Duration::from_secs(30);

pub(super) async fn refuse_options(root: &Path, workspace: &Path, config: &serde_json::Value) {
    for option in ["-lc", "-i"] {
        trace_case(root, "refusal");
        let state_path = root.join(format!("state-shell{option}"));
        let state = StateRoot::admit(&state_path).unwrap();
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(config).unwrap(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let call = ToolCall::Command {
            program: "shell".into(),
            args: vec![
                option.into(),
                "printf dispatched > src/shell-started.txt".into(),
            ],
            cwd: ".".into(),
        };
        let provider = Arc::new(Journey {
            requests: requests.clone(),
            calls: vec![call],
            linked_file: None,
        });
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let outcome = tokio::time::timeout(
            REFUSAL_TIMEOUT,
            run_with(
                &engine,
                &provider,
                request(workspace),
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("bounded shell-option refusal")
        .unwrap();
        engine.close().await.unwrap();
        assert_eq!(outcome.run.status, RunStatus::Failed, "{option}");
        assert_eq!(
            outcome.run.failure,
            Some(arany::RunFailure::ProposalRefused),
            "{option}"
        );
        assert!(outcome.run.tools.is_empty(), "{option}");
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "no continuation: {option}"
        );
        assert_eq!(trace_snapshot(root)["stages"]["admission"], "unknown");
        assert_eq!(trace_snapshot(root)["stages"]["worker_go"], "unknown");
        assert!(!workspace.join("src/shell-started.txt").exists());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let refusals: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                arany::Event::ToolPreflightRefused { record, .. } => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(refusals.len(), 1, "one shell proposal refusal: {option}");
        let refusal = refusals[0];
        assert_eq!(
            refusal.refusal,
            arany::ToolRefusal {
                cause: arany::ProposalRefusalCause::InvalidArguments,
                reason: arany::ToolRefusalReason::Payload,
            }
        );
        assert!(refusal.verdict.is_none());
        assert!(!events.iter().any(|event| matches!(
            event.event,
            arany::Event::ToolProposed { .. }
                | arany::Event::ToolStarted { .. }
                | arany::Event::ToolFinished { .. }
        )));
        let primary = outcome
            .run
            .agents
            .iter()
            .find(|agent| agent.role == arany::AgentRole::Primary)
            .unwrap();
        assert_eq!(primary.tool_refusal.as_ref(), Some(refusal));
        let replay = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(
            replay.runs[0], outcome.run,
            "closed shell refusal replay: {option}"
        );
    }
}

fn request(workspace: &Path) -> RunRequest {
    RunRequest {
        skill: None,
        session_id: None,
        title: Some("Refuse shell startup options".into()),
        objective: "Refuse login and interactive shell commands before dispatch".into(),
        images: Vec::new(),
        workspace: workspace.into(),
        launch_directory: None,
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
        goal: arany::GoalRequest::default(),
    }
}
