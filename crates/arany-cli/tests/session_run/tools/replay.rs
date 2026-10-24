#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
use crate::engine_support::run_with;
use arany::WorkspaceIdentity;
use arany::{
    AgentPhase, CollaborationPolicy, RunStatus, SessionView, StateRoot, ToolCall, ToolDisposition,
};
#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
use arany::{EngineConfig, RunControls, ToolGrant};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;

#[test]
fn tool_journal_transition_corpus_rejects_unmatched_or_reusable_effects() {
    use arany::{
        AgentDisposition, AgentRole, AgentRunId, Event, EventEnvelope, OutputTokenBound,
        ProviderCallDisposition, ProviderCallRecord, RunConfig, RunDisposition, RunId, SessionId,
    };
    let session = SessionId::new();
    let goal = arany::GoalId::new();
    let run = RunId::new();
    let primary = AgentRunId::new();
    const AGENT_SPAWNED: usize = 5;
    const TOOL_STARTED: usize = 11;
    const TOOL_FINISHED: usize = 12;
    const NEXT_CALL_RESERVED: usize = 13;
    const NEXT_CALL_RECORDED: usize = 14;
    const COMMAND_STARTED: usize = TOOL_STARTED;
    const COMMAND_FINISHED: usize = TOOL_STARTED + 1;
    const INTEGRATION_STARTED: usize = TOOL_STARTED + 2;
    const INTEGRATION_FINISHED: usize = TOOL_STARTED + 3;
    const INTEGRATED_NEXT_CALL_RESERVED: usize = TOOL_STARTED + 4;
    let policy = arany::ToolPolicyReceipt {
        host_epoch: None,
        permissions_digest: None,
        contract_version: 1,
        config_digest: [1; 32],
        max_context_bytes: 65536,
        unguarded_rules_digest: None,
        unguarded: arany::UnguardedGrant::HumanOnly,
        child_unguarded: arany::UnguardedGrant::Denied,
    };
    let config = RunConfig {
        supervisor: Some(arany::supervisor::SupervisorPolicy::all_off()),
        worker_policy: None,
        agent_definitions: None,
        roster: None,
        providers: None,
        provider: "scripted".into(),
        model: "replay-model".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: None,
        chatgpt_provenance: None,
        output_token_bound: OutputTokenBound::ProviderEnforced,
        context_window_tokens: None,
        policy: CollaborationPolicy::Single,
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
        tool_policy: Some(Box::new(policy)),
    };
    let call = ToolCall::Command {
        program: "shell".into(),
        args: Vec::new(),
        cwd: String::new(),
    };
    let intent = arany::EffectIntent {
        host: None,
        host_command: None,
        target: arany::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: run,
        agent_run_id: primary,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: call.clone(),
        limits: arany::ToolLimits::default(),
        use_count: 1,
    };
    let observation = arany::ToolObservation {
        enforcement_cause: None,
        limit_cause: None,
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: serde_json::json!({
            "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 0,
            "stdout": "",
            "stderr": "",
            "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset": {"files": []}
        })
        .to_string(),
        attestation: Some(arany::Attestation::Guard(arany::GuardReceipt {
            contract_version: 1,
            intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(),
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        })),
        approval_cause: None,
        cache_update: arany::CacheUpdate::NotApplicable,
    };
    let record = |disposition| Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: primary,
        record: ProviderCallRecord {
            rate_limits: None,
            delegation: None,
            phase: AgentPhase::RootPlan,
            disposition,
            response_id: None,
            input_tokens: None,
            output_tokens: None,
            wire_provenance: None,
            refusal: None,
            failure_reason: None,
            requested_tools: u8::from(disposition == ProviderCallDisposition::ToolRequested),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        },
    };
    let prefix = [
        Event::SessionStarted {
            title: "Historical tool facts".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        Event::GoalStarted {
            include_paths: Some(Vec::new()),
            goal_id: goal,
            objective: "Past work".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: arany::GoalBudget::default(),
            phase: arany::GoalPhase::Implementation,
            autonomous: false,
            resource_settings: arany::ToolResourceSettings::default(),
            continues: None,
        },
        Event::GoalRunLinked {
            goal_id: goal,
            run_id: run,
            phase: arany::GoalPhase::Implementation,
        },
        Event::MessageAccepted {
            skill: None,
            run_id: run,
            text: "Past work".into(),
            images: Vec::new(),
        },
        Event::RunStarted {
            run_id: run,
            config,
            active_deadline_ms: None,
        },
        Event::AgentSpawned {
            profile: None,
            assignment: None,
            definition: None,
            run_id: run,
            agent_run_id: primary,
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
            grant: None,
        },
        Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: primary,
            active_ceiling_ms: 0,
        },
        record(ProviderCallDisposition::PlanRecorded),
        Event::GoalPlanContractRecorded {
            goal_id: goal,
            run_id: run,
            contract: arany::GoalPlanContract {
                mission: "Verify the bounded command result".into(),
                paths: vec!["src".into()],
                commands: vec!["shell".into()],
                resource_class: arany::ToolResourceClass::Default,
                command_timeout_ms: None,
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: arany::GoalSuccessCriterion::WorkspaceFileContent {
                    path: "src/new.txt".into(),
                    content: "new\n".into(),
                },
                budget: arany::GoalBudget::default(),
            },
        },
        Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: primary,
            active_ceiling_ms: 0,
        },
        record(ProviderCallDisposition::ToolRequested),
        Event::ToolStarted {
            run_id: run,
            agent_run_id: primary,
            intent,
            active_ceiling_ms: 0,
        },
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: primary,
            observation,
        },
    ];
    let suffix = vec![
        Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: primary,
            active_ceiling_ms: 0,
        },
        record(ProviderCallDisposition::Finished),
        Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: primary,
            disposition: AgentDisposition::Finished,
            summary: Some("Past result".into()),
            result: Some("Past result".into()),
        },
        Event::MessageCommitted {
            run_id: run,
            text: "Past result".into(),
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Finished,
            failure: None,
            active_ms: 0,
        },
    ];
    let replay = |events: Vec<Event>| {
        let envelopes: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| {
                let scope = match &event {
                    Event::SessionStarted { .. }
                    | Event::GoalStarted { .. }
                    | Event::GoalResumed { .. }
                    | Event::GoalFinished { .. } => (None, None),
                    Event::GoalRunLinked { run_id, .. }
                    | Event::GoalPlanContractRecorded { run_id, .. } => (Some(*run_id), None),
                    Event::MessageAccepted { run_id, .. }
                    | Event::RunStarted { run_id, .. }
                    | Event::MessageCommitted { run_id, .. }
                    | Event::RunFinished { run_id, .. } => (Some(*run_id), None),
                    Event::AgentSpawned {
                        run_id,
                        agent_run_id,
                        ..
                    }
                    | Event::ProviderCallRecorded {
                        run_id,
                        agent_run_id,
                        ..
                    }
                    | Event::GoalProviderCallReserved {
                        run_id,
                        agent_run_id,
                        ..
                    }
                    | Event::ToolStarted {
                        run_id,
                        agent_run_id,
                        ..
                    }
                    | Event::ToolFinished {
                        run_id,
                        agent_run_id,
                        ..
                    }
                    | Event::AgentFinished {
                        run_id,
                        agent_run_id,
                        ..
                    } => (Some(*run_id), Some(*agent_run_id)),
                    _ => unreachable!(),
                };
                EventEnvelope {
                    sequence: index as u64 + 1,
                    session_id: session,
                    run_id: scope.0,
                    agent_run_id: scope.1,
                    event,
                    created_at_ms: 1,
                }
            })
            .collect();
        SessionView::replay(session, &envelopes)
    };
    let all: Vec<_> = prefix.iter().chain(&suffix).cloned().collect();
    assert_eq!(
        replay(all.clone()).unwrap().unwrap().runs[0].status,
        RunStatus::Finished
    );
    for stream in ["stdout", "stderr"] {
        for fault in ["missing", "bytes", "lines"] {
            let mut row = all.clone();
            if let Event::ToolFinished { observation, .. } = &mut row[TOOL_FINISHED] {
                let mut value: serde_json::Value =
                    serde_json::from_str(&observation.output).unwrap();
                value[stream] = serde_json::json!("head\ntail\n");
                let total = format!("{stream}_total");
                value[&total] = serde_json::json!({"bytes":10,"lines":2});
                observation.output = value.to_string();
                assert!(
                    replay({
                        let mut valid = all.clone();
                        valid[TOOL_FINISHED] = row[TOOL_FINISHED].clone();
                        valid
                    })
                    .is_ok()
                );
                match fault {
                    "missing" => {
                        value.as_object_mut().unwrap().remove(&total);
                    }
                    "bytes" => value[&total]["bytes"] = serde_json::json!(9),
                    "lines" => value[&total]["lines"] = serde_json::json!(1),
                    _ => unreachable!(),
                }
                if let Event::ToolFinished { observation, .. } = &mut row[TOOL_FINISHED] {
                    observation.output = value.to_string();
                }
            }
            assert!(replay(row).is_err(), "{stream} total {fault}");
        }
    }
    let timeout_refusal = || {
        let mut row = prefix.to_vec();
        for event in &mut row {
            match event {
                Event::GoalStarted {
                    resource_settings, ..
                } => {
                    resource_settings.default_command_timeout_ms = 1_000;
                    resource_settings.max_command_timeout_ms = 1_000;
                }
                Event::GoalPlanContractRecorded { contract, .. } => {
                    contract.command_timeout_ms = Some(2_000);
                }
                Event::ToolStarted { intent, .. } => {
                    intent.limits.runtime_ms = 2_000;
                    intent.limits.cache_bytes = 0;
                    intent.limits.cache_candidate_bytes = 0;
                    intent.limits.cache_transfer_ms = 0;
                }
                Event::ToolFinished { observation, .. } => {
                    observation.intent.limits.runtime_ms = 2_000;
                    observation.intent.limits.cache_bytes = 0;
                    observation.intent.limits.cache_candidate_bytes = 0;
                    observation.intent.limits.cache_transfer_ms = 0;
                    observation.disposition = ToolDisposition::Limit;
                    observation.limit_cause = Some(arany::LimitCause::RequestedTime);
                    observation.output = "Timeout 2000 ms exceeds pinned max 1000 ms. Lower request or change /settings; Goal remains pinned.".into();
                    observation.attestation = None;
                    observation.cache_update = arany::CacheUpdate::NotApplicable;
                }
                _ => {}
            }
        }
        row
    };
    let admitted_timeout_refusal = timeout_refusal();
    assert!(
        replay(admitted_timeout_refusal.clone()).is_ok(),
        "over-max command is recorded as a correlated non-dispatched Limit"
    );
    let mut dispatched_over_max = admitted_timeout_refusal.clone();
    if let Event::ToolFinished { observation, .. } = &mut dispatched_over_max[TOOL_FINISHED] {
        let digest = Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
        observation.disposition = ToolDisposition::Succeeded;
        observation.output = serde_json::json!({
            "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 0,
            "stdout": "",
            "stderr": "",
            "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset": {"files": []}
        })
        .to_string();
        observation.attestation = Some(arany::Attestation::Guard(arany::GuardReceipt {
            contract_version: 1,
            intent_digest: digest,
            enforcement_digest: observation.intent.enforcement_digest,
            limits: observation.intent.limits.clone(),
        }));
    }
    assert!(
        replay(dispatched_over_max).is_err(),
        "replay rejects a guarded success above the pinned maximum"
    );
    let mut mismatched_limit = admitted_timeout_refusal;
    if let Event::ToolStarted { intent, .. } = &mut mismatched_limit[TOOL_STARTED] {
        intent.limits.runtime_ms = 1_000;
    }
    if let Event::ToolFinished { observation, .. } = &mut mismatched_limit[TOOL_FINISHED] {
        observation.intent.limits.runtime_ms = 1_000;
    }
    assert!(
        replay(mismatched_limit).is_err(),
        "replay requires the requested timeout to match the plan and receipt"
    );
    let mut review = record(ProviderCallDisposition::Finished);
    if let Event::ProviderCallRecorded { record, .. } = &mut review {
        record.phase = AgentPhase::ToolReview;
        record.input_tokens = Some(3);
        record.output_tokens = Some(1);
    }
    let mut reviewed = all.clone();
    reviewed.splice(
        TOOL_FINISHED..TOOL_FINISHED,
        [
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            review.clone(),
        ],
    );
    let view = replay(reviewed.clone()).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(
        view.runs[0].agents[0].provider_calls[2].input_tokens,
        Some(3)
    );
    for case in 0..6 {
        let mut row = reviewed.clone();
        match case {
            0 => {
                row.drain(TOOL_FINISHED..TOOL_FINISHED + 2);
                row.insert(TOOL_STARTED, review.clone());
            }
            1 => {
                row.drain(TOOL_FINISHED..TOOL_FINISHED + 2);
                drop(row.splice(
                    TOOL_FINISHED + 1..TOOL_FINISHED + 1,
                    [
                        Event::GoalProviderCallReserved {
                            goal_id: goal,
                            run_id: run,
                            agent_run_id: primary,
                            active_ceiling_ms: 0,
                        },
                        review.clone(),
                    ],
                ));
            }
            2 => {
                drop(row.splice(
                    TOOL_FINISHED + 3..TOOL_FINISHED + 3,
                    [
                        Event::GoalProviderCallReserved {
                            goal_id: goal,
                            run_id: run,
                            agent_run_id: primary,
                            active_ceiling_ms: 0,
                        },
                        review.clone(),
                    ],
                ));
            }
            3 => {
                row.remove(NEXT_CALL_RECORDED + 2);
            }
            4 | 5 => {
                if let Event::ProviderCallRecorded { record, .. } = &mut row[TOOL_FINISHED + 1] {
                    record.disposition = if case == 4 {
                        ProviderCallDisposition::ToolRequested
                    } else {
                        ProviderCallDisposition::Delegated
                    };
                }
            }
            _ => unreachable!(),
        }
        assert!(replay(row).is_err(), "invalid review transition {case}");
    }
    let interrupted = replay(prefix[..TOOL_STARTED + 1].to_vec())
        .unwrap()
        .unwrap();
    assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
    assert!(interrupted.runs[0].tools[0].observation.is_none());
    for case in 0..11 {
        let mut row = all.clone();
        match case {
            0 => {
                row.remove(TOOL_STARTED);
            }
            1 => row.insert(NEXT_CALL_RESERVED, row[TOOL_FINISHED].clone()),
            2 => {
                if let Event::ToolStarted { intent, .. } = &mut row[TOOL_STARTED] {
                    intent.policy_digest = [9; 32];
                }
            }
            3 => {
                if let Event::ToolStarted { intent, .. } = &mut row[TOOL_STARTED] {
                    intent.workspace_inode += 1;
                }
            }
            4 => {
                if let Event::ToolStarted { intent, .. } = &mut row[TOOL_STARTED] {
                    intent.use_count = 2;
                }
            }
            5 => {
                if let Event::ToolFinished { observation, .. } = &mut row[TOOL_FINISHED] {
                    let Some(arany::Attestation::Guard(receipt)) = &mut observation.attestation
                    else {
                        unreachable!()
                    };
                    receipt.intent_digest = [9; 32];
                }
            }
            6 => {
                if let Event::ToolFinished { observation, .. } = &mut row[TOOL_FINISHED] {
                    observation.disposition = ToolDisposition::Uncertain;
                }
            }
            7 => {
                if let Event::ToolFinished { observation, .. } = &mut row[TOOL_FINISHED] {
                    observation.disposition = ToolDisposition::Cancelled;
                }
            }
            8 => {
                if let Event::ToolStarted {
                    agent_run_id,
                    intent,
                    ..
                } = &mut row[TOOL_STARTED]
                {
                    *agent_run_id = AgentRunId::new();
                    intent.agent_run_id = *agent_run_id;
                }
            }
            9 => {
                if let Event::RunStarted { config, .. } = &mut row[4] {
                    config.tool_policy = None;
                    config.supervisor = None;
                }
            }
            10 => {
                if let Event::ToolFinished { observation, .. } = &mut row[TOOL_FINISHED] {
                    observation.disposition = ToolDisposition::Failed;
                    observation.attestation = None;
                }
            }
            _ => unreachable!(),
        }
        assert!(replay(row).is_err(), "Tool journal transition {case}");
    }
    for (guarded, later_failure) in [(true, true), (false, true), (false, false)] {
        let mut denied = prefix.to_vec();
        if let Event::ToolFinished { observation, .. } = &mut denied[TOOL_FINISHED] {
            observation.disposition = ToolDisposition::Denied;
            observation.output = "UNTRUSTED_TOOL_ERROR_CANARY".into();
            if !guarded {
                observation.attestation = None;
            }
        }
        if later_failure {
            let mut failure = record(ProviderCallDisposition::InvalidResponse);
            if let Event::ProviderCallRecorded { record, .. } = &mut failure {
                record.failure_reason = Some(arany::ProviderFailureReason::OutcomeContract);
            }
            denied.push(Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            });
            denied.push(failure);
        }
        denied.extend([
            Event::AgentFinished {
                worker: None,
                run_id: run,
                agent_run_id: primary,
                disposition: AgentDisposition::Failed,
                summary: None,
                result: None,
            },
            Event::RunFinished {
                run_id: run,
                disposition: RunDisposition::Failed,
                failure: Some(if later_failure {
                    arany::RunFailure::ProviderCall
                } else {
                    arany::RunFailure::ToolStopped
                }),
                active_ms: 0,
            },
        ]);
        let view = replay(denied).unwrap().unwrap();
        let feedback = arany_cli::presentation::run_feedback(&view.runs[0]).unwrap();
        if later_failure {
            assert!(
                feedback.contains("structured outcome contract"),
                "recoverable denial must not mask the later Provider failure"
            );
            assert!(!feedback.contains("approval expired"));
        } else {
            assert!(feedback.contains("Action denied"));
        }
        assert!(!feedback.contains("CANARY"));
    }
    let mut observed_failure = all.clone();
    if let Event::ToolFinished { observation, .. } = &mut observed_failure[TOOL_FINISHED] {
        observation.disposition = ToolDisposition::Failed;
    }
    assert!(
        replay(observed_failure).is_ok(),
        "an attested ordinary failure can continue"
    );
    let changeset = arany::CommandChangeset {
        files: vec![arany::FileChange {
            path: "src/new.txt".into(),
            before: None,
            after: Some(arany::FileVersion {
                sha256: crate::hash(b"new\n"),
                content: "bmV3Cg==".into(),
                executable: false,
            }),
        }],
    };
    let mut command_intent = match &prefix[TOOL_STARTED] {
        Event::ToolStarted { intent, .. } => intent.clone(),
        _ => unreachable!(),
    };
    command_intent.call = ToolCall::Command {
        program: "shell".into(),
        args: Vec::new(),
        cwd: "".into(),
    };
    let command_digest = Sha256::digest(serde_json::to_vec(&command_intent).unwrap()).into();
    let mut integration_intent = command_intent.clone();
    integration_intent.id = uuid::Uuid::now_v7();
    integration_intent.call = ToolCall::ApplyChangeset {
        source: arany::worker::ChangesetSource::CommandReceipt {
            source_intent_digest: command_digest,
            changeset: changeset.clone(),
        },
    };
    let observed = |intent: arany::EffectIntent, disposition, output: String| Event::ToolFinished {
        checkpoint: matches!(intent.call, ToolCall::ApplyChangeset { .. }).then_some(
            arany::Checkpoint {
                status: arany::CheckpointStatus::NotNeeded,
                reason: None,
                pruned_before_run: None,
            },
        ),
        run_id: run,
        agent_run_id: primary,
        observation: arany::ToolObservation {
            enforcement_cause: None,
            limit_cause: (disposition == arany::ToolDisposition::Limit)
                .then_some(arany::LimitCause::Resource),
            disposition,
            output,
            attestation: Some(arany::Attestation::Guard(arany::GuardReceipt {
                contract_version: 1,
                intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            })),
            intent,
            approval_cause: None,
            cache_update: arany::CacheUpdate::NotApplicable,
        },
    };
    let command_result = serde_json::json!({
        "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":7,"stdout":"","stderr":"","stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0},"changeset":changeset
    })
    .to_string();
    let mut integrated = prefix[..TOOL_STARTED].to_vec();
    integrated.extend([
        Event::ToolStarted {
            run_id: run,
            agent_run_id: primary,
            intent: command_intent.clone(),
            active_ceiling_ms: 0,
        },
        observed(
            command_intent.clone(),
            ToolDisposition::Failed,
            command_result.clone(),
        ),
        Event::ToolStarted {
            run_id: run,
            agent_run_id: primary,
            intent: integration_intent.clone(),
            active_ceiling_ms: 0,
        },
        observed(
            integration_intent.clone(),
            ToolDisposition::Succeeded,
            "{\"applied\":1}".into(),
        ),
    ]);
    integrated.extend(suffix.clone());
    let view = replay(integrated.clone()).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].tools.len(), 2);
    assert_eq!(view.runs[0].agents[0].provider_calls.len(), 3);
    let mut reviewed_integration = integrated.clone();
    reviewed_integration.splice(
        COMMAND_FINISHED..COMMAND_FINISHED,
        [
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            review.clone(),
        ],
    );
    reviewed_integration.splice(
        INTEGRATION_FINISHED + 2..INTEGRATION_FINISHED + 2,
        [
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            review.clone(),
        ],
    );
    let view = replay(reviewed_integration).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].agents[0].provider_calls.len(), 5);
    let mut duplicate_review = integrated.clone();
    duplicate_review.splice(
        INTEGRATION_STARTED + 1..INTEGRATION_STARTED + 1,
        [
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            review.clone(),
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            review.clone(),
        ],
    );
    assert!(
        replay(duplicate_review).is_err(),
        "integration admits one review even when its command had none"
    );
    let interrupted = replay(integrated[..INTEGRATION_FINISHED].to_vec())
        .unwrap()
        .unwrap();
    assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
    assert!(interrupted.runs[0].tools[1].observation.is_none());
    let mut unpublished = integrated[..COMMAND_FINISHED + 1].to_vec();
    unpublished.extend([
        Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: primary,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Failed,
            failure: Some(arany::RunFailure::ToolStopped),
            active_ms: 0,
        },
    ]);
    let view = replay(unpublished).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Failed);
    assert_eq!(view.runs[0].tools.len(), 1);
    let mut unfinalized_cache = integrated[..COMMAND_FINISHED + 1].to_vec();
    if let Event::ToolFinished { observation, .. } = &mut unfinalized_cache[COMMAND_FINISHED] {
        observation.disposition = ToolDisposition::Succeeded;
        observation.output = serde_json::json!({
            "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 0,
            "stdout": "build complete",
            "stderr": "",
            "stdout_total":{"bytes":14,"lines":1},"stderr_total":{"bytes":0,"lines":0},"changeset": changeset,
        })
        .to_string();
        observation.cache_update = arany::CacheUpdate::NotAttempted;
    }
    assert!(
        replay(unfinalized_cache).is_err(),
        "successful cache-backed command requires controller finalization before Event commit"
    );
    let mut uncertain_cache = integrated[..COMMAND_FINISHED + 1].to_vec();
    if let Event::ToolFinished { observation, .. } = &mut uncertain_cache[COMMAND_FINISHED] {
        observation.disposition = ToolDisposition::Succeeded;
        observation.output = serde_json::json!({
            "selection":{"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 0,
            "stdout": "build complete",
            "stderr": "",
            "stdout_total":{"bytes":14,"lines":1},"stderr_total":{"bytes":0,"lines":0},"changeset": changeset,
        })
        .to_string();
        observation.cache_update = arany::CacheUpdate::Uncertain;
    }
    uncertain_cache.extend([
        Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: primary,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Failed,
            failure: Some(arany::RunFailure::ToolStopped),
            active_ms: 0,
        },
    ]);
    let view = replay(uncertain_cache).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Failed);
    assert_eq!(view.runs[0].tools.len(), 1);
    let retained = view.runs[0].tools[0].observation.as_ref().unwrap();
    assert_eq!(retained.disposition, ToolDisposition::Succeeded);
    assert_eq!(retained.cache_update, arany::CacheUpdate::Uncertain);
    assert_eq!(
        arany_cli::presentation::render_cache_update_feedback(&view.runs[0]),
        Some("Build cache outcome is uncertain; inspect Session history before retrying.")
    );
    assert_eq!(
        arany_cli::presentation::run_feedback(&view.runs[0]).as_deref(),
        Some(
            "Error: The command succeeded but its build cache update is uncertain; the Run stopped without retrying. No answer was committed."
        )
    );
    for case in [
        "source",
        "content",
        "missing-command",
        "unattested",
        "denied",
        "reused",
        "model-request",
        "new-inference",
        "omitted-integration",
    ] {
        let mut row = integrated.clone();
        match case {
            "source" | "content" => {
                let Event::ToolStarted { intent, .. } = &mut row[INTEGRATION_STARTED] else {
                    unreachable!()
                };
                let ToolCall::ApplyChangeset {
                    source:
                        arany::worker::ChangesetSource::CommandReceipt {
                            source_intent_digest,
                            changeset,
                        },
                } = &mut intent.call
                else {
                    unreachable!()
                };
                if case == "source" {
                    *source_intent_digest = [9; 32];
                } else {
                    changeset.files[0].after.as_mut().unwrap().executable = true;
                }
                let altered = intent.clone();
                let Event::ToolFinished { observation, .. } = &mut row[INTEGRATION_FINISHED] else {
                    unreachable!()
                };
                observation.intent = altered;
                let Some(arany::Attestation::Guard(receipt)) = &mut observation.attestation else {
                    unreachable!()
                };
                receipt.intent_digest =
                    Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
            }
            "missing-command" => {
                row.drain(COMMAND_STARTED..COMMAND_FINISHED + 1);
            }
            "unattested" | "denied" => {
                let Event::ToolFinished { observation, .. } = &mut row[COMMAND_FINISHED] else {
                    unreachable!()
                };
                if case == "unattested" {
                    observation.attestation = None;
                } else {
                    observation.disposition = ToolDisposition::Denied;
                }
            }
            "reused" => {
                let mut reuse = integrated[INTEGRATION_STARTED..INTEGRATION_FINISHED + 1].to_vec();
                let Event::ToolStarted { intent, .. } = &mut reuse[0] else {
                    unreachable!()
                };
                intent.id = uuid::Uuid::now_v7();
                let altered = intent.clone();
                let Event::ToolFinished { observation, .. } = &mut reuse[1] else {
                    unreachable!()
                };
                observation.intent = altered;
                let Some(arany::Attestation::Guard(receipt)) = &mut observation.attestation else {
                    unreachable!()
                };
                receipt.intent_digest =
                    Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
                row.splice(
                    INTEGRATED_NEXT_CALL_RESERVED..INTEGRATED_NEXT_CALL_RESERVED,
                    reuse,
                );
            }
            "model-request" => {
                row.splice(
                    INTEGRATION_STARTED..INTEGRATION_STARTED,
                    [
                        Event::GoalProviderCallReserved {
                            goal_id: goal,
                            run_id: run,
                            agent_run_id: primary,
                            active_ceiling_ms: 0,
                        },
                        record(ProviderCallDisposition::ToolRequested),
                    ],
                );
            }
            "new-inference" => {
                drop(row.splice(
                    INTEGRATION_STARTED..INTEGRATION_STARTED,
                    [
                        Event::GoalProviderCallReserved {
                            goal_id: goal,
                            run_id: run,
                            agent_run_id: primary,
                            active_ceiling_ms: 0,
                        },
                        record(ProviderCallDisposition::Finished),
                    ],
                ));
            }
            "omitted-integration" => {
                row.drain(INTEGRATION_STARTED..INTEGRATION_FINISHED + 1);
            }
            _ => unreachable!(),
        }
        assert!(
            replay(row).is_err(),
            "integration journal correlation: {case}"
        );
    }
    let mut out_of_plan_integration = integrated.clone();
    if let Event::ToolFinished { observation, .. } = &mut out_of_plan_integration[COMMAND_FINISHED]
    {
        let mut result: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        result["changeset"]["files"][0]["path"] = "unselected/new.txt".into();
        observation.output = result.to_string();
    }
    let mut out_of_plan_intent = match &out_of_plan_integration[INTEGRATION_STARTED] {
        Event::ToolStarted { intent, .. } => intent.clone(),
        _ => unreachable!(),
    };
    let ToolCall::ApplyChangeset {
        source: arany::worker::ChangesetSource::CommandReceipt { changeset, .. },
    } = &mut out_of_plan_intent.call
    else {
        unreachable!()
    };
    changeset.files[0].path = "unselected/new.txt".into();
    if let Event::ToolStarted { intent, .. } = &mut out_of_plan_integration[INTEGRATION_STARTED] {
        *intent = out_of_plan_intent.clone();
    }
    if let Event::ToolFinished {
        observation,
        checkpoint,
        ..
    } = &mut out_of_plan_integration[INTEGRATION_FINISHED]
    {
        *checkpoint = None;
        observation.intent = out_of_plan_intent.clone();
        observation.disposition = ToolDisposition::Denied;
        observation.output = "The effect is outside the active Goal plan".into();
        observation.attestation = None;
        observation.cache_update = arany::CacheUpdate::NotApplicable;
    }
    out_of_plan_integration.truncate(INTEGRATION_FINISHED + 1);
    out_of_plan_integration.extend([
        Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: primary,
            active_ceiling_ms: 0,
        },
        record(ProviderCallDisposition::Finished),
        Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: primary,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Failed,
            failure: Some(arany::RunFailure::ToolStopped),
            active_ms: 0,
        },
    ]);
    let view = replay(out_of_plan_integration.clone()).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Failed);
    let denied = view.runs[0].tools[1].observation.as_ref().unwrap();
    assert_eq!(denied.disposition, ToolDisposition::Denied);
    assert!(denied.attestation.is_none());
    assert_eq!(denied.cache_update, arany::CacheUpdate::NotApplicable);

    let mut forged_out_of_plan_publication = out_of_plan_integration;
    let Event::ToolStarted { intent, .. } = &forged_out_of_plan_publication[INTEGRATION_STARTED]
    else {
        unreachable!()
    };
    let intent = intent.clone();
    if let Event::ToolFinished {
        observation,
        checkpoint,
        ..
    } = &mut forged_out_of_plan_publication[INTEGRATION_FINISHED]
    {
        *checkpoint = Some(arany::Checkpoint {
            status: arany::CheckpointStatus::NotNeeded,
            reason: None,
            pruned_before_run: None,
        });
        observation.disposition = ToolDisposition::Succeeded;
        observation.output = "{\"applied\":1}".into();
        observation.attestation = Some(arany::Attestation::Guard(arany::GuardReceipt {
            contract_version: 1,
            intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(),
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        }));
    }
    assert!(
        replay(forged_out_of_plan_publication).is_err(),
        "an out-of-plan integration cannot carry a dispatched or successful receipt"
    );
    for (limit, count, accepted) in [
        (None, 64, true),
        (Some(64), 63, true),
        (Some(64), 64, false),
    ] {
        let mut rows = prefix[..AGENT_SPAWNED + 1].to_vec();
        if let Event::GoalStarted { budget, .. } = &mut rows[1] {
            budget.max_provider_calls = limit;
        }
        for _ in 0..count {
            let mut start = prefix[TOOL_STARTED].clone();
            let Event::ToolStarted { intent, .. } = &mut start else {
                unreachable!()
            };
            intent.id = uuid::Uuid::now_v7();
            let mut result = prefix[TOOL_FINISHED].clone();
            let Event::ToolFinished { observation, .. } = &mut result else {
                unreachable!()
            };
            observation.intent = intent.clone();
            let Some(arany::Attestation::Guard(receipt)) = &mut observation.attestation else {
                unreachable!()
            };
            receipt.intent_digest = Sha256::digest(serde_json::to_vec(intent).unwrap()).into();
            rows.extend([
                Event::GoalProviderCallReserved {
                    goal_id: goal,
                    run_id: run,
                    agent_run_id: primary,
                    active_ceiling_ms: 0,
                },
                record(ProviderCallDisposition::ToolRequested),
            ]);
            rows.extend([start, result]);
        }
        rows.extend(suffix.clone());
        assert_eq!(replay(rows).is_ok(), accepted, "explicit Goal call bound");
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
pub(super) async fn native_command_changeset_corpus(
    root: &std::path::Path,
    config: &serde_json::Value,
    program: &str,
) {
    use arany::{
        Answer, ApprovalMode, Provider, ProviderError, ProviderOutcome, ProviderRequest,
        ProviderResponse, RunCancellation, RunHints, RunRequest, StateRoot, Store, ToolApprovals,
    };
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct CommandJourney {
        calls: Vec<ToolCall>,
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
        review: bool,
        goal_paths: Option<Vec<String>>,
        #[cfg(target_os = "macos")]
        trace_root: std::path::PathBuf,
    }
    #[async_trait::async_trait]
    impl Provider for CommandJourney {
        fn identity(&self) -> &arany::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    arany::ProviderIdentity::new(
                        "scripted",
                        "changeset-model",
                        arany::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let planned = crate::planned(&request);

            let mut requests = self.requests.lock().unwrap();
            let review = request.phase == AgentPhase::ToolReview;
            let step = requests
                .iter()
                .filter(|request| request.phase != AgentPhase::ToolReview)
                .count()
                .saturating_sub(usize::from(planned));
            let catalog = request
                .input
                .tools
                .as_ref()
                .map(|tools| serde_json::from_str::<serde_json::Value>(tools.catalog()))
                .transpose()
                .map_err(|_| ProviderError::InvalidOutcome)?;
            if step > self.calls.len() || review && !self.review {
                return Err(ProviderError::InvalidOutcome);
            }
            let outcome = if review {
                ProviderOutcome::Review(arany::ReviewVerdict::Approve)
            } else {
                self.calls.get(step).cloned().map_or_else(
                    || {
                        ProviderOutcome::Answer(Answer {
                            text: "Observed both effect stages.".into(),
                        })
                    },
                    |call| {
                        let plan_contract = (step == 0)
                            .then(|| {
                                let mut paths: Vec<_> =
                                    self.goal_paths.clone().unwrap_or_else(|| {
                                        catalog
                                            .as_ref()
                                            .and_then(|catalog| {
                                                catalog["workspace_paths"].as_array()
                                            })
                                            .into_iter()
                                            .flatten()
                                            .filter_map(|path| path.as_str().map(str::to_owned))
                                            .collect()
                                    });
                                paths.sort();
                                paths.dedup();
                                let mut commands: Vec<_> = self
                                    .calls
                                    .iter()
                                    .filter_map(|call| match call {
                                        ToolCall::Command { program, .. } => Some(program.clone()),
                                        _ => None,
                                    })
                                    .collect();
                                commands.sort();
                                commands.dedup();
                                let success_call =
                                    self.calls.last().cloned().unwrap_or(call.clone());
                                arany::GoalPlanContract {
                                    mission: "Exercise and verify the bounded command snapshot"
                                        .into(),
                                    paths,
                                    commands,
                                    resource_class: arany::ToolResourceClass::Default,
                                    command_timeout_ms: None,
                                    skills: Vec::new(),
                                    mcp_tools: Vec::new(),
                                    success_criterion:
                                        arany::GoalSuccessCriterion::ToolCallSucceeded {
                                            call: Box::new(success_call),
                                        },
                                    budget: arany::GoalBudget::default(),
                                }
                            })
                            .map(Box::new);
                        if !planned && let Some(contract) = plan_contract {
                            arany::test_support::scenario::plan_outcome(*contract)
                        } else {
                            crate::tool_outcome(call)
                        }
                    },
                )
            };
            #[cfg(target_os = "macos")]
            if !review && self.calls.get(step).is_some() {
                crate::tools::reset_tool_trace_stages(&self.trace_root);
            }
            requests.push(request);
            Ok(ProviderResponse {
                turn: arany::ProviderTurn::scripted(),
                outcome,
                response_id: None,
                usage: arany::Usage {
                    input_tokens: review.then_some(3),
                    output_tokens: review.then_some(1),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }
    }
    for (case, suffix, rejected) in [
        ("nonzero", "exit 7", None),
        (
            "selection",
            "test ! -e src/asset; test ! -e src/ignored/secret; test ! -r /prepared/command-selection.json; mkdir src/ignored; printf generated > src/ignored/new; printf '*' > src/.gitignore",
            None,
        ),
        ("ai-approve", "", None),
        ("deny", "", None),
        ("drop", "", None),
        ("cancel", "", None),
        ("conflict", "", None),
        ("scope", "printf forbidden > unselected", None),
        ("scope-auto", "printf forbidden > unselected", None),
        ("partial", "", None),
        (
            "symlink",
            "ln -s m-modified src/hostile",
            Some(ToolDisposition::Denied),
        ),
        (
            "hardlink",
            "ln src/m-modified src/hostile",
            Some(ToolDisposition::Denied),
        ),
        ("fifo", "mkfifo src/hostile", Some(ToolDisposition::Denied)),
        (
            "setid",
            "chmod 4755 src/m-modified",
            Some(ToolDisposition::Denied),
        ),
        (
            "excluded",
            "mkdir src/.git; printf forbidden > src/.git/blocked",
            Some(ToolDisposition::Denied),
        ),
        (
            "bytes-overflow",
            "printf '%8193s' x > src/overflow",
            Some(ToolDisposition::Limit),
        ),
        (
            "files-overflow",
            "i=0; while test $i -lt 65; do printf x > src/extra-$i; i=$((i + 1)); done",
            Some(ToolDisposition::Limit),
        ),
        (
            "directory-conversion",
            "rm src/m-modified; mkdir src/m-modified; printf x > src/m-modified/child",
            Some(ToolDisposition::Denied),
        ),
    ] {
        let case_root = root.join(format!("changeset-{case}"));
        let workspace = case_root.join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        crate::private_file(&workspace.join("src/m-modified"), b"before\n");
        crate::private_file(&workspace.join("src/z-deleted"), b"delete\n");
        if case == "selection" {
            crate::private_file(&workspace.join("src/.gitignore"), b"ignored/\n");
            std::fs::create_dir(workspace.join("src/ignored")).unwrap();
            crate::private_file(
                &workspace.join("src/ignored/secret"),
                b"omitted input canary",
            );
            crate::private_file(&workspace.join("src/asset"), &vec![b'x'; 1024 * 1024 + 1]);
        }
        if case == "partial" {
            std::fs::create_dir(workspace.join("src/a")).unwrap();
            std::fs::create_dir(workspace.join("src/z")).unwrap();
            std::fs::set_permissions(
                workspace.join("src/z"),
                std::fs::Permissions::from_mode(0o500),
            )
            .unwrap();
        }
        let state_path = case_root.join("state");
        let state = StateRoot::admit(&state_path).unwrap();
        let mut config = config.clone();
        config["workspace_paths"] = json!(if matches!(case, "scope" | "scope-auto") {
            ["."]
        } else {
            ["src"]
        });
        config["skills"] = json!([]);
        config["mcp"] = json!([]);
        crate::private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let delete = if cfg!(target_os = "macos") {
            "/usr/bin/find src/z-deleted -type f -delete"
        } else {
            "rm src/z-deleted"
        };
        let suffix = if cfg!(target_os = "macos") {
            match case {
                "directory-conversion" => {
                    "/usr/bin/find src/m-modified -type f -delete; mkdir src/m-modified; printf x > src/m-modified/child"
                }
                _ => suffix,
            }
        } else {
            suffix
        };
        let native_setid = cfg!(target_os = "macos") && case == "setid";
        let rejected = if native_setid {
            Some(ToolDisposition::Failed)
        } else {
            rejected
        };
        let call = if native_setid {
            ToolCall::Command {
                program: program.into(),
                args: vec!["-c".into(), "set -eu; chmod 4755 src/m-modified".into()],
                cwd: "".into(),
            }
        } else if case == "partial" {
            ToolCall::Command { program: program.into(), args: vec!["-c".into(), "set -eu; printf 'first\\n' > src/a/created; printf 'last\\n' > src/z/created; printf payload-complete".into()], cwd: "".into() }
        } else if cfg!(target_os = "macos") && matches!(case, "symlink" | "hardlink") {
            ToolCall::Command {
                program: "native-links".into(),
                args: vec![case.into()],
                cwd: "".into(),
            }
        } else {
            ToolCall::Command {
                program: program.into(),
                args: vec![
                    "-c".into(),
                    format!(
                        "set -eu; printf '\\000\\377' > src/a-created; chmod 700 src/a-created; printf 'modified\\n' > src/m-modified; {delete}; printf payload-complete; {suffix}"
                    ),
                ],
                cwd: "".into(),
            }
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut calls = vec![call.clone()];
        if case == "nonzero" {
            calls.push(ToolCall::Command {
                program: program.into(),
                args: vec!["-c".into(), "set -eu; test -x src/a-created; printf '\\000\\377' | cmp - src/a-created; test \"$(cat src/m-modified)\" = modified; test ! -e src/z-deleted; printf fresh-binary-snapshot".into()],
                cwd: "".into(),
            });
        }
        let provider = Arc::new(CommandJourney {
            calls,
            requests: requests.clone(),
            review: matches!(case, "ai-approve" | "scope-auto"),
            goal_paths: matches!(case, "scope" | "scope-auto").then(|| vec!["src".into()]),
            #[cfg(target_os = "macos")]
            trace_root: root.to_path_buf(),
        });
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        crate::tools::trace_case(root, case);
        let interactive = matches!(case, "deny" | "drop" | "cancel" | "conflict");
        let (approvals, mut inbox) =
            ToolApprovals::new(if matches!(case, "ai-approve" | "scope-auto") {
                ApprovalMode::Auto
            } else {
                ApprovalMode::Request
            });
        // The test holds the sender so the inbox stays open after the Run.
        let granted = (interactive || matches!(case, "ai-approve" | "scope" | "scope-auto"))
            .then(|| approvals.clone());
        let cancellation = RunCancellation::new();
        let pending = cancellation.clone();
        let (run_done, run_done_rx) = tokio::sync::oneshot::channel();
        let decision = async {
            if case == "scope" {
                let command = inbox.next().await.expect("in-scope command approval");
                assert_eq!(command.intent.call, call, "scope: command intent");
                command.decide(true);
            }
            if matches!(case, "scope" | "scope-auto") {
                tokio::select! {
                    biased;
                    approval = inbox.next() => panic!("{case}: unexpected out-of-plan integration approval ({})", approval.is_some()),
                    _ = run_done_rx => {}
                }
                return;
            }
            if !interactive {
                return;
            }
            let command = inbox.next().await.expect("pending command approval");
            assert_eq!(command.intent.call, call, "{case}: command intent");
            let source: [u8; 32] =
                Sha256::digest(serde_json::to_vec(&command.intent).unwrap()).into();
            command.decide(true);
            let action = inbox.next().await.expect("separate integration approval");
            let ToolCall::ApplyChangeset {
                source:
                    arany::worker::ChangesetSource::CommandReceipt {
                        source_intent_digest,
                        changeset,
                    },
            } = &action.intent.call
            else {
                panic!("{case}: expected integration");
            };
            assert_eq!(*source_intent_digest, source, "{case}: source correlation");
            assert_eq!(
                changeset
                    .files
                    .iter()
                    .map(|file| file.path.as_str())
                    .collect::<Vec<_>>(),
                ["src/a-created", "src/m-modified", "src/z-deleted"]
            );
            assert!(
                !workspace.join("src/a-created").exists(),
                "{case}: no publication before approval"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/m-modified")).unwrap(),
                b"before\n"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/z-deleted")).unwrap(),
                b"delete\n"
            );
            match case {
                "deny" => action.decide(false),
                "drop" => drop(action),
                "cancel" => {
                    assert!(pending.cancel());
                    drop(action);
                }
                "conflict" => {
                    crate::private_file(&workspace.join("src/m-modified"), b"concurrent\n");
                    action.decide(true);
                }
                _ => unreachable!(),
            }
        };
        let timeout = Duration::from_secs(120);
        let (outcome, ()) = tokio::time::timeout(timeout, async {
            tokio::join!(
                async {
                    let outcome = run_with(
                        &engine,
                        &provider,
                        RunRequest {
                            skill: None,
                            session_id: None,
                            title: Some(format!("Changeset {case}")),
                            objective: "Exercise native changeset integration".into(),
                            images: Vec::new(),
                            workspace: workspace.clone(),
                            launch_directory: None,
                            include_paths: Vec::new(),
                            policy: CollaborationPolicy::Single,
                            goal: arany::GoalRequest::default(),
                        },
                        RunControls {
                            tools: ToolGrant::Configured,
                            approvals: granted,
                            cancellation,
                            hints: Some(RunHints::new()),
                            ..Default::default()
                        },
                    )
                    .await;
                    let _ = run_done.send(());
                    outcome
                },
                decision
            )
        })
        .await
        .unwrap_or_else(|_| panic!("{case}: native changeset deadline"));
        // An unknown publication journals no terminal Event; the reopened Run is interrupted.
        let outcome = match outcome {
            Ok(outcome) => {
                assert_ne!(
                    case, "partial",
                    "an unknown publication never settles its Run"
                );
                Some(outcome)
            }
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain)) if case == "partial" => None,
            Err(error) => panic!("{case}: native changeset Engine run failed: {error:?}"),
        };
        engine.close().await.unwrap();
        let session_id = match &outcome {
            Some(outcome) => outcome.session_id,
            None => {
                arany::list_sessions(
                    StateRoot::open_existing(&state_path).unwrap(),
                    workspace.clone(),
                )
                .await
                .unwrap()[0]
                    .id
            }
        };
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(session_id).await.unwrap();
        store.close().await.unwrap();
        let view = SessionView::replay(session_id, &events).unwrap().unwrap();
        let run = outcome
            .as_ref()
            .map_or_else(|| view.runs[0].clone(), |outcome| outcome.run.clone());
        assert_eq!(view.runs[0], run, "{case}: closed replay");
        assert_eq!(
            events
                .iter()
                .any(|envelope| matches!(envelope.event, arany::Event::RunFinished { .. })),
            case != "partial",
            "{case}: terminal Event"
        );
        let human_denied = matches!(case, "deny" | "drop" | "cancel");
        let stopped = matches!(case, "cancel" | "partial");
        let criterion_unmet = matches!(
            case,
            "deny"
                | "drop"
                | "conflict"
                | "scope"
                | "scope-auto"
                | "symlink"
                | "hardlink"
                | "fifo"
                | "setid"
                | "excluded"
                | "bytes-overflow"
                | "files-overflow"
                | "directory-conversion"
        );
        assert_eq!(
            run.status,
            if case == "cancel" {
                RunStatus::Cancelled
            } else if case == "partial" {
                RunStatus::Interrupted
            } else if stopped || criterion_unmet {
                RunStatus::Failed
            } else {
                RunStatus::Finished
            },
            "{case}: {} Tools",
            run.tools.len()
        );
        let requests = requests.lock().unwrap().clone();
        if case == "scope" {
            assert!(
                requests
                    .iter()
                    .all(|request| request.phase != AgentPhase::ToolReview),
                "{case}: Request mode must not invoke the AI judge"
            );
        }
        assert_eq!(
            requests.len(),
            if stopped {
                2
            } else if case == "nonzero" {
                4
            } else if case == "ai-approve" {
                5
            } else if case == "scope-auto" {
                4
            } else {
                3
            },
            "{case}: no retry"
        );
        for request in &requests {
            assert_eq!(request.model, "changeset-model");
            if request.phase == AgentPhase::ToolReview {
                assert!(
                    request.input.tools.is_none()
                        && request.input.includes.is_empty()
                        && request.input.images.is_empty()
                        && request.input.history.is_empty()
                        && request.input.child_results.is_empty()
                        && request.input.context_summary.is_none()
                );
                assert_eq!(request.input.role, arany::PromptRole::ToolReview);
                assert_eq!(request.max_output_tokens, 256);
                let payload: serde_json::Value =
                    serde_json::from_str(&request.input.objective).unwrap();
                assert_eq!(
                    payload["user_request"],
                    "Exercise native changeset integration"
                );
                continue;
            }
            assert_eq!(request.phase, AgentPhase::RootPlan);
            assert_eq!(
                request.input.objective,
                "Exercise native changeset integration"
            );
            assert!(
                request.input.workspace_guidance.is_empty()
                    && request.input.history.is_empty()
                    && request.input.includes.is_empty()
                    && request.input.context_summary.is_none()
                    && request.input.child_results.is_empty()
                    && request.input.images.is_empty()
            );
            assert_eq!(request.max_output_tokens, 4096);
        }
        assert!(
            requests
                .iter()
                .all(|request| request.run_id == view.runs[0].id
                    && request.agent_run_id == view.runs[0].agents[0].id)
        );
        assert_refused_integration_continues(case, &run, &requests);
        let tools = &view.runs[0].tools;
        if matches!(case, "scope" | "scope-auto") {
            let goal = view
                .goals
                .iter()
                .find(|goal| {
                    outcome
                        .as_ref()
                        .is_some_and(|outcome| goal.id == outcome.goal_id)
                })
                .expect("out-of-plan Goal replay");
            assert_eq!(
                goal.status,
                arany::GoalStatus::Failed,
                "{case}: Goal status"
            );
            assert_eq!(
                goal.plan_contract.as_ref().unwrap().paths,
                ["src".to_owned()]
            );
            assert!(
                goal.report
                    .as_ref()
                    .unwrap()
                    .unresolved
                    .iter()
                    .any(|fact| { fact.contains("Command changeset publication failed") }),
                "{case}: durable denial report: {:?}",
                goal.report
            );
            let catalog: serde_json::Value =
                serde_json::from_str(requests[0].input.tools.as_ref().unwrap().catalog()).unwrap();
            assert_eq!(catalog["workspace_paths"], json!(["."]));
        }
        if case == "ai-approve" {
            let reviews = requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .collect::<Vec<_>>();
            assert_eq!(reviews.len(), 2);
            for (request, tool) in reviews.iter().zip(tools) {
                let payload: serde_json::Value =
                    serde_json::from_str(&request.input.objective).unwrap();
                assert_eq!(
                    payload["proposed_action"]["call"],
                    serde_json::to_value(&tool.intent.call).unwrap()
                );
            }
            let reviews = view.runs[0].agents[0]
                .provider_calls
                .iter()
                .filter(|call| call.phase == AgentPhase::ToolReview)
                .collect::<Vec<_>>();
            assert_eq!(reviews.len(), 2);
            assert!(reviews.iter().all(|call| call.input_tokens == Some(3)
                && call.output_tokens == Some(1)
                && call.disposition == arany::ProviderCallDisposition::Finished));
        }
        if case == "scope-auto" {
            let reviews = requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .collect::<Vec<_>>();
            assert_eq!(reviews.len(), 1, "in-scope command gets one AI review");
            let payload: serde_json::Value =
                serde_json::from_str(&reviews[0].input.objective).unwrap();
            assert_eq!(
                payload["proposed_action"]["call"],
                serde_json::to_value(&call).unwrap(),
                "out-of-plan integration never reaches the AI judge"
            );
        }
        assert_eq!(
            tools.len(),
            if rejected.is_some() {
                1
            } else if case == "nonzero" {
                3
            } else {
                2
            },
            "{case}: effect stages: {tools:?}"
        );
        let command = tools[0].observation.as_ref().unwrap();
        assert_eq!(command.intent.call, call);
        assert!(
            command.attestation.is_some(),
            "{case}: native command receipt"
        );
        assert_eq!(
            command.disposition,
            rejected.unwrap_or(if case == "nonzero" {
                ToolDisposition::Failed
            } else {
                ToolDisposition::Succeeded
            }),
            "{case}: command capture"
        );
        if rejected.is_none() {
            let result: serde_json::Value = serde_json::from_str(&command.output).unwrap();
            assert_eq!(result["exit_code"], if case == "nonzero" { 7 } else { 0 });
            assert_eq!(result["stdout"], "payload-complete");
            assert_eq!(result["stderr"], "");
            if case == "selection" {
                assert_eq!(
                    result["selection"],
                    json!({"ignored_entries":1,"pruned_directories":1,"oversized_inputs":1,"discarded_outputs":1})
                );
                assert!(!workspace.join("src/ignored/new").exists());
                assert_eq!(
                    std::fs::read(workspace.join("src/ignored/secret")).unwrap(),
                    b"omitted input canary"
                );
            }
            let ToolCall::ApplyChangeset {
                source:
                    arany::worker::ChangesetSource::CommandReceipt {
                        source_intent_digest,
                        changeset,
                    },
            } = &tools[1].intent.call
            else {
                panic!("{case}: missing integration intent");
            };
            let digest: [u8; 32] =
                Sha256::digest(serde_json::to_vec(&command.intent).unwrap()).into();
            assert_eq!(*source_intent_digest, digest);
            assert_eq!(
                serde_json::to_value(changeset).unwrap(),
                result["changeset"]
            );
            let integration = tools[1].observation.as_ref().unwrap();
            assert_eq!(
                integration.disposition,
                if case == "partial" {
                    ToolDisposition::Uncertain
                } else if human_denied || matches!(case, "scope" | "scope-auto") {
                    ToolDisposition::Denied
                } else if case == "conflict" {
                    ToolDisposition::Conflict
                } else {
                    ToolDisposition::Succeeded
                },
                "{case}: publication outcome"
            );
            assert_eq!(
                integration.attestation.is_some(),
                !human_denied && !matches!(case, "scope" | "scope-auto")
            );
        }
        if native_setid {
            let result: serde_json::Value = serde_json::from_str(&command.output).unwrap();
            assert_eq!(result["exit_code"], 1);
            assert_eq!(result["stdout"], "");
            assert!(
                result["stderr"]
                    .as_str()
                    .unwrap()
                    .contains("Operation not permitted")
            );
            assert_eq!(result["changeset"]["files"], json!([]));
        }
        if !stopped {
            assert_eq!(
                requests
                    .last()
                    .unwrap()
                    .input
                    .tools
                    .as_ref()
                    .unwrap()
                    .observations()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>(),
                tools
                    .iter()
                    .map(|tool| tool.observation.as_ref().unwrap().clone())
                    .collect::<Vec<_>>(),
                "{case}: exact continuation evidence"
            );
        }
        if case == "partial" {
            assert_eq!(
                std::fs::read(workspace.join("src/a/created")).unwrap(),
                b"first\n"
            );
            assert!(!workspace.join("src/z/created").exists());
            assert_eq!(
                std::fs::read(workspace.join("src/m-modified")).unwrap(),
                b"before\n"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/z-deleted")).unwrap(),
                b"delete\n"
            );
            assert!(view.runs[0].assistant_message.is_none());
        } else if matches!(case, "nonzero" | "ai-approve" | "selection") {
            if case == "nonzero" {
                assert_eq!(
                    requests[2]
                        .input
                        .tools
                        .as_ref()
                        .unwrap()
                        .observations()
                        .len(),
                    2
                );
                let fresh = tools[2].observation.as_ref().unwrap();
                assert_eq!(fresh.disposition, ToolDisposition::Succeeded);
                assert!(fresh.attestation.is_some());
                let result: serde_json::Value = serde_json::from_str(&fresh.output).unwrap();
                assert_eq!(result["exit_code"], 0);
                assert_eq!(result["stdout"], "fresh-binary-snapshot");
                assert_eq!(result["stderr"], "");
                assert_eq!(result["changeset"]["files"], json!([]));
            }
            assert_eq!(
                std::fs::read(workspace.join("src/a-created")).unwrap(),
                [0, 255]
            );
            assert_ne!(
                std::fs::metadata(workspace.join("src/a-created"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
            assert_eq!(
                std::fs::read(workspace.join("src/m-modified")).unwrap(),
                b"modified\n"
            );
            assert!(
                !workspace.join("src/z-deleted").exists(),
                "{case}: delete publication receipts: {:?}",
                &tools[..2]
            );
        } else {
            assert!(
                !workspace.join("src/a-created").exists(),
                "{case}: no sibling publication"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/m-modified")).unwrap(),
                if case == "conflict" {
                    b"concurrent\n".as_slice()
                } else {
                    b"before\n".as_slice()
                },
                "{case}"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/z-deleted")).unwrap(),
                b"delete\n",
                "{case}"
            );
            assert!(!workspace.join("src/hostile").exists());
            assert!(!workspace.join("src/.git").exists());
            assert!(!workspace.join("src/overflow").exists());
            assert!(!workspace.join("src/extra-0").exists());
            assert!(!workspace.join("unselected").exists());
        }
    }
}

#[test]
fn exec_tool_configuration_rejects_before_workspace_content_or_provider_egress() {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let omitted = temp.path().join("omitted");
    let private = |path: &std::path::Path, bytes: &[u8]| {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    private(&omitted, b"OMITTED_CONFIG_CANARY");
    symlink(&omitted, workspace.join("AGENTS.md")).unwrap();
    for (name, bytes, expected) in [
        ("missing", None, "tools require a private tools.json; see docs/tools.md"),
        ("unknown", Some(b"{\"version\":1,\"workspace_paths\":[\"src\"],\"write\":false,\"commands\":[],\"skills\":[],\"mcp\":[],\"network\":true}".as_slice()), "Tool configuration is invalid or unsafe"),
        ("duplicates", Some(b"{\"version\":1,\"version\":1}".as_slice()), "Tool configuration is invalid or unsafe"),
    ] {
        let state_path = temp.path().join(format!("state-{name}"));
        StateRoot::admit(&state_path).unwrap();
        if let Some(bytes) = bytes { private(&state_path.join("tools.json"), bytes); }
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
        command.env_clear().current_dir(temp.path()).env("OPENAI_API_KEY", "synthetic-never-sent-key")
            .args(["exec", "--tools", "--provider", "openai", "--model", "gpt-5.4", "--state-dir"]).arg(&state_path)
            .arg("--workspace").arg(&workspace).arg("Must reject before hostile Workspace input");
        let output = super::process_output_before_deadline(command);
        assert_eq!(output.status.code(), Some(1), "configuration case {name}");
        assert!(output.stdout.is_empty());
        assert_eq!(String::from_utf8(output.stderr).unwrap(), format!("error: {expected}\n"));
        let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM events", [], |row| row.get::<_,i64>(0)).unwrap(), 0);
        assert_eq!(std::fs::read(&omitted).unwrap(), b"OMITTED_CONFIG_CANARY");
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
fn assert_refused_integration_continues(
    case: &str,
    run: &arany::RunView,
    requests: &[arany::ProviderRequest],
) {
    let label = match case {
        "deny" => "user_declined",
        "drop" => "channel_unavailable",
        _ => return,
    };
    assert_eq!(
        run.failure,
        Some(arany::RunFailure::CriterionUnmet),
        "{case}"
    );
    assert!(
        run.assistant_message.is_none(),
        "{case}: criterion remains unmet"
    );
    assert_eq!(
        run.agents[0].provider_calls.last().unwrap().disposition,
        arany::ProviderCallDisposition::Finished,
        "{case}: explanatory Provider call completes"
    );
    let observations = requests
        .last()
        .unwrap()
        .input
        .tools
        .as_ref()
        .unwrap()
        .observations();
    assert_eq!(
        observations.len(),
        2,
        "{case}: no repeated command or integration"
    );
    assert_eq!(observations[1].intent, run.tools[1].intent);
    assert_eq!(
        observations[1].output,
        format!("{label}: no operation was dispatched")
    );
    assert_eq!(observations[1].disposition, ToolDisposition::Denied);
    assert!(observations[1].attestation.is_none());
}
