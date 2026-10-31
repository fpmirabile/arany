use super::*;

struct OverTimeoutCommand {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    planned: bool,
}
#[async_trait::async_trait]
impl Provider for OverTimeoutCommand {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "openai",
                    "goal-resource-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = {
            let mut requests = self.requests.lock().expect("Provider requests");
            let step = requests.len();
            requests.push(request);
            step
        };
        let outcome = if step <= usize::from(self.planned) {
            let call = ToolCall::Command {
                program: "bash".into(),
                args: vec!["-c".into(), "touch src/not-dispatched".into()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: (!self.planned).then_some(2_000),
                additional_reads: None,
            };
            if self.planned && step == 0 {
                let mut contract = proposal_plan_contract(&call).unwrap();
                contract.command_timeout_ms = Some(2_000);
                arany::test_support::scenario::plan_outcome(contract)
            } else {
                crate::tool_outcome(call)
            }
        } else {
            ProviderOutcome::Answer(arany::Answer {
                text: "The command was not dispatched".into(),
            })
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

#[tokio::test]
async fn over_max_command_is_closed_as_a_non_dispatched_limit() {
    for planned in [false, true] {
        over_max_command(planned).await;
    }
}

async fn over_max_command(planned: bool) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    basic_config(&state_path);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(OverTimeoutCommand {
        requests: Arc::clone(&requests),
        planned,
    });
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut request = proposal_request(&workspace);
    request.goal = arany::GoalRequest::new(
        arany::GoalBudget::default(),
        arany::GoalPhase::Implementation,
    );
    request.goal.resource_settings.default_command_timeout_ms = 1_000;
    request.goal.resource_settings.max_command_timeout_ms = 1_000;
    let outcome = run_with(
        &engine,
        &provider,
        request,
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
        if planned {
            RunStatus::Failed
        } else {
            RunStatus::Finished
        }
    );
    assert_eq!(outcome.run.tools.len(), 1);
    let tool = &outcome.run.tools[0];
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(tool.intent.limits.runtime_ms, 2_000);
    assert_eq!(observation.disposition, ToolDisposition::Limit);
    assert_eq!(
        observation.limit_cause,
        Some(arany::LimitCause::RequestedTime)
    );
    assert!(observation.attestation.is_none());
    assert_eq!(observation.cache_update, arany::CacheUpdate::NotApplicable);
    assert!(
        observation
            .output
            .contains("Lower request or change /settings")
    );
    assert!(!workspace.join("src/not-dispatched").exists());
    assert_eq!(requests.lock().unwrap().len(), 2 + usize::from(planned));

    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let started = events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                arany::Event::ToolStarted { intent, .. } if *intent == tool.intent.clone().into()
            )
        })
        .unwrap();
    let finished = events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                arany::Event::ToolFinished { observation, .. } if observation.intent == tool.intent
            )
        })
        .unwrap();
    assert!(started < finished);
    assert!(matches!(
        &events[finished].event,
        arany::Event::ToolFinished { observation, .. }
            if observation.disposition == ToolDisposition::Limit && observation.attestation.is_none()
    ));
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs[0], outcome.run);
    assert_eq!(replay.goals[0].plan_contract.is_some(), planned);
    assert_eq!(
        replay.goals[0].resource_settings.max_command_timeout_ms,
        1_000
    );
    assert!(
        replay.goals[0]
            .report
            .as_ref()
            .unwrap()
            .unresolved
            .iter()
            .any(|item| item.contains("Lower request or change /settings"))
    );
    store.close().await.unwrap();
}

fn assert_goal_provider_request(request: &ProviderRequest, expected_budget: &arany::GoalBudget) {
    assert_eq!(request.phase, AgentPhase::RootPlan);
    assert_eq!(request.collaboration, CollaborationPolicy::Single);
    assert_eq!(request.input.objective, "Guard corpus");
    assert!(
        request.input.goal_budget.is_none(),
        "a plain turn sends no budget block"
    );
    assert_eq!(&request.input.pinned_budget, expected_budget);
    assert!(request.input.goal_plan_contract.is_none());
    assert!(request.input.tools.is_some());
}

struct ExactTokenTool {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}

