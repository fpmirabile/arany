use super::super::test_support::*;
use super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::{CollaborationPolicy, GoalSuccessCriterion};
use crate::tools::{EffectIntent, ToolCall, ToolLimits, ToolObservation};

#[test]
fn plan_wait_does_not_block_a_terminal_failure_before_tool_start() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![
        Event::SessionStarted {
            title: "Goal".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        goal_started(goal_id, GoalBudget::default()),
    ];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, true),
    ));
    events.push(Event::GoalProviderCallReserved {
        goal_id,
        run_id,
        agent_run_id,
        active_ceiling_ms: 0,
    });
    events.push(Event::ProviderCallRecorded {
        run_id,
        agent_run_id,
        record: record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::ToolRequested,
            Some(0),
        ),
    });
    // Engine fails the primary when the Tool deadline or context capacity refuses before ToolStarted.
    events.push(Event::AgentFinished {
        worker: None,
        run_id,
        agent_run_id,
        disposition: AgentDisposition::Failed,
        summary: None,
        result: None,
    });
    events.push(Event::RunFinished {
        run_id,
        disposition: RunDisposition::Failed,
        failure: Some(crate::session::RunFailure::ProviderCall),
        active_ms: 0,
    });
    events.push(Event::GoalFinished {
        goal_id,
        status: GoalStatus::Failed,
        active_ms: 0,
        report: GoalReport {
            summary: "failed".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        },
        exhausted: false,
        budget_refusal: None,
    });
    let mut forged = events.clone();
    if let Some(Event::GoalFinished { exhausted, .. }) = forged.last_mut() {
        *exhausted = true;
    }
    assert!(
        SessionView::replay_open(session_id, &journal(session_id, forged)).is_err(),
        "exhausted is refused while the Goal still admits a Run"
    );

    let view = SessionView::replay_open(session_id, &journal(session_id, events))
        .expect("a terminal failure closes the plan wait")
        .expect("Session exists");
    let goal = view.goals.last().expect("Goal");
    assert_eq!(goal.status, GoalStatus::Failed);
    assert!(!goal.awaiting_plan_contract);
    assert_eq!(view.runs[0].status, RunStatus::Failed);
}

fn read_observation(path: &str, content: &[u8]) -> ToolObservation {
    file_observation(
        ToolCall::Read {
            path: path.into(),
            offset: None,
            limit: Some(1024),
        },
        content,
    )
}

fn file_observation(call: ToolCall, content: &[u8]) -> ToolObservation {
    let intent = EffectIntent {
        host: None,
        wrap: None,
        rewrite: Vec::new(),
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call,
        limits: ToolLimits::default(),
        use_count: 1,
    };
    observation_for(intent, content)
}

