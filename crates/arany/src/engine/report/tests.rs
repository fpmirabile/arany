use super::*;
use crate::session::{
    AgentRole, AgentRunId, AgentStatus, AgentView, GoalBudget, GoalId, GoalPhase, GoalStatus,
    GoalView, RunId, RunStatus, SessionId, ToolView, WorkspaceIdentity,
};
use crate::tools::{
    Attestation, CacheUpdate, EffectIntent, GuardReceipt, ToolCall, ToolDisposition, ToolLimits,
};
use uuid::Uuid;

#[test]
fn bounded_report_preserves_usage_uncertainty_ahead_of_many_tool_facts() {
    let session_id = SessionId::new();
    let goal_id = GoalId::new();
    let run_id = RunId::new();
    let command_intent = EffectIntent {
        target: crate::session::worker::EffectTarget::Workspace,
        id: Uuid::now_v7(),
        run_id,
        agent_run_id: AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Command {
            program: "cargo".into(),
            args: vec!["check".into()],
            cwd: ".".into(),
        },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let command_observation = crate::tools::ToolObservation {
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(Attestation::Guard(GuardReceipt {
            contract_version: 1,
            intent_digest: command_intent.digest(),
            enforcement_digest: command_intent.enforcement_digest,
            limits: command_intent.limits.clone(),
        })),
        intent: command_intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: serde_json::json!({
            "exit_code": 0,
            "stdout": "",
            "stderr": "",
            "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset": {"files": []}
        })
        .to_string(),
        cache_update: CacheUpdate::Failed,
    };
    let mut tools = vec![ToolView {
        checkpoint: None,
        intent: command_intent,
        observation: Some(command_observation),
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    }];
    tools.extend((0..15).map(|index| {
        let intent = EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: Uuid::now_v7(),
            run_id,
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Read {
                path: format!("src/{index}-{}", "x".repeat(480)),
                offset: None,
                limit: None,
            },
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let observation = crate::tools::ToolObservation {
            enforcement_cause: None,
            limit_cause: None,
            attestation: Some(Attestation::for_site(&intent)),
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: serde_json::json!({
                "sha256": crate::tools::types::hex_digest(b"evidence")
            })
            .to_string(),
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        };
        ToolView {
            checkpoint: None,
            intent,
            observation: Some(observation),
            review_call_index: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        }
    }));
    let run = RunView {
        skill: None,
        failure: None,
        supervisor: Default::default(),
        workers: Default::default(),
        id: run_id,
        objective: "Report bounded evidence".into(),
        images: Vec::new(),
        config: None,
        agents: vec![AgentView {
            profile: None,
            assignment: None,
            definition: None,
            id: AgentRunId::new(),
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
            summary: Some("A concise result".into()),
            result: Some("Done".into()),
            provider_calls: Vec::new(),
            steering_notes: Vec::new(),
            grant: None,
            tools: Vec::new(),
            status: AgentStatus::Finished,
        }],
        assistant_message: Some("Done".into()),
        status: RunStatus::Finished,
        accepted_sequence: 1,
        finished_sequence: Some(2),
        tools,
        tool_compaction_summary: None,
        tool_compaction_covered_count: 0,
        tool_results_cleared_count: 0,
        definitions: Default::default(),
        display: Vec::new(),
        steering_notes: Vec::new(),
        lifecycle_hooks: Vec::new(),
        lifecycle_hook_sequence: None,
        provider_sequences: Default::default(),
        provider_retries: Vec::new(),
        provider_fallbacks: (0..64)
            .map(|_| {
                (
                    AgentRunId::new(),
                    crate::provider::ProviderFallback {
                        rate_limits: None,
                        role: crate::provider::ProviderRole::Child,
                        call_index: 0,
                        failure: crate::session::ProviderCallDisposition::Unavailable,
                        from: crate::provider::ProviderIdentity::new(
                            "p".repeat(128),
                            "m".repeat(128),
                            crate::provider::Provenance::native(),
                        ),
                        to: crate::provider::ProviderIdentity::new(
                            "f".repeat(128),
                            "n".repeat(128),
                            crate::provider::Provenance::native(),
                        ),
                    },
                )
            })
            .collect(),
    };
    let goal = GoalView {
        id: goal_id,
        objective: "Report bounded evidence".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget: GoalBudget::default(),
        phase: GoalPhase::Implementation,
        autonomous: false,
        resource_settings: crate::tools::ToolResourceSettings::default(),
        status: GoalStatus::Running,
        run_ids: vec![run_id],
        active_run: None,
        plan_contract: None,
        success_state: crate::session::GoalSuccessState::NoContract,
        success_criterion_satisfied: false,
        awaiting_plan_contract: false,
        active_ms: 0,
        uncertain_active_ms: 0,
        active_deadline: None,
        compaction_call_reserved: None,
        uncertain_compaction_calls: 0,
        provider_calls: 1,
        unobserved_provider_calls: Vec::new(),
        known_tokens: 0,
        token_usage_unknown: true,
        report: None,
        started_sequence: 0,
        finished_sequence: None,
        known_cached_tokens: 0,
        active_ceiling_ms: 0,
        continues: None,
        budget_closed: false,
    };
    let view = crate::session::SessionView {
        usage: Default::default(),
        rewinds: Default::default(),
        id: session_id,
        title: "Report bounded evidence".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some(WorkspaceIdentity {
            device: 1,
            inode: 2,
        }),
        defaults: crate::session::SessionDefaults::default(),
        created_sequence: 0,
        last_sequence: 2,
        lineage: None,
        runs: vec![run.clone()],
        compactions: Vec::new(),
        goals: vec![goal.clone()],
    };

    let report = goal_report(&view, &run, &goal);
    assert_eq!(
        report
            .unresolved
            .iter()
            .filter(|item| item.starts_with("Provider fallback"))
            .count(),
        8
    );
    assert!(report.evidence.len() < 17, "report retains its byte bound");
    assert!(
        report.evidence.len() > 1,
        "report keeps canonical tool evidence"
    );
    assert!(
        report
            .evidence
            .iter()
            .any(|item| item == "Command cargo exited 0."),
        "successful Command evidence survives a cache update failure"
    );
    assert!(report.unresolved.iter().any(|item| {
        item.contains("build cache update failed") && item.contains("previous cache was kept")
    }));
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("token usage is incomplete"))
    );
    assert!(serde_json::to_vec(&report).unwrap().len() <= crate::session::MAX_GOAL_REPORT_BYTES);

    let mut checkpoint_view = view.clone();
    let tool = &mut checkpoint_view.runs[0].tools[1];
    tool.intent.call = ToolCall::Write {
        path: "src/file.rs".into(),
        expected_digest: Some(crate::tools::types::hex_digest(b"before")),
        content: "after".into(),
    };
    let observation = tool.observation.as_mut().unwrap();
    observation.intent = tool.intent.clone();
    observation.attestation = Some(Attestation::for_site(&tool.intent));
    tool.checkpoint = Some(crate::Checkpoint {
        status: crate::CheckpointStatus::Skipped,
        reason: Some(crate::CheckpointReason::Quota),
        pruned_before_run: Some(run_id),
    });
    let checkpoint_report = goal_report(&checkpoint_view, &checkpoint_view.runs[0], &goal);
    assert!(
        checkpoint_report
            .unresolved
            .iter()
            .any(|fact| fact == "This change cannot be rewound (checkpoint storage full)")
    );
    assert!(checkpoint_report.unresolved.iter().any(|fact| {
        fact.starts_with("Older checkpoints pruned; code restore unavailable before Run")
    }));
    assert!(
        serde_json::to_vec(&checkpoint_report).unwrap().len()
            <= crate::session::MAX_GOAL_REPORT_BYTES
    );

    let mut late_run = run.clone();
    let late_failed_command = late_run.tools.remove(0);
    let failed_cache_observation = late_failed_command
        .observation
        .as_ref()
        .expect("attested Command result");
    assert!(failed_cache_observation.valid());
    assert!(failed_cache_observation.attested_for(&late_failed_command.intent));
    late_run.tools.truncate(10);
    let mut late_rejected_command = late_failed_command.clone();
    late_run.tools.push(late_failed_command);
    late_rejected_command.intent.id = Uuid::now_v7();
    let rejected_digest = late_rejected_command.intent.digest();
    let rejected_observation = late_rejected_command
        .observation
        .as_mut()
        .expect("attested Command result");
    rejected_observation.intent = late_rejected_command.intent.clone();
    let Some(Attestation::Guard(receipt)) = &mut rejected_observation.attestation else {
        panic!("Guard receipt");
    };
    receipt.intent_digest = rejected_digest;
    rejected_observation.cache_update = CacheUpdate::Rejected;
    assert!(rejected_observation.valid());
    assert!(rejected_observation.attested_for(&late_rejected_command.intent));
    late_run.tools.push(late_rejected_command);
    let uncertain_intent = EffectIntent {
        target: crate::session::worker::EffectTarget::Workspace,
        id: Uuid::now_v7(),
        run_id,
        agent_run_id: late_run.agents[0].id,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Command {
            program: "cargo".into(),
            args: vec!["test".into()],
            cwd: ".".into(),
        },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let uncertain_observation = crate::tools::ToolObservation {
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(Attestation::Guard(GuardReceipt {
            contract_version: 1,
            intent_digest: uncertain_intent.digest(),
            enforcement_digest: uncertain_intent.enforcement_digest,
            limits: uncertain_intent.limits.clone(),
        })),
        intent: uncertain_intent.clone(),
        disposition: ToolDisposition::Uncertain,
        output: run.tools[0]
            .observation
            .as_ref()
            .expect("valid Command result")
            .output
            .clone(),
        cache_update: CacheUpdate::Uncertain,
    };
    assert!(uncertain_observation.valid());
    assert!(uncertain_observation.attested_for(&uncertain_intent));
    late_run.tools.push(ToolView {
        checkpoint: None,
        intent: uncertain_intent.clone(),
        observation: Some(uncertain_observation),
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    });
    assert_eq!(late_run.tools.len(), 13);
    late_run.agents[0].summary = Some("\u{1}".repeat(2_048));
    let mut late_view = view.clone();
    late_view.runs = vec![late_run.clone()];
    let late_report = goal_report(&late_view, &late_run, &goal);
    let late_report_bytes = serde_json::to_vec(&late_report).unwrap();
    assert!(
        late_report_bytes.len() > crate::session::MAX_GOAL_REPORT_BYTES * 3 / 4,
        "late cache outcomes exercise a near-capacity report"
    );
    assert!(late_report_bytes.len() <= crate::session::MAX_GOAL_REPORT_BYTES);
    assert!(late_report.summary.len() <= 2_048);
    assert!(
        late_report.summary.len() < 1_800,
        "JSON-escaped controls consume serialized report capacity"
    );
    assert!(
        late_report
            .summary
            .chars()
            .all(|character| character == '\u{1}')
    );
    late_report
        .validate()
        .expect("bounded report remains valid");
    assert!(late_report.unresolved.iter().any(|item| {
        item.contains("build cache update was rejected") && item.contains("previous cache was kept")
    }));
    assert!(late_report.unresolved.iter().any(|item| {
        item.contains("build cache update failed") && item.contains("previous cache was kept")
    }));
    assert!(late_report.unresolved.iter().any(|item| {
        item.contains("uncertain build cache publication") && item.contains("Session history")
    }));
    assert!(
        late_report
            .unresolved
            .iter()
            .any(|item| item == GOAL_REPORT_OMISSION),
        "omitted lower-priority facts remain visible: {late_report:?}"
    );
    assert!(
        late_report
            .unresolved
            .iter()
            .any(|item| { item.contains("primary summary was shortened") })
    );

    let mut byte_run = run.clone();
    byte_run.tools.truncate(13);
    let failed_intent = EffectIntent {
        target: crate::session::worker::EffectTarget::Workspace,
        id: Uuid::now_v7(),
        run_id,
        agent_run_id: byte_run.agents[0].id,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Read {
            path: "src/report-failure".into(),
            offset: None,
            limit: None,
        },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let failed_observation = crate::tools::ToolObservation {
        enforcement_cause: None,
        limit_cause: None,
        intent: failed_intent.clone(),
        disposition: ToolDisposition::Denied,
        output: String::new(),
        attestation: None,
        cache_update: CacheUpdate::NotApplicable,
    };
    assert!(failed_observation.valid());
    assert!(!failed_observation.attested_for(&failed_intent));
    assert!(!failed_observation.stops_run());
    byte_run.tools.push(ToolView {
        checkpoint: None,
        observation: Some(failed_observation),
        intent: failed_intent,
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    });
    assert_eq!(byte_run.tools.len(), 14);
    byte_run.agents[0].summary = Some("x".repeat(1_800));
    let mut byte_view = view.clone();
    byte_view.runs = vec![byte_run.clone()];
    let byte_report = goal_report(&byte_view, &byte_run, &goal);
    byte_report
        .validate()
        .expect("byte-boundary Goal report remains valid");
    assert!(byte_report.unresolved.len() < 32);
    assert!(byte_report.unresolved.iter().any(|item| {
        item.contains("read src/report-failure failed")
            && item.contains("effect was not recorded as successful")
    }));
    assert!(
        byte_report
            .unresolved
            .iter()
            .any(|item| item == GOAL_REPORT_OMISSION)
    );
    assert!(byte_report.evidence.len() < 13);
    assert!(
        byte_report
            .evidence
            .iter()
            .any(|item| item == "Command cargo exited 0.")
    );
    assert!(
        serde_json::to_vec(&byte_report).unwrap().len() <= crate::session::MAX_GOAL_REPORT_BYTES
    );

    let mut saturation_runs = Vec::with_capacity(3);
    let mut saturation_run_ids = Vec::with_capacity(3);
    for (run_index, denied_count) in [10, 11, 12].into_iter().enumerate() {
        let run_id = RunId::new();
        let agent_run_id = AgentRunId::new();
        saturation_run_ids.push(run_id);
        let mut tools = Vec::with_capacity(denied_count + usize::from(run_index == 0));
        if run_index == 0 {
            let mut command = run.tools[0].clone();
            command.intent.id = Uuid::now_v7();
            command.intent.run_id = run_id;
            command.intent.agent_run_id = agent_run_id;
            let digest = command.intent.digest();
            let observation = command.observation.as_mut().expect("successful Command");
            observation.intent = command.intent.clone();
            let Some(Attestation::Guard(receipt)) = &mut observation.attestation else {
                panic!("Command Guard receipt");
            };
            receipt.intent_digest = digest;
            observation.cache_update = CacheUpdate::NotApplicable;
            assert!(observation.valid());
            assert!(observation.attested_for(&command.intent));
            tools.push(command);
        }
        for failure_index in 0..denied_count {
            let intent = EffectIntent {
                target: crate::session::worker::EffectTarget::Workspace,
                id: Uuid::now_v7(),
                run_id,
                agent_run_id,
                policy_digest: [1; 32],
                enforcement_digest: [2; 32],
                workspace_device: 1,
                workspace_inode: 2,
                call: ToolCall::Read {
                    path: format!("src/{run_index}-{failure_index}"),
                    offset: None,
                    limit: None,
                },
                limits: ToolLimits::default(),
                expires_at_ms: 1,
                use_count: 1,
            };
            let observation = crate::tools::ToolObservation {
                enforcement_cause: None,
                limit_cause: None,
                intent: intent.clone(),
                disposition: ToolDisposition::Denied,
                output: String::new(),
                attestation: None,
                cache_update: CacheUpdate::NotApplicable,
            };
            assert!(observation.valid());
            assert!(!observation.attested_for(&intent));
            tools.push(ToolView {
                checkpoint: None,
                observation: Some(observation),
                intent,
                review_call_index: None,
                permission_verdict: None,
                hooks: Vec::new(),
                hook_sequence: None,
                finished_sequence: None,
            });
        }
        let mut saturation_run = run.clone();
        saturation_run.id = run_id;
        saturation_run.agents[0].id = agent_run_id;
        saturation_run.tools = tools;
        saturation_runs.push(saturation_run);
    }
    let mut saturation_goal = goal.clone();
    saturation_goal.run_ids = saturation_run_ids;
    let mut saturation_view = view.clone();
    saturation_view.runs = saturation_runs;
    assert_eq!(
        saturation_view
            .runs
            .iter()
            .flat_map(|task_run| &task_run.tools)
            .filter(|tool| {
                tool.observation
                    .as_ref()
                    .is_some_and(|observation| observation.disposition == ToolDisposition::Denied)
            })
            .count(),
        33
    );
    let saturation_report =
        goal_report(&saturation_view, &saturation_view.runs[0], &saturation_goal);
    saturation_report
        .validate()
        .expect("item-boundary Goal report remains valid");
    assert_eq!(saturation_report.unresolved.len(), 32);
    assert!(
        saturation_report
            .unresolved
            .iter()
            .any(|item| item == GOAL_REPORT_OMISSION)
    );
    assert!(
        saturation_report
            .evidence
            .iter()
            .any(|item| item == "Command cargo exited 0.")
    );
    assert!(
        serde_json::to_vec(&saturation_report).unwrap().len()
            <= crate::session::MAX_GOAL_REPORT_BYTES
    );

    let mut timeout_intent = run.tools[0].intent.clone();
    timeout_intent.id = Uuid::now_v7();
    timeout_intent.limits.runtime_ms = 2_000;
    timeout_intent.limits.cache_bytes = 0;
    timeout_intent.limits.cache_candidate_bytes = 0;
    timeout_intent.limits.cache_transfer_ms = 0;
    let timeout_observation = crate::tools::ToolObservation {
        enforcement_cause: None,
        limit_cause: Some(crate::tools::LimitCause::RequestedTime),
        intent: timeout_intent.clone(),
        disposition: ToolDisposition::Limit,
        output: crate::tools::types::command_timeout_limit_message(2_000, 1_000),
        attestation: None,
        cache_update: CacheUpdate::NotApplicable,
    };
    assert!(timeout_observation.valid());
    let mut timeout_run = run.clone();
    timeout_run.tools = vec![ToolView {
        checkpoint: None,
        intent: timeout_intent,
        observation: Some(timeout_observation),
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    }];
    let mut timeout_goal = goal.clone();
    timeout_goal.resource_settings.default_command_timeout_ms = 1_000;
    timeout_goal.resource_settings.max_command_timeout_ms = 1_000;
    timeout_goal.plan_contract = Some(crate::session::GoalPlanContract {
        mission: "Run the configured command within pinned limits".into(),
        paths: Vec::new(),
        commands: vec!["cargo".into()],
        resource_class: crate::tools::ToolResourceClass::Default,
        command_timeout_ms: Some(2_000),
        skills: Vec::new(),
        mcp_tools: Vec::new(),
        success_criterion: crate::session::GoalSuccessCriterion::WorkspaceFileContent {
            path: "src/main.rs".into(),
            content: "expected".into(),
        },
        budget: GoalBudget::default(),
    });
    let mut timeout_view = view.clone();
    timeout_view.runs = vec![timeout_run.clone()];
    timeout_view.goals = vec![timeout_goal.clone()];
    let timeout_report = goal_report(&timeout_view, &timeout_run, &timeout_goal);
    assert!(timeout_report.unresolved.iter().any(|item| {
        item.contains("2000 ms")
            && item.contains("pinned max 1000 ms")
            && item.contains("Lower request or change /settings")
    }));
}

#[test]
fn read_evidence_names_the_site_that_attested_it() {
    let call = ToolCall::Read {
        path: "src/lib.rs".into(),
        offset: None,
        limit: None,
    };
    assert_eq!(
        goal_report_evidence(
            &call,
            "{}",
            Some(&Attestation::for_site(&read_intent(&call)))
        ),
        "The Engine attested a read of src/lib.rs."
    );
    assert_eq!(
        goal_report_evidence(&call, "{}", None),
        "Guard attested a read of src/lib.rs."
    );
}

fn read_intent(call: &ToolCall) -> EffectIntent {
    EffectIntent {
        target: crate::session::worker::EffectTarget::Workspace,
        id: Uuid::now_v7(),
        run_id: RunId::new(),
        agent_run_id: AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: call.clone(),
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    }
}

#[test]
fn unguarded_success_is_evidence_that_names_the_host_run_and_survives_omission() {
    let run_id = RunId::new();
    let call = ToolCall::CommandUnguarded {
        program: "cargo".into(),
        args: vec!["check".into()],
        cwd: String::new(),
        justification: "Run the failed command once".into(),
    };
    let mut intent = read_intent(&call);
    intent.run_id = run_id;
    let observation = crate::tools::ToolObservation {
        enforcement_cause: None,
        limit_cause: None,
        attestation: Some(Attestation::for_site(&intent)),
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: serde_json::json!({"exit_code": 0, "stdout": "", "stderr": "", "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset": {"files": []}}).to_string(),
        cache_update: CacheUpdate::NotApplicable,
    };
    assert_eq!(
        goal_report_evidence(&call, &observation.output, observation.attestation.as_ref()),
        "An unguarded host run attested Unguarded command cargo."
    );
    let tool = ToolView {
        checkpoint: None,
        intent,
        observation: Some(observation),
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    };
    assert!(tool.ran_unguarded());
    let goal_id = GoalId::new();
    let run = RunView {
        skill: None,
        failure: None,
        supervisor: Default::default(),
        workers: Default::default(),
        id: run_id,
        objective: "Report".into(),
        images: Vec::new(),
        config: None,
        agents: Vec::new(),
        assistant_message: Some("Done".into()),
        status: RunStatus::Finished,
        accepted_sequence: 1,
        finished_sequence: Some(2),
        tools: vec![tool; 100],
        tool_compaction_summary: None,
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
    };
    let goal = GoalView {
        continues: None,
        budget_closed: false,
        id: goal_id,
        objective: "Report".into(),
        workspace_identity: WorkspaceIdentity {
            device: 1,
            inode: 2,
        },
        budget: GoalBudget::default(),
        phase: GoalPhase::Implementation,
        autonomous: false,
        resource_settings: crate::tools::ToolResourceSettings::default(),
        status: GoalStatus::Running,
        run_ids: vec![run_id],
        active_run: None,
        plan_contract: None,
        success_state: crate::session::GoalSuccessState::NoContract,
        success_criterion_satisfied: false,
        awaiting_plan_contract: false,
        active_ms: 0,
        uncertain_active_ms: 0,
        active_deadline: None,
        compaction_call_reserved: None,
        uncertain_compaction_calls: 0,
        provider_calls: 1,
        unobserved_provider_calls: Vec::new(),
        known_tokens: 0,
        token_usage_unknown: false,
        report: None,
        started_sequence: 0,
        finished_sequence: None,
        known_cached_tokens: 0,
        active_ceiling_ms: 0,
    };
    let view = crate::session::SessionView {
        usage: Default::default(),
        rewinds: Default::default(),
        id: SessionId::new(),
        title: "Report".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some(WorkspaceIdentity {
            device: 1,
            inode: 2,
        }),
        defaults: crate::session::SessionDefaults::default(),
        created_sequence: 0,
        last_sequence: 2,
        lineage: None,
        runs: vec![run.clone()],
        compactions: Vec::new(),
        goals: vec![goal.clone()],
    };
    let report = goal_report(&view, &run, &goal);
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("100 unguarded host run(s) executed outside the Guard"))
    );
    assert!(
        report
            .evidence
            .iter()
            .any(|item| item.starts_with("An unguarded host run attested"))
    );
}

