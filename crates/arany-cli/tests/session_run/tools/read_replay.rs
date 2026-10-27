use super::*;

#[tokio::test]
async fn budget_incident_failed_reads_then_next_action_has_distinct_terminal_feedback() {
    use arany::test_support::{goals::*, scenario::ScriptedProvider};
    for exhausted in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = facts_workspace(temp.path(), "facts");
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).unwrap();
        private_file(&state_path.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let provider = Arc::new(
            ScriptedProvider::new(
                (0..3)
                    .map(|_| {
                        crate::tool_outcome(ToolCall::Read {
                            path: "notes/missing.txt".into(),
                            offset: None,
                            limit: None,
                        })
                    })
                    .chain([finish_with("Recovered")])
                    .collect(),
            )
            .with_known_input_tokens(),
        );
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut input = proposal_request(&workspace);
        input.include_paths.clear();
        input.goal = arany::GoalRequest::new(
            arany::GoalBudget {
                max_provider_calls: exhausted.then_some(3),
                ..Default::default()
            },
            arany::GoalPhase::Implementation,
        );
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        engine.close().await.unwrap();
        let (events, view) = replayed(&state_path, result.session_id).await;
        assert_eq!(view.runs[0], result.run);
        assert_eq!(view.goals[0].budget_closed, exhausted);
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            if exhausted { 3 } else { 4 }
        );
        let feedback = arany_cli::presentation::session_run_feedback(&view, &view.runs[0]);
        if exhausted {
            let feedback = feedback.unwrap();
            assert!(feedback.starts_with("Arany status: Goal budget refused Tool: Provider calls; exhausted; used at refusal 3, pinned limit 3; requested 1; required minimum 1; final settled usage 3."));
            assert_eq!(feedback.matches("primary: read failed").count(), 2);
            assert!(
                feedback
                    .ends_with("No answer was committed; completed actions remain in the Session.")
            );
            assert_eq!(
                arany_cli::presentation::render_session(
                    &view,
                    &events,
                    arany_cli::presentation::Output::Text
                )
                .matches(&feedback)
                .count(),
                1
            );
        } else {
            assert_eq!(feedback, None);
        }
        assert!(
            matches!(events.last().unwrap().event, arany::Event::GoalFinished { exhausted: closed, .. } if closed == exhausted)
        );
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(&workspace)
            .args(["show", "--output", "text", "--state-dir"])
            .arg(&state_path)
            .arg(result.session_id.to_string());
        let output = crate::process_output_before_deadline(command);
        assert!(output.status.success(), "show failed: {output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(
            output.stdout,
            arany_cli::presentation::render_session(
                &view,
                &events,
                arany_cli::presentation::Output::Text
            )
            .as_bytes()
        );
    }
}

const IN_PROCESS_READS: usize = 24;

struct InProcessReads;
#[async_trait::async_trait]
impl Provider for InProcessReads {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "read-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = request
            .input
            .tools
            .as_ref()
            .ok_or(ProviderError::InvalidOutcome)?
            .observation_count();
        let outcome = match step % 4 {
            _ if step == IN_PROCESS_READS => ProviderOutcome::Answer(Answer {
                text: "Read files and discovered paths.".into(),
            }),
            0 => crate::tool_outcome(ToolCall::Read {
                path: "src/main.c".into(),
                offset: None,
                limit: None,
            }),
            1 => crate::tool_outcome(ToolCall::List { path: "src".into() }),
            2 => crate::tool_outcome(ToolCall::Search {
                path: "src".into(),
                query: r"^INT \w+\(".into(),
                regex: true,
                include: Some("*.c".into()),
                case_sensitive: false,
                offset: 0,
                limit: 2,
            }),
            _ => crate::tool_outcome(ToolCall::Glob {
                pattern: "**".into(),
                path: "src".into(),
                limit: 100,
            }),
        };
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: None,
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

fn assert_read_observations(run: &arany::RunView) {
    assert_eq!(run.tools.len(), IN_PROCESS_READS);
    for tool in &run.tools {
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        if matches!(tool.intent.call, ToolCall::Read { .. }) {
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::InProcess { .. })
            ));
        } else {
            assert!(matches!(
                observation.attestation,
                Some(arany::Attestation::Guard(_))
            ));
        }
        assert!(!observation.output.contains("secret-canary"));
        assert!(!observation.output.contains(".env"));
        if matches!(tool.intent.call, ToolCall::Search { .. }) {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&observation.output).unwrap(),
                serde_json::json!({"entries":[{"path":"src/main.c","line":1,"text":"int main(void) { return 0; }"}],"limit":2,"may_be_truncated":false,"next_offset":null})
            );
        }
    }
    let globbed: Vec<_> = run
        .tools
        .iter()
        .filter(|tool| matches!(tool.intent.call, ToolCall::Glob { .. }))
        .collect();
    assert_eq!(globbed.len(), IN_PROCESS_READS / 4);
    for tool in globbed {
        assert_eq!(
            tool.observation.as_ref().unwrap().output,
            r#"{"entries":[{"bytes":29,"path":"src/main.c"}],"limit":100,"may_be_truncated":false}"#
        );
    }
}

#[tokio::test]
async fn in_process_reads_are_attested_and_replay_after_exit_or_a_crash() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(
        &workspace.join("src/main.c"),
        b"int main(void) { return 0; }\n",
    );
    private_file(&workspace.join("src/.env"), b"TOKEN=secret-canary\n");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    private_file(
        &state_path.join("tools.json"),
        br#"{"version":1,"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    );
    let provider = Arc::new(InProcessReads);
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let outcome = run_with(
        &engine,
        &provider,
        proposal_request(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "{}",
        crate::safe_run_diagnostics(&outcome.run)
    );
    assert_read_observations(&outcome.run);

    // A fresh read-only Store stands in for the next process.
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    store.close().await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs[0], outcome.run);

    let last_started = events
        .iter()
        .rposition(|envelope| matches!(envelope.event, arany::Event::ToolStarted { .. }))
        .unwrap();
    let arany::Event::ToolStarted {
        active_ceiling_ms, ..
    } = &events[last_started].event
    else {
        unreachable!("located above");
    };
    let crashed = SessionView::replay(outcome.session_id, &events[..=last_started])
        .unwrap()
        .unwrap();
    assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
    assert_eq!(crashed.goals[0].status, arany::GoalStatus::Interrupted);
    assert!(crashed.runs[0].tools.last().unwrap().observation.is_none());
    assert!(*active_ceiling_ms > 0);
    assert_eq!(
        crashed.goals[0].active_ceiling_ms, *active_ceiling_ms,
        "replay keeps the open read's ceiling to charge"
    );
}

#[tokio::test]
async fn unknown_spill_is_an_attested_failed_read_outside_workspace_path_scopes() {
    use arany::test_support::{goals::finish_with, scenario::ScriptedProvider};
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    private_file(&state_path.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
    let provider = Arc::new(ScriptedProvider::new(vec![
        crate::tool_outcome(ToolCall::Read {
            path: format!("spill:{}", "a".repeat(64)),
            offset: None,
            limit: None,
        }),
        finish_with("Read refused safely"),
    ]));
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let outcome = run_with(
        &engine,
        &provider,
        proposal_request(&workspace),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Failed);
    assert_eq!(observation.output, "no spill for this Run");
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::InProcess { .. })
    ));
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    engine.close().await.unwrap();
}
