use super::*;
use crate::provider::{AgentPhase, OutputTokenBound};
use crate::session::events::{ChildGrant, CollaborationPolicy, GoalSuccessCriterion};
use crate::tools::{
    CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolLimits, ToolObservation,
    ToolPolicyReceipt,
};

pub(super) const GOAL_OBJECTIVE: &str = "Goal";

pub(super) fn pending_plan_view(disposition: Option<ToolDisposition>) -> SessionView {
    let session_id = SessionId::new();
    let goal_id = GoalId::new();
    let run_id = RunId::new();
    let intent = EffectIntent {
        host: None,
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id,
        agent_run_id: AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(32),
        },
        limits: crate::tools::ToolLimits::default(),
        use_count: 1,
    };
    let tool = ToolView {
        checkpoint: None,
        active_time_reserved: false,
        narrowing_source: None,
        permission_verdict: None,
        started_sequence: Some(0),
        hooks: Vec::new(),
        hook_sequence: None,
        intent: intent.clone(),
        observation: disposition.map(|disposition| ToolObservation {
            spill: None,
            enforcement_cause: None,
            limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
                .then_some(crate::tools::LimitCause::Resource),
            intent,
            disposition,
            output: if disposition == ToolDisposition::Succeeded {
                "{}".into()
            } else {
                String::new()
            },
            attestation: None,
            approval_cause: None,
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        }),
        review_call_index: None,
        finished_sequence: disposition.map(|_| 2),
    };
    SessionView {
        usage: Default::default(),
        rewinds: Default::default(),
        id: session_id,
        title: "Interrupted Goal".into(),
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
            objective: "Inspect before planning".into(),
            images: Vec::new(),
            config: None,
            agents: Vec::new(),
            assistant_message: None,
            status: RunStatus::Interrupted,
            accepted_sequence: 1,
            finished_sequence: None,
            tools: vec![tool],
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
            objective: "Inspect before planning".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: GoalBudget::default(),
            resource_settings: crate::tools::ToolResourceSettings::default(),
            phase: GoalPhase::Research,
            autonomous: false,
            status: GoalStatus::Interrupted,
            run_ids: vec![run_id],
            active_run: None,
            plan_contract: None,
            success_state: crate::session::GoalSuccessState::NoContract,
            success_criterion_satisfied: false,
            awaiting_plan_contract: true,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_deadline: None,
            provider_calls: 1,
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
        pending_provider_setup_edit: None,
        provider_setup_uncertain: false,
    }
}

pub(super) fn config(policy: CollaborationPolicy, tools: bool) -> RunConfig {
    RunConfig {
        supervisor: tools.then(crate::supervisor::SupervisorPolicy::all_off),
        worker_policy: None,
        agent_definitions: None,
        roster: None,
        providers: None,
        provider: crate::ProviderRecord::parse("openai").expect("Provider record"),
        model: "gpt-5.4".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: None,
        chatgpt_provenance: None,
        output_token_bound: OutputTokenBound::ProviderEnforced,
        context_window_tokens: None,
        policy,
        output_token_cap: 4096,
        provider_concurrency: 1,
        workspace_device: 1,
        workspace_inode: 2,
        user_instruction_digest: None,
        instruction_sources: Vec::new(),
        launch_directory: String::new(),
        include_digests: Vec::new(),
        history_run_ids: Vec::new(),
        excluded_history_runs: 0,
        context_usage: None,
        compaction_event_sequence: None,
        compaction_content_digest: None,
        tool_policy: tools.then_some(Box::new(ToolPolicyReceipt {
            host_epoch: None,
            host_prefixes: Vec::new(),
            permissions_digest: None,
            contract_version: 1,
            config_digest: [1; 32],
            max_context_bytes: crate::tools::types::MAX_TOOL_CONTEXT_BYTES as u32,
            unguarded_rules_digest: None,
            unguarded: crate::tools::UnguardedGrant::HumanOnly,
            child_unguarded: crate::tools::UnguardedGrant::Denied,
        })),
    }
}

