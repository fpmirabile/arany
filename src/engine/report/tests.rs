use super::*;
use crate::session::{
    AgentRole, AgentRunId, AgentStatus, AgentView, GoalBudget, GoalId, GoalPhase, GoalStatus,
    GoalView, RunId, RunStatus, SessionId, ToolView,
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
            "changeset": {"files": []}
        })
        .to_string(),
        cache_update: CacheUpdate::Failed,
    };
    let mut tools = vec![ToolView {
        intent: command_intent,
        observation: Some(command_observation),
        review_call_index: None,
        finished_sequence: None,
    }];
    tools.extend((0..15).map(|index| {
        let intent = EffectIntent {
            id: Uuid::now_v7(),
            run_id,
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: ToolCall::Read {
                path: format!("src/{index}-{}", "x".repeat(480)),
                offset: 0,
                limit: 4096,
            },
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let observation = crate::tools::ToolObservation {
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
            intent,
            observation: Some(observation),
            review_call_index: None,
            finished_sequence: None,
        }
    }));
    let run = RunView {
        id: run_id,
        objective: "Report bounded evidence".into(),
        images: Vec::new(),
        config: None,
        agents: vec![AgentView {
            id: AgentRunId::new(),
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
            summary: Some("A concise result".into()),
            result: Some("Done".into()),
            provider_calls: Vec::new(),
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
        display: Vec::new(),
        steering_notes: Vec::new(),
        provider_fallbacks: (0..64)
            .map(|_| {
                (
                    AgentRunId::new(),
                    crate::provider::ProviderFallback {
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
        workspace_identity: (1, 2),
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
    };
    let view = crate::session::SessionView {
        id: session_id,
        title: "Report bounded evidence".into(),
        title_is_explicit: false,
        inherited_title: None,
        workspace_identity: Some((1, 2)),
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
        intent: uncertain_intent.clone(),
        observation: Some(uncertain_observation),
        review_call_index: None,
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
        id: Uuid::now_v7(),
        run_id,
        agent_run_id: byte_run.agents[0].id,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Read {
            path: "src/report-failure".into(),
            offset: 0,
            limit: 4096,
        },
        limits: ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let failed_observation = crate::tools::ToolObservation {
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
        observation: Some(failed_observation),
        intent: failed_intent,
        review_call_index: None,
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
                id: Uuid::now_v7(),
                run_id,
                agent_run_id,
                policy_digest: [1; 32],
                enforcement_digest: [2; 32],
                workspace_device: 1,
                workspace_inode: 2,
                call: ToolCall::Read {
                    path: format!("src/{run_index}-{failure_index}"),
                    offset: 0,
                    limit: 4096,
                },
                limits: ToolLimits::default(),
                expires_at_ms: 1,
                use_count: 1,
            };
            let observation = crate::tools::ToolObservation {
                intent: intent.clone(),
                disposition: ToolDisposition::Denied,
                output: String::new(),
                attestation: None,
                cache_update: CacheUpdate::NotApplicable,
            };
            assert!(observation.valid());
            assert!(!observation.attested_for(&intent));
            tools.push(ToolView {
                observation: Some(observation),
                intent,
                review_call_index: None,
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
        intent: timeout_intent.clone(),
        disposition: ToolDisposition::Limit,
        output: crate::tools::types::command_timeout_limit_message(2_000, 1_000),
        attestation: None,
        cache_update: CacheUpdate::NotApplicable,
    };
    assert!(timeout_observation.valid());
    let mut timeout_run = run.clone();
    timeout_run.tools = vec![ToolView {
        intent: timeout_intent,
        observation: Some(timeout_observation),
        review_call_index: None,
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