#[test]
fn worker_recovery_report_preserves_exact_ids_disposal_and_unverified_publication() {
    let f = crate::session::worker_protocol_fixture();
    let events = f.envelopes();
    for end in 1..=events.len() {
        let view = crate::session::SessionView::replay(f.session, &events[..end])
            .unwrap()
            .unwrap();
        let Some(run) = view.runs.last() else {
            continue;
        };
        let goal = view.goal(f.goal).unwrap();
        let report = goal_report(&view, run, goal);
        for worker in run.worker_history() {
            let text = report
                .unresolved
                .iter()
                .chain(&report.changes)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains(&worker.assignment_id.to_string()),
                "prefix {end} lost worker ID"
            );
            if worker.publication == crate::worker::WorkerPublication::Confirmed && !worker.verified
            {
                assert!(
                    text.contains(
                        "confirmed Workspace publication needs fresh primary verification"
                    ),
                    "prefix {end}"
                );
            }
            if worker.private_outcome_unknown {
                assert!(
                    text.contains("private_outcome_unknown=true"),
                    "prefix {end}"
                );
            }
        }
        assert!(
            serde_json::to_vec(&report).unwrap().len() <= crate::session::MAX_GOAL_REPORT_BYTES
        );
    }
}

#[test]
fn many_worker_lines_never_displace_unguarded_or_unknown_facts() {
    let f = crate::session::worker_protocol_fixture();
    let events = f.envelopes();
    let mut view = crate::session::SessionView::replay(f.session, &events)
        .unwrap()
        .unwrap();
    let run = view.runs.last_mut().unwrap();
    let template = run
        .agents
        .iter()
        .find(|a| a.assignment.is_some())
        .unwrap()
        .clone();
    for _ in 0..40 {
        let mut agent = template.clone();
        agent.id = AgentRunId::new();
        run.agents.push(agent);
    }
    let call = ToolCall::CommandUnguarded {
        program: "cargo".into(),
        args: vec!["check".into()],
        cwd: String::new(),
        justification: "Run the failed command once".into(),
    };
    let mut intent = read_intent(&call);
    intent.run_id = run.id;
    run.tools.push(ToolView {
        checkpoint: None,
        observation: Some(crate::tools::ToolObservation {
            enforcement_cause: None,
            limit_cause: None,
            attestation: Some(Attestation::for_site(&intent)),
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: serde_json::json!({"exit_code": 0, "stdout": "", "stderr": "", "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset": {"files": []}}).to_string(),
            cache_update: CacheUpdate::NotApplicable,
        }),
        intent,
        review_call_index: None,
        permission_verdict: None,
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    });
    let run = view.runs.last().unwrap();
    let goal = view.goal(f.goal).unwrap();
    assert!(run.worker_history().len() > 40);
    let report = goal_report(&view, run, goal);
    assert!(
        report
            .unresolved
            .iter()
            .any(|item| item.contains("unguarded host run(s) executed outside the Guard"))
    );
    assert!(serde_json::to_vec(&report).unwrap().len() <= crate::session::MAX_GOAL_REPORT_BYTES);
}