pub(super) fn record(
    phase: AgentPhase,
    disposition: ProviderCallDisposition,
    tokens: Option<u32>,
) -> ProviderCallRecord {
    ProviderCallRecord {
        rate_limits: None,
        delegation: (disposition == ProviderCallDisposition::Delegated).then(|| {
            vec![crate::session::worker::ChildAssignment::Researcher {
                objective: "Check the child result".into(),
                definition: None,
                profile: None,
            }]
        }),
        phase,
        disposition,
        response_id: None,
        input_tokens: tokens,
        output_tokens: tokens,
        wire_provenance: None,
        refusal: None,
        failure_reason: None,
        requested_tools: u8::from(disposition == ProviderCallDisposition::ToolRequested),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
    }
}

pub(super) fn journal(session_id: SessionId, mut events: Vec<Event>) -> Vec<EventEnvelope> {
    for i in 0..events.len() {
        if matches!(&events[i], Event::ProviderCallRecorded { record, .. } if record.disposition == ProviderCallDisposition::Delegated)
        {
            let assignments = events[i + 1..]
                .iter()
                .filter_map(|e| match e {
                    Event::AgentSpawned {
                        role: AgentRole::Child,
                        objective: Some(objective),
                        definition,
                        profile,
                        ..
                    } => Some(crate::session::worker::ChildAssignment::Researcher {
                        objective: objective.clone(),
                        definition: definition.clone(),
                        profile: profile.clone(),
                    }),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if !assignments.is_empty()
                && let Event::ProviderCallRecorded { record, .. } = &mut events[i]
            {
                record.delegation = Some(assignments);
            }
        }
    }
    events
        .into_iter()
        .zip(1..)
        .map(|(event, sequence)| {
            let (run_id, agent_run_id) = event.scope();
            EventEnvelope {
                sequence,
                session_id,
                run_id,
                agent_run_id,
                event,
                created_at_ms: 0,
            }
        })
        .collect()
}

pub(super) fn goal_started(goal_id: GoalId, budget: GoalBudget) -> Event {
    started_with(goal_id, budget, false, None)
}

pub(super) fn started_with(
    goal_id: GoalId,
    budget: GoalBudget,
    autonomous: bool,
    continues: Option<GoalId>,
) -> Event {
    Event::GoalStarted {
        include_paths: Some(Vec::new()),
        goal_id,
        objective: GOAL_OBJECTIVE.into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget,
        resource_settings: crate::tools::ToolResourceSettings::default(),
        phase: GoalPhase::Implementation,
        autonomous,
        continues,
    }
}

pub(super) fn start_run(
    goal_id: GoalId,
    run_id: RunId,
    agent_run_id: AgentRunId,
    config: RunConfig,
) -> Vec<Event> {
    let profile = config
        .roster
        .as_ref()
        .map(|roster| roster.providers.primary.clone());
    vec![
        Event::GoalRunLinked {
            goal_id,
            run_id,
            phase: GoalPhase::Implementation,
        },
        Event::MessageAccepted {
            skill: None,
            run_id,
            text: GOAL_OBJECTIVE.into(),
            images: Vec::new(),
        },
        Event::RunStarted {
            run_id,
            config,
            active_deadline_ms: None,
        },
        Event::AgentSpawned {
            profile,
            assignment: None,
            definition: None,
            run_id,
            agent_run_id,
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
            grant: None,
        },
    ]
}

pub(super) fn complete_run_and_goal(
    goal_id: GoalId,
    run_id: RunId,
    agent_run_id: AgentRunId,
) -> Vec<Event> {
    vec![
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms: 0,
        },
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                None,
            ),
        },
        Event::AgentFinished {
            worker: None,
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
        },
        Event::MessageCommitted {
            run_id,
            text: "done".into(),
        },
        Event::RunFinished {
            run_id,
            disposition: RunDisposition::Finished,
            failure: None,
            active_ms: 0,
        },
        Event::GoalFinished {
            goal_id,
            status: GoalStatus::Completed,
            active_ms: 0,
            report: GoalReport {
                summary: "done".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: false,
            budget_refusal: None,
        },
    ]
}

pub(super) fn cancel_run(run_id: RunId, agent_run_id: AgentRunId) -> Vec<Event> {
    vec![
        Event::AgentFinished {
            worker: None,
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Cancelled,
            summary: None,
            result: None,
        },
        Event::RunFinished {
            run_id,
            disposition: RunDisposition::Cancelled,
            failure: None,
            active_ms: 0,
        },
    ]
}