#[test]
fn plan_contract_counts_a_pre_plan_read_matching_its_digest() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let read = read_observation("src/main.rs", b"before");
    let read_intent = read.intent.clone();
    let contract = GoalPlanContract {
        mission: "Verify the file".into(),
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        paths: vec!["src/main.rs".into()],
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
            path: "src/main.rs".into(),
            sha256: crate::validate::hex_digest(b"before"),
        },
        budget: GoalBudget::default(),
    };
    let mut view = SessionView {
        usage: Default::default(),
        rewinds: Default::default(),
        id: session_id,
        title: "Goal".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some(WorkspaceIdentity {
            device: 1,
            inode: 2,
        }),
        defaults: SessionDefaults::default(),
        host_epoch_floor: 0,
        created_sequence: 0,
        last_sequence: 1,
        lineage: None,
        runs: vec![RunView {
            skill: None,
            failure: None,
            supervisor: Default::default(),
            workers: Default::default(),
            id: run_id,
            objective: GOAL_OBJECTIVE.into(),
            images: Vec::new(),
            config: Some(config(CollaborationPolicy::Single, true)),
            agents: vec![AgentView {
                tool_refusal: None,
                tool_refusal_sequence: None,
                profile: None,
                assignment: None,
                definition: None,
                id: agent_run_id,
                role: AgentRole::Primary,
                ordinal: 0,
                objective: None,
                summary: None,
                result: None,
                provider_calls: vec![
                    record(
                        AgentPhase::RootPlan,
                        ProviderCallDisposition::ToolRequested,
                        Some(0),
                    ),
                    record(
                        AgentPhase::RootPlan,
                        ProviderCallDisposition::PlanRecorded,
                        Some(0),
                    ),
                ],
                steering_notes: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 1,
            finished_sequence: None,
            tools: vec![ToolView {
                checkpoint: None,
                active_time_reserved: false,
                narrowing_source: None,
                permission_verdict: None,
                started_sequence: Some(0),
                hooks: Vec::new(),
                hook_sequence: None,
                intent: read_intent,
                observation: Some(read),
                review_call_index: None,
                finished_sequence: Some(2),
            }],
            tool_compaction_summary: None,
            tool_compaction_recent_files: Vec::new(),
            tool_compaction_covered_count: 0,
            tool_results_cleared_count: 0,
            definitions: Default::default(),
            display: Vec::new(),
            steering_notes: Vec::new(),
            lifecycle_hooks: Vec::new(),
            lifecycle_hook_sequence: None,
            provider_sequences: Default::default(),
            provider_fallbacks: Vec::new(),
            provider_retries: Vec::new(),
        }],
        compactions: Vec::new(),
        goals: vec![GoalView {
            include_paths: Some(Vec::new()),
            id: goal_id,
            objective: GOAL_OBJECTIVE.into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget::default(),
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: GoalPhase::Research,
            autonomous: false,
            status: GoalStatus::Running,
            run_ids: vec![run_id],
            active_run: Some(run_id),
            plan_contract: None,
            success_criterion_satisfied: false,
            awaiting_plan_contract: true,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_deadline: None,
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            success_state: crate::session::GoalSuccessState::NoContract,
            provider_calls: 2,
            unobserved_provider_calls: Vec::new(),
            known_tokens: 0,
            known_cached_tokens: 0,
            token_usage_unknown: false,
            report: None,
            started_sequence: 1,
            finished_sequence: None,
            active_ceiling_ms: 0,
            continues: None,
            budget_closed: false,
            budget_refusal: None,
            run_start_usage: Default::default(),
        }],
        command_tool_probe: None,
        pending_provider_setup_edit: None,
        provider_setup_uncertain: false,
        environment_capture: None,
    };
    let mut build = contract.clone();
    build.resource_class = crate::tools::ToolResourceClass::Build;
    for cpu_unavailable in [false, true] {
        let mut unavailable = view.clone();
        if cpu_unavailable {
            unavailable.goals[0].resource_settings.build_cpu_cores = None;
        } else {
            unavailable.goals[0].resource_settings.build_jobs = None;
        }
        assert!(
            unavailable
                .record_goal_plan_contract(&goal_id, &run_id, &build)
                .is_err()
        );
        assert!(
            unavailable
                .record_goal_plan_contract(&goal_id, &run_id, &contract)
                .is_ok()
        );
    }
    for path in ["unrelated", "src/main.rs"] {
        let mut after_rewind = view.clone();
        let mut rewind = super::super::rewind_tests::restored_operation();
        rewind.files[0].path = path.into();
        rewind.finished_sequence = Some(3);
        after_rewind.rewinds.operations.push(rewind);
        after_rewind.last_sequence = 4;
        after_rewind
            .record_goal_plan_contract(&goal_id, &run_id, &contract)
            .unwrap();
        assert_eq!(
            after_rewind.goals[0].success_criterion_satisfied,
            path == "unrelated"
        );
    }
    view.apply_committed(&EventEnvelope {
        sequence: 2,
        session_id,
        run_id: Some(run_id),
        agent_run_id: None,
        event: Event::GoalPlanContractRecorded {
            goal_id,
            run_id,
            contract,
        },
        created_at_ms: 0,
    })
    .expect("contract records");
    assert!(view.goals[0].success_criterion_satisfied);
}

#[test]
fn plan_contract_counts_a_write_from_an_earlier_run() {
    let session_id = SessionId::new();
    let (goal_id, earlier_run_id, run_id, agent_run_id) =
        (GoalId::new(), RunId::new(), RunId::new(), AgentRunId::new());
    let write = file_observation(
        ToolCall::Write {
            path: "src/main.rs".into(),
            expected_digest: None,
            content: "after".into(),
        },
        b"after",
    );
    let contract = GoalPlanContract {
        mission: "Write the file".into(),
        paths: vec!["src/main.rs".into()],
        commands: Vec::new(),
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/main.rs".into(),
            content: "after".into(),
        },
        budget: GoalBudget::default(),
    };
    let mut view = SessionView {
        usage: Default::default(),
        rewinds: Default::default(),
        id: session_id,
        title: "Goal".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some(WorkspaceIdentity {
            device: 1,
            inode: 2,
        }),
        defaults: SessionDefaults::default(),
        host_epoch_floor: 0,
        created_sequence: 0,
        last_sequence: 2,
        lineage: None,
        runs: vec![
            RunView {
                skill: None,
                failure: None,
                supervisor: Default::default(),
                workers: Default::default(),
                id: earlier_run_id,
                objective: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
                config: Some(config(CollaborationPolicy::Single, true)),
                agents: Vec::new(),
                assistant_message: None,
                status: RunStatus::Finished,
                accepted_sequence: 1,
                finished_sequence: Some(1),
                tools: vec![ToolView {
                    checkpoint: None,
                    active_time_reserved: false,
                    narrowing_source: None,
                    permission_verdict: None,
                    started_sequence: Some(0),
                    hooks: Vec::new(),
                    hook_sequence: None,
                    intent: write.intent.clone(),
                    observation: Some(write),
                    review_call_index: None,
                    finished_sequence: Some(1),
                }],
                tool_compaction_summary: None,
                tool_compaction_recent_files: Vec::new(),
                tool_compaction_covered_count: 0,
                tool_results_cleared_count: 0,
                definitions: Default::default(),
                display: Vec::new(),
                steering_notes: Vec::new(),
                lifecycle_hooks: Vec::new(),
                lifecycle_hook_sequence: None,
                provider_sequences: Default::default(),
                provider_fallbacks: Vec::new(),
                provider_retries: Vec::new(),
            },
            RunView {
                skill: None,
                failure: None,
                supervisor: Default::default(),
                workers: Default::default(),
                id: run_id,
                objective: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
                config: Some(config(CollaborationPolicy::Single, true)),
                agents: vec![AgentView {
                    tool_refusal: None,
                    tool_refusal_sequence: None,
                    profile: None,
                    assignment: None,
                    definition: None,
                    id: agent_run_id,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    summary: None,
                    result: None,
                    provider_calls: vec![record(
                        AgentPhase::RootPlan,
                        ProviderCallDisposition::PlanRecorded,
                        Some(0),
                    )],
                    steering_notes: Vec::new(),
                    grant: None,
                    tools: Vec::new(),
                    status: AgentStatus::Active,
                }],
                assistant_message: None,
                status: RunStatus::Active,
                accepted_sequence: 2,
                finished_sequence: None,
                tools: Vec::new(),
                tool_compaction_summary: None,
                tool_compaction_recent_files: Vec::new(),
                tool_compaction_covered_count: 0,
                tool_results_cleared_count: 0,
                definitions: Default::default(),
                display: Vec::new(),
                steering_notes: Vec::new(),
                lifecycle_hooks: Vec::new(),
                lifecycle_hook_sequence: None,
                provider_sequences: Default::default(),
                provider_fallbacks: Vec::new(),
                provider_retries: Vec::new(),
            },
        ],
        compactions: Vec::new(),
        goals: vec![GoalView {
            include_paths: Some(Vec::new()),
            id: goal_id,
            objective: GOAL_OBJECTIVE.into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget::default(),
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: GoalPhase::Research,
            autonomous: false,
            status: GoalStatus::Running,
            run_ids: vec![earlier_run_id, run_id],
            active_run: Some(run_id),
            plan_contract: None,
            success_state: crate::session::GoalSuccessState::NoContract,
            success_criterion_satisfied: false,
            awaiting_plan_contract: true,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_deadline: None,
            provider_calls: 2,
            unobserved_provider_calls: Vec::new(),
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            known_tokens: 0,
            known_cached_tokens: 0,
            token_usage_unknown: false,
            report: None,
            started_sequence: 1,
            finished_sequence: None,
            active_ceiling_ms: 0,
            continues: None,
            budget_closed: false,
            budget_refusal: None,
            run_start_usage: Default::default(),
        }],
        command_tool_probe: None,
        pending_provider_setup_edit: None,
        provider_setup_uncertain: false,
        environment_capture: None,
    };
    view.apply_committed(&EventEnvelope {
        sequence: 3,
        session_id,
        run_id: Some(run_id),
        agent_run_id: None,
        event: Event::GoalPlanContractRecorded {
            goal_id,
            run_id,
            contract,
        },
        created_at_ms: 0,
    })
    .expect("contract records");
    assert!(view.goals[0].success_criterion_satisfied);
    assert_eq!(
        view.goals[0].success_state,
        crate::session::GoalSuccessState::WorkspaceFile {
            sha256: Some(crate::validate::hex_digest(b"after")),
        }
    );
}

