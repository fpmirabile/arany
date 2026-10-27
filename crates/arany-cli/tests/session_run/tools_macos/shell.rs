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
            calls: vec![call.clone()],
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
            Some(arany::RunFailure::ToolStopped),
            "{option}"
        );
        assert_eq!(outcome.run.tools.len(), 1, "{option}");
        assert_eq!(
            requests.lock().unwrap().len(),
            2,
            "no continuation: {option}"
        );
        let observation = outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(observation.intent.call, call);
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert!(observation.attestation.is_none());
        assert!(observation.enforcement_cause.is_none());
        assert!(observation.output.contains("no operation was dispatched"));
        assert!(
            observation
                .output
                .contains("Cause: pinned shell requires -c SCRIPT")
        );
        assert_eq!(trace_snapshot(root)["stages"]["worker_go"], "unknown");
        assert!(!workspace.join("src/shell-started.txt").exists());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
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