pub(super) fn session_with(session_id: SessionId, events: Vec<Event>) -> Vec<EventEnvelope> {
    let mut all = vec![Event::SessionStarted {
        title: "Goal".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
    }];
    all.extend(events);
    journal(session_id, all)
}

pub(super) fn delegated_session(known_tokens: u64) -> SessionView {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let contract = GoalPlanContract {
        mission: "Plan".into(),
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: None,
        paths: vec!["src/lib.rs".into()],
        commands: Vec::new(),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/lib.rs".into(),
            content: "ok".into(),
        },
        budget: GoalBudget {
            max_tokens: Some(1000),
            ..GoalBudget::default()
        },
    };
    SessionView {
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
            config: Some(config(
                CollaborationPolicy::Auto {
                    max_active_children: 1,
                },
                false,
            )),
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
                    ProviderCallDisposition::Delegated,
                    Some(0),
                )],
                steering_notes: Vec::new(),
                grant: None,
                tools: Vec::new(),
                status: AgentStatus::Active,
            }],
            assistant_message: None,
            status: RunStatus::Active,
            accepted_sequence: 1,
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
            phase: GoalPhase::Implementation,
            autonomous: false,
            status: GoalStatus::Running,
            run_ids: vec![run_id],
            active_run: Some(run_id),
            plan_contract: Some(contract),
            success_criterion_satisfied: false,
            awaiting_plan_contract: false,
            active_ms: 0,
            uncertain_active_ms: 0,
            active_deadline: None,
            compaction_call_reserved: None,
            uncertain_compaction_calls: 0,
            success_state: crate::session::GoalSuccessState::NoContract,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            provider_calls: 1,
            unobserved_provider_calls: Vec::new(),
            known_tokens,
            token_usage_unknown: false,
            report: None,
            started_sequence: 1,
            finished_sequence: None,
            known_cached_tokens: 0,
            active_ceiling_ms: 0,
            continues: None,
            budget_closed: false,
            budget_refusal: None,
            run_start_usage: Default::default(),
        }],
        pending_provider_setup_edit: None,
        provider_setup_uncertain: false,
    }
}

pub(super) fn apply_child_spawn(
    view: &mut SessionView,
    grant: ChildGrant,
) -> Result<(), ReplayError> {
    let run_id = view.runs[0].id;
    let event = Event::AgentSpawned {
        profile: None,
        assignment: None,
        definition: None,
        run_id,
        agent_run_id: AgentRunId::new(),
        role: AgentRole::Child,
        ordinal: 1,
        objective: Some("Check the child result".into()),
        grant: Some(grant),
    };
    let (run_id, agent_run_id) = event.scope();
    view.apply_committed(&EventEnvelope {
        sequence: 2,
        session_id: view.id,
        run_id,
        agent_run_id,
        event,
        created_at_ms: 0,
    })
}

pub(super) fn apply(view: &mut SessionView, event: Event) -> Result<(), ReplayError> {
    let (run_id, agent_run_id) = event.scope();
    let sequence = view.last_sequence + 1;
    view.apply_committed(&EventEnvelope {
        sequence,
        session_id: view.id,
        run_id,
        agent_run_id,
        event,
        created_at_ms: 0,
    })
}

pub(super) fn child_intent(
    run_id: RunId,
    agent_run_id: AgentRunId,
    call: ToolCall,
) -> EffectIntent {
    EffectIntent {
        host: None,
        host_command: call.is_unguarded().then(crate::tools::types::test_host_pin),
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id,
        agent_run_id,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call,
        limits: ToolLimits::default(),
        use_count: 1,
    }
}

pub(super) fn reserve_child_call(
    goal_id: GoalId,
    run_id: RunId,
    agent_run_id: AgentRunId,
) -> Event {
    Event::GoalProviderCallReserved {
        goal_id,
        run_id,
        agent_run_id,
        active_ceiling_ms: 0,
    }
}

