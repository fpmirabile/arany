use super::*;

struct ApprovalJourney {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    call: ToolCall,
    review: &'static str,
}
#[async_trait::async_trait]
impl Provider for ApprovalJourney {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "guard-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        let review = request.phase == AgentPhase::ToolReview;
        requests.push(request);
        if review && self.review == "error" {
            return Err(ProviderError::InvalidOutcome);
        }
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: if first {
                crate::tool_outcome(self.call.clone())
            } else {
                match (review, self.review) {
                    (true, "ask") => ProviderOutcome::Review(arany::ReviewVerdict::Ask),
                    (true, _) => ProviderOutcome::Review(arany::ReviewVerdict::Approve),
                    (false, _) => ProviderOutcome::Answer(Answer {
                        text: "Action result observed".into(),
                    }),
                }
            },
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(3),
                output_tokens: Some(if review && self.review == "over-cap" {
                    257
                } else {
                    1
                }),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
    async fn compact(&self, _: CompactionRequest) -> Result<CompactionResponse, ProviderError> {
        Err(ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
#[ignore = "native Linux Guard approval and AI review gate"]
async fn approvals_bind_exact_actions_and_review_usage_survives_closed_replay() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    use arany::{ApprovalMode, ProviderCallDisposition, RunCancellation, RunHints, ToolApprovals};
    for (name, mode, review, decision, command) in [
        ("request-allow", ApprovalMode::Request, "", "allow", false),
        ("request-deny", ApprovalMode::Request, "", "deny", false),
        ("request-drop", ApprovalMode::Request, "", "drop", false),
        (
            "request-headless",
            ApprovalMode::Request,
            "",
            "headless",
            false,
        ),
        ("request-cancel", ApprovalMode::Request, "", "cancel", false),
        ("auto-edit", ApprovalMode::AutoEdits, "", "none", false),
        ("auto-command", ApprovalMode::AutoEdits, "", "allow", true),
        ("ai-approve", ApprovalMode::Auto, "approve", "none", false),
        ("ai-ask", ApprovalMode::Auto, "ask", "allow", false),
        ("ai-malformed", ApprovalMode::Auto, "maybe", "deny", false),
        ("ai-over-cap", ApprovalMode::Auto, "over-cap", "deny", false),
        ("ai-error", ApprovalMode::Auto, "error", "allow", false),
        ("ai-error-deny", ApprovalMode::Auto, "error", "deny", false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        let state_path = temp.path().join("state");
        let state = StateRoot::admit(&state_path).unwrap();
        let config = json!({"version":1,"workspace_paths":["."],"write":true,"commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],"skills":[],"mcp":[]});
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let call = if command {
            ToolCall::Command {
                program: "bash".into(),
                args: vec![
                    "-c".into(),
                    "printf published > command-output.txt; printf approval-command-passed".into(),
                ],
                cwd: "".into(),
            }
        } else {
            ToolCall::Write {
                path: "README.md".into(),
                expected_digest: None,
                content: "2026-10-06\n".into(),
            }
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(ApprovalJourney {
            requests: requests.clone(),
            call: call.clone(),
            review,
        });
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (approvals, mut inbox) = ToolApprovals::new(mode);
        let cancellation = RunCancellation::new();
        let pending = cancellation.clone();
        let approve = async {
            if matches!(decision, "none" | "headless") {
                return;
            }
            let action = inbox.next().await.expect("one exact pending intent");
            assert_eq!(action.intent.call, call);
            assert_eq!(
                action.allow_auto_edits,
                mode == ApprovalMode::Request && !command
            );
            match decision {
                "allow" => action.decide(true),
                "deny" => action.decide(false),
                "drop" => drop(action),
                "cancel" => {
                    pending.cancel();
                    drop(action);
                }
                _ => unreachable!(),
            }
        };
        let (outcome, ()) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(
                run_with(
                    &engine,
                    &provider,
                    proposal_request(&workspace),
                    RunControls {
                        tools: ToolGrant::Configured,
                        approvals: (decision != "headless").then(|| approvals.clone()),
                        cancellation,
                        hints: Some(RunHints::new()),
                        ..Default::default()
                    }
                ),
                approve
            )
        })
        .await
        .expect("bounded approval journey");
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        let allowed = decision == "allow" || decision == "none";
        assert_eq!(
            outcome.run.status,
            if decision == "cancel" {
                RunStatus::Cancelled
            } else {
                RunStatus::Finished
            },
            "{name}: {:?}",
            outcome.run
        );
        assert_eq!(
            workspace.join("README.md").exists(),
            allowed && !command,
            "{name}"
        );
        if allowed && !command {
            assert_eq!(
                std::fs::read(workspace.join("README.md")).unwrap(),
                b"2026-10-06\n"
            );
        }
        assert_eq!(
            workspace.join("command-output.txt").exists(),
            allowed && command
        );
        if allowed && command {
            assert_eq!(
                std::fs::read(workspace.join("command-output.txt")).unwrap(),
                b"published"
            );
        }
        let observed = requests.lock().unwrap().clone();
        let review_count = usize::from(mode == ApprovalMode::Auto);
        assert_eq!(
            observed.len(),
            1 + review_count + usize::from(decision != "cancel"),
            "{name}"
        );
        assert_eq!(observed[0].phase, AgentPhase::RootPlan);
        assert_eq!(observed[0].input.objective, "Guard corpus");
        assert!(observed[0].input.tools.is_some());
        if review_count == 1 {
            let request = &observed[1];
            assert_eq!(request.phase, AgentPhase::ToolReview);
            assert_eq!(request.max_output_tokens, 256);
            assert!(
                request.input.tools.is_none()
                    && request.input.includes.is_empty()
                    && request.input.images.is_empty()
                    && request.input.history.is_empty()
                    && request.input.child_results.is_empty()
                    && request.input.context_summary.is_none()
            );
            let payload: Value = serde_json::from_str(&request.input.objective).unwrap();
            assert_eq!(payload["user_request"], "Guard corpus");
            assert_eq!(
                payload["proposed_action"]["call"],
                serde_json::to_value(&call).unwrap()
            );
        }
        if allowed {
            let continuation = observed.last().unwrap();
            assert_eq!(continuation.phase, AgentPhase::RootPlan);
            let observations = &continuation.input.tools.as_ref().unwrap().observations();
            assert_eq!(observations.len(), if command { 2 } else { 1 });
            assert_eq!(observations[0].intent.call, call);
            assert_eq!(
                observations[0].disposition,
                ToolDisposition::Succeeded,
                "{name}"
            );
            if command {
                let result: Value = serde_json::from_str(&observations[0].output).unwrap();
                assert_eq!(result["exit_code"], 0);
                assert_eq!(result["stdout"], "approval-command-passed");
                assert_eq!(result["stderr"], "");
                assert_eq!(result["changeset"]["files"].as_array().unwrap().len(), 1);
                assert!(matches!(
                    observations[1].intent.call,
                    ToolCall::ApplyChangeset { .. }
                ));
                assert_eq!(observations[1].disposition, ToolDisposition::Succeeded);
            }
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let replay = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(replay.runs[0].status, outcome.run.status);
        assert_eq!(replay.runs[0].tools, outcome.run.tools);
        let calls = &replay.runs[0].agents[0].provider_calls;
        assert_eq!(
            calls.len(),
            1 + review_count + usize::from(decision != "cancel")
        );
        if review_count == 1 {
            assert_eq!(calls[1].phase, AgentPhase::ToolReview);
            assert_eq!(
                calls[1].disposition,
                if matches!(review, "error" | "maybe") {
                    ProviderCallDisposition::InvalidResponse
                } else if review == "over-cap" {
                    ProviderCallDisposition::OutputLimit
                } else {
                    ProviderCallDisposition::Finished
                }
            );
            if !matches!(review, "error" | "over-cap") {
                assert_eq!(calls[1].input_tokens, Some(3));
                assert_eq!(calls[1].output_tokens, Some(1));
            }
        }
        assert_eq!(
            replay.runs[0].assistant_message.as_deref(),
            (decision != "cancel").then_some("Action result observed")
        );
        assert_native_approval_refusal(decision, &replay.runs[0], &observed);
        assert!(
            replay.runs[0].tools[0]
                .observation
                .as_ref()
                .unwrap()
                .attestation
                .is_some()
                == allowed
        );
    }
}

fn assert_native_approval_refusal(
    decision: &str,
    run: &arany::RunView,
    requests: &[ProviderRequest],
) {
    let label = match decision {
        "deny" => "user_declined",
        "drop" | "headless" => "channel_unavailable",
        _ => return,
    };
    assert!(run.failure.is_none());
    assert_eq!(run.tools.len(), 1, "{decision}: no automatic retry");
    let observation = run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Denied);
    assert_eq!(
        observation.output,
        format!("{label}: no operation was dispatched")
    );
    assert!(observation.attestation.is_none());
    let continuation = requests.last().unwrap();
    assert_eq!(continuation.phase, AgentPhase::RootPlan);
    assert_eq!(
        continuation.input.tools.as_ref().unwrap().observations(),
        vec![observation]
    );
}