#[test]
fn unplanned_effects_obey_the_goal_phase() {
    for phase in [
        GoalPhase::Implementation,
        GoalPhase::Research,
        GoalPhase::Plan,
    ] {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        view.goals[0].phase = phase;
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let intent = child_intent(
            run,
            agent,
            ToolCall::Write {
                path: "src/new".into(),
                expected_digest: None,
                content: "written".into(),
            },
        );
        assert_eq!(
            apply(&mut view, tool_started(run, agent, &intent)).is_ok(),
            phase == GoalPhase::Implementation
        );
    }
}

#[test]
fn build_jobs_are_bound_to_goal_resources_before_tool_admission() {
    for (recorded, jobs, cpu, limit_cpu, admitted) in [
        (Some(2), 2, Some(1), 1, true),
        (Some(2), 1, Some(1), 1, false),
        (None, 1, Some(1), 1, false),
        (Some(2), 2, Some(8), 8, true),
        (Some(2), 2, Some(8), 1, false),
        (Some(2), 2, None, 1, false),
    ] {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        view.goals[0].resource_settings.build_jobs = recorded;
        view.goals[0].resource_settings.build_cpu_cores = cpu;
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let mut intent = read_intent(run, agent, "src/main.rs");
        intent.call = ToolCall::Command {
            program: "cargo".into(),
            args: vec!["build".into()],
            cwd: String::new(),
            resource_class: Some(crate::tools::ToolResourceClass::Build),
            timeout_ms: None,
            additional_reads: None,
        };
        intent.limits.resource_class = crate::tools::ToolResourceClass::Build;
        intent.limits.build_jobs = jobs;
        intent.limits.build_cpu_cores = limit_cpu;
        intent.limits.cache_bytes = 0;
        intent.limits.cache_candidate_bytes = 0;
        let result =
            crate::session::validate_tool_started(&view, &tool_proposed(run, agent, &intent));
        assert_eq!(result.is_ok(), admitted);
        if let Err(refusal) = result {
            assert!(refusal.valid());
        }
    }
}