pub(super) fn observation_for(intent: EffectIntent, content: &[u8]) -> ToolObservation {
    let output = if matches!(
        intent.call,
        ToolCall::Write { .. } | ToolCall::Edit { .. } | ToolCall::MultiEdit { .. }
    ) {
        let expected = match &intent.call {
            ToolCall::Write {
                expected_digest, ..
            } => expected_digest.clone(),
            ToolCall::Edit {
                expected_digest, ..
            }
            | ToolCall::MultiEdit {
                expected_digest, ..
            } => Some(expected_digest.clone()),
            _ => unreachable!(),
        };
        serde_json::to_string(&crate::session::rewind::FilePublicationResult {
            sha256: crate::validate::hex_digest(content),
            bytes: content.len() as u32,
            executable: false,
            before: expected.map(|sha256| crate::session::rewind::FileState {
                sha256,
                bytes: 0,
                executable: false,
            }),
        })
        .unwrap()
    } else if matches!(
        intent.call,
        ToolCall::Command { .. } | ToolCall::CommandUnguarded { .. }
    ) {
        let stdout = String::from_utf8(content.to_vec()).unwrap();
        serde_json::to_string(&crate::tools::types::CommandResult {
            selection: Some(Default::default()),
            exit_code: Some(0),
            stdout_total: crate::tools::OutputTotal {
                bytes: stdout.len() as u64,
                lines: stdout.lines().count() as u64,
            },
            stdout,
            stderr: String::new(),
            stderr_total: Default::default(),
            changeset: Default::default(),
        })
        .unwrap()
    } else if matches!(
        intent.call,
        ToolCall::GhPrList { .. } | ToolCall::GhPrView { .. }
    ) {
        String::from_utf8(content.to_vec()).unwrap()
    } else {
        serde_json::json!({ "sha256": crate::validate::hex_digest(content) }).to_string()
    };
    ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(crate::tools::Attestation::for_site(&intent)),
        intent,
        disposition: ToolDisposition::Succeeded,
        output,
        approval_cause: None,
        cache_update: CacheUpdate::NotApplicable,
    }
}

pub(super) fn primary_tool_session(planned: bool) -> (SessionView, GoalId, RunId, AgentRunId) {
    let mut view = delegated_session(0);
    view.runs[0].config = Some(config(CollaborationPolicy::Single, true));
    view.runs[0].agents[0].provider_calls.clear();
    if !planned {
        view.goals[0].plan_contract = None;
    }
    let (goal, run, agent) = (view.goals[0].id, view.runs[0].id, view.runs[0].agents[0].id);
    (view, goal, run, agent)
}

pub(super) fn read_intent(run: RunId, agent: AgentRunId, path: &str) -> EffectIntent {
    child_intent(
        run,
        agent,
        ToolCall::Read {
            path: path.into(),
            offset: None,
            limit: Some(1024),
        },
    )
}

pub(super) fn tool_proposed(run: RunId, agent: AgentRunId, intent: &EffectIntent) -> Event {
    Event::ToolProposed {
        run_id: run,
        agent_run_id: agent,
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
    }
}

pub(super) fn tool_started(run: RunId, agent: AgentRunId, intent: &EffectIntent) -> Event {
    Event::ToolStarted {
        verdict: None,
        run_id: run,
        agent_run_id: agent,
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
    }
}

pub(super) fn requesting_step(run: RunId, agent: AgentRunId, requested_tools: u8) -> Event {
    Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: agent,
        record: ProviderCallRecord {
            requested_tools,
            ..record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::ToolRequested,
                Some(0),
            )
        },
    }
}

pub(super) fn research_session() -> (SessionView, GoalId, RunId, AgentRunId) {
    let mut view = delegated_session(0);
    view.runs[0].config = Some(config(
        CollaborationPolicy::Auto {
            max_active_children: 1,
        },
        true,
    ));
    apply_child_spawn(
        &mut view,
        ChildGrant::ReadOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        },
    )
    .expect("research child spawn");
    let (goal, run, child) = (view.goals[0].id, view.runs[0].id, view.runs[0].agents[1].id);
    (view, goal, run, child)
}

pub(super) fn child_record(
    run_id: RunId,
    agent_run_id: AgentRunId,
    disposition: ProviderCallDisposition,
) -> Event {
    Event::ProviderCallRecorded {
        run_id,
        agent_run_id,
        record: record(AgentPhase::ChildWork, disposition, Some(0)),
    }
}