#[async_trait::async_trait]
impl arany::Provider for ExactTokenTool {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "openai",
                    "goal-budget-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(
        &self,
        request: ProviderRequest,
    ) -> Result<arany::ProviderResponse, arany::ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(arany::ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: crate::tool_outcome(ToolCall::Edit {
                path: "src/current.txt".into(),
                expected_digest: Some(hash(b"current")),
                old: "current".into(),
                new: "after".into(),
            }),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(0),
                output_tokens: Some(8),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        _: CompactionRequest,
    ) -> Result<CompactionResponse, arany::ProviderError> {
        Err(arany::ProviderError::InvalidOutcome)
    }
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux account-isolated Goal token admission"
)]
async fn exact_goal_token_cap_fails_before_a_tool_request_is_journaled() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    basic_config(&state_path);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(ExactTokenTool {
        requests: Arc::clone(&requests),
    });
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut request = proposal_request(&workspace);
    request.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_tokens: Some(8),
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    let outcome = run_with(
        &engine,
        &provider,
        request,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();

    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert!(outcome.run.tools.is_empty());
    assert_eq!(
        std::fs::read(workspace.join("src/current.txt")).unwrap(),
        b"current"
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_goal_provider_request(
            &requests[0],
            &arany::GoalBudget {
                max_tokens: Some(8),
                ..arany::GoalBudget::default()
            },
        );
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.event,
        arany::Event::ProviderCallRecorded { record, .. }
            if record.disposition == arany::ProviderCallDisposition::InvalidResponse
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(&event.event, arany::Event::ToolStarted { .. }))
    );
    store.close().await.unwrap();
}

struct GoalWorkspaceLease {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
}
#[async_trait::async_trait]
impl Provider for GoalWorkspaceLease {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new(
                    "openai",
                    "goal-lease-model",
                    arany::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: ProviderOutcome::Answer(Answer {
                text: "No Workspace mutation was needed".into(),
            }),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
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
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux cross-StateRoot Goal Workspace lease"
)]
async fn paused_implementation_goal_does_not_hold_the_workspace_writer() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let first_state_path = temp.path().join("first-state");
    let second_state_path = temp.path().join("second-state");
    for state_path in [&first_state_path, &second_state_path] {
        let state = StateRoot::admit(state_path).unwrap();
        drop(state);
        basic_config(state_path);
    }

    let first_requests = Arc::new(Mutex::new(Vec::new()));
    let first_engine_provider = Arc::new(GoalWorkspaceLease {
        requests: Arc::clone(&first_requests),
    });
    let first_engine = arany::test_support::engine::open(
        StateRoot::open_existing(&first_state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let mut first = proposal_request(&workspace);
    first.goal = arany::GoalRequest::new(
        arany::GoalBudget {
            max_runs: 2,
            ..arany::GoalBudget::default()
        },
        arany::GoalPhase::Implementation,
    );
    first.goal.finish_after_run = false;
    let initial = run_with(
        &first_engine,
        &first_engine_provider,
        first,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(initial.run.status, RunStatus::Finished);
    let lease_budget = arany::GoalBudget {
        max_runs: 2,
        ..arany::GoalBudget::default()
    };
    {
        let requests = first_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_goal_provider_request(&requests[0], &lease_budget);
    }

    let blocked_requests = Arc::new(Mutex::new(Vec::new()));
    let blocked_engine_provider = Arc::new(GoalWorkspaceLease {
        requests: Arc::clone(&blocked_requests),
    });
    let blocked_engine = arany::test_support::engine::open(
        StateRoot::open_existing(&second_state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    // The paused Goal holds no writer lock, so another StateRoot's Goal writes the Workspace.
    assert_eq!(
        run_with(
            &blocked_engine,
            &blocked_engine_provider,
            proposal_request(&workspace),
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .run
        .status,
        RunStatus::Finished
    );
    assert_eq!(blocked_requests.lock().unwrap().len(), 1);
    blocked_engine.close().await.unwrap();

    let mut resumed = proposal_request(&workspace);
    resumed.session_id = Some(initial.session_id);
    resumed.title = None;
    resumed.goal = arany::GoalRequest::resume(initial.goal_id, true);
    let terminal = run_with(
        &first_engine,
        &first_engine_provider,
        resumed,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(terminal.run.status, RunStatus::Finished);
    {
        let requests = first_requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_goal_provider_request(request, &lease_budget);
        }
    }

    let released_requests = Arc::new(Mutex::new(Vec::new()));
    let released_engine_provider = Arc::new(GoalWorkspaceLease {
        requests: Arc::clone(&released_requests),
    });
    let released_engine = arany::test_support::engine::open(
        StateRoot::open_existing(&second_state_path).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        run_with(
            &released_engine,
            &released_engine_provider,
            proposal_request(&workspace),
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            }
        )
        .await
        .unwrap()
        .run
        .status,
        RunStatus::Finished
    );
    {
        let requests = released_requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_goal_provider_request(&requests[0], &arany::GoalBudget::default());
    }
    released_engine.close().await.unwrap();
    first_engine.close().await.unwrap();
}