#[tokio::test]
async fn committed_search_proposals_spend_the_session_allowance() {
    use crate::store::{StateRoot, Store};

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("state");
    let store = Store::open(StateRoot::admit(&path).unwrap()).unwrap();
    let session = SessionId::new();
    store
        .append(
            session,
            Event::SessionStarted {
                title: "Search allowance".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
            },
        )
        .await
        .unwrap();
    for _ in 0..2 {
        commit_search_run(&store, session).await;
    }
    let (goal, run, agent) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![goal_started(goal, GoalBudget::default())];
    events.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, true),
    ));
    events.extend([
        reserve_child_call(goal, run, agent),
        requesting_step(run, agent, 1),
    ]);
    for event in events {
        store.append(session, event).await.unwrap();
    }
    let mut intent = read_intent(run, agent, "src/main.rs");
    intent.call = ToolCall::WebSearch {
        query: "excess".into(),
    };
    let excess = tool_proposed(run, agent, &intent);
    let committed = store.load_session(session).await.unwrap();
    let live = SessionView::replay_open(session, &committed)
        .unwrap()
        .unwrap();
    assert!(matches!(
        crate::session::validate_primary_tool_scope(&live, run, &intent.call),
        Err(crate::session::ProposalRefusalCause::NotOffered)
    ));
    assert!(store.append(session, excess.clone()).await.is_err());
    store.close().await.unwrap();

    let store = Store::open(StateRoot::open_existing(&path).unwrap()).unwrap();
    let mut reopened = store.load_session(session).await.unwrap();
    assert_eq!(reopened, committed);
    let resumed = SessionView::replay_open(session, &reopened)
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::session::validate_tool_started(&resumed, &excess)
            .unwrap_err()
            .cause,
        crate::session::ProposalRefusalCause::NotOffered
    );
    let replay = SessionView::replay(session, &reopened).unwrap().unwrap();
    let proposals: Vec<_> = replay.runs.iter().flat_map(|run| &run.tools).collect();
    assert_eq!(proposals.len(), crate::tools::MAX_SESSION_SEARCHES);
    let ids: std::collections::HashSet<_> = proposals.iter().map(|tool| tool.intent.id).collect();
    assert_eq!(ids.len(), crate::tools::MAX_SESSION_SEARCHES);
    assert!(store.append(session, excess.clone()).await.is_err());
    reopened.push(crate::session::EventEnvelope {
        sequence: reopened.last().unwrap().sequence + 1,
        session_id: session,
        run_id: Some(run),
        agent_run_id: Some(agent),
        event: excess,
        created_at_ms: committed.last().unwrap().created_at_ms,
    });
    assert!(SessionView::replay(session, &reopened).is_err());
    store.close().await.unwrap();
}