pub(super) fn rejected(view: &SessionView, event: Event) -> bool {
    apply(&mut view.clone(), event).is_err()
}

pub(super) fn tool_finished(
    run: RunId,
    agent: AgentRunId,
    intent: &EffectIntent,
    content: &[u8],
) -> Event {
    Event::ToolFinished {
        checkpoint: crate::session::checkpoint::publication(intent).then(|| {
            if matches!(
                intent.call,
                ToolCall::Write {
                    expected_digest: None,
                    ..
                }
            ) {
                crate::Checkpoint::not_needed()
            } else {
                crate::Checkpoint::skipped(crate::CheckpointReason::StoreUnavailable)
            }
        }),
        run_id: run,
        agent_run_id: agent,
        observation: observation_for(intent.clone(), content),
    }
}

pub(super) fn verdict_event(
    intent: &EffectIntent,
    mode: Option<crate::tools::permissions::PermissionMode>,
    decision: crate::tools::permissions::Decision,
    basis: crate::tools::permissions::PermissionSource,
    source: crate::tools::permissions::PermissionSource,
    approved: bool,
) -> Event {
    Event::ToolStarted {
        intent: intent.clone().into(),
        active_ceiling_ms: 0,
        run_id: intent.run_id,
        agent_run_id: intent.agent_run_id,
        verdict: Some(crate::tools::permissions::PermissionVerdict {
            host: (approved && intent.call.is_unguarded()).then_some(
                crate::tools::permissions::HostApproval {
                    rule: None,
                    epoch: uuid::Uuid::from_u128(0x01a15ecfa92b736a91a6560b8bc09501),
                    session: false,
                    source_intent: None,
                },
            ),
            intent_digest: intent.digest(),
            decision,
            mode,
            basis,
            source,
            approved,
            hook_restriction: None,
            hooks: 0,
            narrowed: false,
        }),
    }
}

pub(super) fn batch_start(
    intent: &EffectIntent,
    mode: Option<crate::tools::permissions::PermissionMode>,
    approved: bool,
) -> Event {
    use crate::tools::permissions::{Decision, PermissionSource};
    if mode.is_some() {
        verdict_event(
            intent,
            mode,
            Decision::Allow,
            PermissionSource::Mode,
            PermissionSource::Mode,
            approved,
        )
    } else {
        tool_started(intent.run_id, intent.agent_run_id, intent)
    }
}

pub(super) fn reserved_read_pair() -> (SessionView, [EffectIntent; 2]) {
    const ACTIVE_CEILING_MS: u64 = 1000;
    let (mut view, goal, run, agent) = primary_tool_session(false);
    let intents = [
        read_intent(run, agent, "src/lib.rs"),
        read_intent(run, agent, "src/main.rs"),
    ];
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 2)).unwrap();
    for intent in &intents {
        apply(&mut view, tool_proposed(run, agent, intent)).unwrap();
    }
    for intent in &intents {
        apply(
            &mut view,
            Event::ToolActiveTimeReserved {
                run_id: run,
                agent_run_id: agent,
                intent_digest: intent.digest(),
                active_ceiling_ms: ACTIVE_CEILING_MS,
            },
        )
        .unwrap();
        apply(&mut view, batch_start(intent, None, true)).unwrap();
    }
    (view, intents)
}