#[test]
fn unresolved_facts_always_leave_room_for_the_omission_marker() {
    let mut report = GoalReport {
        summary: "Summary".into(),
        cached_input_tokens: 0,
        changes: Vec::new(),
        evidence: Vec::new(),
        unresolved: Vec::new(),
    };
    let mut omitted = false;
    for index in 0..64 {
        push_goal_report_item(
            &mut report,
            GoalReportSection::Unresolved,
            &format!("fact {index} {}", "x".repeat(500)),
            &mut omitted,
        );
    }
    assert!(omitted);
    assert!(goal_report_fits_with_omission(&mut report));
}

#[test]
fn rewind_facts_remain_in_the_bounded_goal_report_after_evidence_is_invalidated() {
    let f = crate::session::worker_protocol_fixture();
    let mut view = crate::session::SessionView::replay(f.session, &f.envelopes())
        .unwrap()
        .unwrap();
    let start = crate::session::rewind_started_fixture(&view, view.rewinds.checkpoints[0].clone());
    for finish in [false, true] {
        let event = if finish {
            crate::session::rewind_finished_fixture(
                &view,
                crate::session::rewind::RewindOutcome::Restored,
            )
        } else {
            start.clone()
        };
        view.apply_committed(&crate::session::EventEnvelope {
            sequence: view.last_sequence + 1,
            session_id: view.id,
            run_id: None,
            agent_run_id: None,
            event,
            created_at_ms: 0,
        })
        .unwrap();
    }
    let goal = view.goal(f.goal).unwrap();
    let report = super::goal_report(&view, &view.runs[0], goal);
    assert!(
        report
            .unresolved
            .iter()
            .any(|line| line.contains("Workspace rewind") && line.contains("\"src/a\""))
    );
    assert!(serde_json::to_vec(&report).unwrap().len() <= crate::session::MAX_GOAL_REPORT_BYTES);
    assert!(!goal.success_criterion_satisfied);
    assert!(!view.runs[0].worker_history()[0].verified);

    let template = view.rewinds.operations[0].clone();
    for index in 1..10 {
        let mut operation = template.clone();
        operation.started.rewind_id = Uuid::now_v7();
        operation.started_sequence = view.last_sequence + index;
        operation.finished = None;
        operation.finished_sequence = None;
        view.rewinds.operations.push(operation);
    }
    view.last_sequence += 10;
    let facts = crate::session::rewind::history_facts(&view, 0, view.last_sequence).unwrap();
    assert!(facts.len() > 512);
    let report = goal_report(&view, &view.runs[0], view.goal(f.goal).unwrap());
    let newest = view.rewinds.operations.last().unwrap().history_fact();
    assert_eq!(
        report.unresolved[..8],
        view.rewinds
            .operations
            .iter()
            .rev()
            .take(8)
            .map(|operation| operation.history_fact())
            .collect::<Vec<_>>()
    );
    assert!(report.unresolved[8].starts_with("Older Workspace rewind facts omitted"));
    report.validate().unwrap();

    let mut later_goal = view.goal(f.goal).unwrap().clone();
    later_goal.id = GoalId::new();
    later_goal.started_sequence = view.last_sequence + 1;
    later_goal.run_ids.clear();
    let mut later_view = view.clone();
    later_view.last_sequence = later_goal.started_sequence;
    let later_report = goal_report(&later_view, &view.runs[0], &later_goal);
    assert_eq!(later_report.unresolved, report.unresolved[..9]);
    later_report.validate().unwrap();

    let run = &mut view.runs[0];
    let template = run
        .agents
        .iter()
        .find(|agent| agent.assignment.is_some())
        .unwrap()
        .clone();
    let mut uncertain_tool = run.tools[0].clone();
    uncertain_tool.observation = None;
    for _ in 0..25 {
        let mut agent = template.clone();
        agent.id = AgentRunId::new();
        agent.tools = vec![uncertain_tool.clone()];
        run.agents.push(agent);
    }
    let report = goal_report(&view, &view.runs[0], view.goal(f.goal).unwrap());
    assert_eq!(report.unresolved[25], newest);
    assert!(report.unresolved[..25].iter().all(|line| {
        line.starts_with("Worker ") && line.contains("private_outcome_unknown=true")
    }));
    assert_eq!(report.unresolved.last().unwrap(), GOAL_REPORT_OMISSION);
    assert!(report.unresolved.len() < GOAL_REPORT_ITEM_LIMIT);
    assert!(
        report
            .unresolved
            .iter()
            .filter(|line| line.starts_with("Workspace rewind "))
            .count()
            < 8
    );
    let bytes = serde_json::to_vec(&report).unwrap();
    assert!(bytes.len() > crate::session::MAX_GOAL_REPORT_BYTES * 3 / 4);
    report.validate().unwrap();
}