async fn commit_search_run(store: &crate::Store, session: SessionId) {
    let (goal, run, agent) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![goal_started(goal, GoalBudget::default())];
    events.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, true),
    ));
    for index in 0..crate::tools::MAX_SESSION_SEARCHES / 2 {
        let mut intent = read_intent(run, agent, "src/main.rs");
        intent.call = ToolCall::WebSearch {
            query: format!("query {run} {index}"),
        };
        events.extend([
            reserve_child_call(goal, run, agent),
            requesting_step(run, agent, 1),
            tool_proposed(run, agent, &intent),
            tool_started(run, agent, &intent),
            tool_finished(run, agent, &intent, b"result"),
        ]);
    }
    events.extend(complete_run_and_goal(goal, run, agent));
    for event in events {
        store.append(session, event).await.unwrap();
    }
}

#[test]
fn child_web_proposals_require_the_pinned_grant() {
    for enabled in [false, true] {
        for call in [
            ToolCall::WebFetch {
                url: "https://example.com/".into(),
            },
            ToolCall::WebSearch { query: "q".into() },
        ] {
            let (mut view, goal, run, child) = research_session();
            view.runs[0]
                .config
                .as_mut()
                .unwrap()
                .tool_policy
                .as_mut()
                .unwrap()
                .child_web = enabled;
            apply(&mut view, reserve_child_call(goal, run, child)).unwrap();
            apply(
                &mut view,
                child_record(run, child, ProviderCallDisposition::ToolRequested),
            )
            .unwrap();
            let mut intent = read_intent(run, child, "src/main.rs");
            intent.call = call;
            intent.limits.network = crate::tools::NetworkGrant::Proxy;
            assert_eq!(
                apply(&mut view, tool_proposed(run, child, &intent)).is_ok(),
                enabled
            );
            if enabled {
                assert_eq!(view.runs[0].agents[1].tools.len(), 1);
            }
        }
    }
}