pub(super) fn assert_exhausted_crash_receipt(view: &SessionView, ceiling_ms: u64) {
    let mut view = view.clone();
    view.goals[0].budget.max_active_ms = Some(ceiling_ms);
    if let Some(contract) = &mut view.goals[0].plan_contract {
        contract.budget.max_active_ms = Some(ceiling_ms);
    }
    let goal = &view.goals[0];
    assert_eq!(goal.effective_budget().max_active_ms, Some(ceiling_ms));
    let usage = goal.resume_usage(&view);
    assert!(!usage.token_usage_unknown);
    assert_eq!((goal.active_ms, goal.uncertain_active_ms), (0, 0));
    assert_eq!(usage.active_ms, ceiling_ms);
    let mut refusal = goal
        .effective_budget()
        .refusal(usage, crate::BudgetAction::Resume)
        .expect("finite active budget refuses the recovered ceiling");
    let mut close = Event::GoalFinished {
        goal_id: goal.id,
        status: GoalStatus::Failed,
        active_ms: usage.active_ms,
        report: GoalReport {
            summary: "Out of budget.".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: goal.known_cached_tokens,
        },
        exhausted: true,
        budget_refusal: Some(refusal),
    };
    assert!(
        rejected(&view, close.clone()),
        "pending time is not observed"
    );
    refusal.charge_basis = crate::BudgetChargeBasis::Conservative;
    if let Event::GoalFinished { budget_refusal, .. } = &mut close {
        *budget_refusal = Some(refusal);
    }
    apply(&mut view, close.clone()).expect("crash ceiling closes conservatively");
    let goal = &view.goals[0];
    assert_eq!(
        (goal.active_ms, goal.uncertain_active_ms),
        (ceiling_ms, ceiling_ms)
    );
    assert_eq!(goal.active_ceiling_ms, 0);
    assert_eq!(goal.budget_refusal, Some(refusal));
    assert_eq!(goal.status, GoalStatus::Failed);
    assert!(rejected(&view, close), "a closed Goal cannot charge twice");
}

pub(super) fn recorded_supervisor_verdict(view: &SessionView, actor: AgentRunId) -> Event {
    Event::SupervisorVerdictRecorded {
        run_id: view.runs[0].id,
        agent_run_id: actor,
        verdict: crate::supervisor::SupervisorVerdict {
            subject_sequence: view.last_sequence,
            site: crate::supervisor::Site::ToolOutcome,
            evaluator: 1,
            mode: crate::supervisor::Mode::Shadow,
            signals: vec![crate::supervisor::Signal {
                signal: crate::supervisor::SignalKind::Loop,
                level: crate::supervisor::Level::Notice,
                would_response: None,
                features: crate::supervisor::Features::Loop(Default::default()),
                evidence: vec![view.last_sequence],
            }],
            capped: false,
        },
    }
}

pub(super) fn compacting_session() -> (SessionView, GoalId, RunId) {
    compacting_session_with_last_tool(ToolCall::Read {
        path: "src/lib.rs".into(),
        offset: None,
        limit: Some(1024),
    })
}

pub(super) fn compacting_session_with_last_tool(call: ToolCall) -> (SessionView, GoalId, RunId) {
    let (mut view, goal, run, agent) = primary_tool_session(true);
    let (first, second) = (
        read_intent(run, agent, "src/lib.rs"),
        child_intent(run, agent, call),
    );
    apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
    apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
    apply(&mut view, tool_proposed(run, agent, &first)).expect("first request is proposed");
    apply(&mut view, tool_proposed(run, agent, &second)).expect("second request is proposed");
    apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
    apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
    apply(&mut view, tool_started(run, agent, &second)).expect("second request starts");
    apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second observed");
    apply(
        &mut view,
        Event::GoalCompactionCallReserved {
            goal_id: goal,
            run_id: run,
            active_ceiling_ms: 0,
        },
    )
    .expect("compaction call reserved");
    (view, goal, run)
}

pub(super) fn covering_compaction(
    view: &SessionView,
    goal: GoalId,
    run: RunId,
    count: u32,
) -> Event {
    let covered = &view.runs[0].tools[count as usize - 1];
    Event::GoalContextCompacted {
        goal_id: goal,
        run_id: run,
        covered_tool_count: count,
        record: crate::session::events::CompactionRecord {
            rate_limits: None,
            covered_run_id: run,
            covered_sequence: covered.finished_sequence.expect("observed Tool"),
            source_digest: [0; 32],
            recent_files: Vec::new(),
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version: crate::session::COMPACTION_COMPILER_VERSION,
            source_bytes: 1,
            source_tokens_estimate: 1,
            input_tokens: None,
            output_tokens: None,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            response_id: None,
            wire_provenance: None,
            chatgpt_provenance: None,
            output_token_bound: OutputTokenBound::ProviderEnforced,
            status: crate::session::events::CompactionStatus::Failed {
                reason: crate::session::events::CompactionFailure::ProviderUnavailable,
            },
        },
    }
}
