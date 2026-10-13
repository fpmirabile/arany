use arany::{AgentPhase, CollaborationPolicy, RunStatus, SessionView, ToolCall, ToolDisposition};
use sha2::{Digest, Sha256};

#[test]
fn tool_journal_transition_corpus_rejects_unmatched_or_reusable_effects() {
    use arany::{
        AgentDisposition, AgentRole, AgentRunId, Event, EventEnvelope, OutputTokenBound,
        ProviderCallDisposition, ProviderCallRecord, RunConfig, RunDisposition, RunId, SessionId,
    };
    let session = SessionId::new();
    let run = RunId::new();
    let primary = AgentRunId::new();
    let policy = arany::ToolPolicyReceipt {
        contract_version: 1,
        config_digest: [1; 32],
        max_tool_calls: 16,
        max_model_steps: 32,
        max_context_bytes: 65536,
    };
    let config = RunConfig {
        provider: "scripted".into(),
        model: "replay-model".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: None,
        chatgpt_provenance: None,
        output_token_bound: OutputTokenBound::ProviderEnforced,
        policy: CollaborationPolicy::Single,
        output_token_cap: 4096,
        provider_concurrency: 1,
        workspace_device: 1,
        workspace_inode: 2,
        instruction_digest: None,
        include_digests: Vec::new(),
        history_run_ids: Vec::new(),
        excluded_history_runs: 0,
        context_usage: None,
        compaction_event_sequence: None,
        compaction_content_digest: None,
        tool_policy: Some(policy),
    };
    let intent = arany::EffectIntent {
        id: uuid::Uuid::now_v7(),
        run_id: run,
        agent_run_id: primary,
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::List { path: "src".into() },
        limits: arany::ToolLimits::default(),
        expires_at_ms: 1,
        use_count: 1,
    };
    let observation = arany::ToolObservation {
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output: "historical observation".into(),
        guard: Some(arany::GuardReceipt {
            contract_version: 1,
            intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(),
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        }),
    };
    let record = |disposition| Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: primary,
        record: ProviderCallRecord {
            phase: AgentPhase::RootPlan,
            disposition,
            response_id: None,
            input_tokens: None,
            output_tokens: None,
            wire_provenance: None,
            failure_reason: None,
        },
    };
    let prefix = [
        Event::SessionStarted {
            title: "Historical tool facts".into(),
            workspace_identity: (1, 2),
        },
        Event::MessageAccepted {
            run_id: run,
            text: "Past work".into(),
            images: Vec::new(),
        },
        Event::RunStarted {
            run_id: run,
            config,
        },
        Event::AgentSpawned {
            run_id: run,
            agent_run_id: primary,
            role: AgentRole::Primary,
            ordinal: 0,
            objective: None,
        },
        record(ProviderCallDisposition::ToolRequested),
        Event::ToolStarted {
            run_id: run,
            agent_run_id: primary,
            intent,
        },
        Event::ToolFinished {
            run_id: run,
            agent_run_id: primary,
            observation,
        },
    ];
    let suffix = vec![
        record(ProviderCallDisposition::Finished),
        Event::AgentFinished {
            run_id: run,
            agent_run_id: primary,
            disposition: AgentDisposition::Finished,
            summary: Some("Past summary".into()),
            result: Some("Past result".into()),
        },
        Event::MessageCommitted {
            run_id: run,
            text: "Past result".into(),
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Finished,
        },
    ];
    let replay = |events: Vec<Event>| {
        let envelopes: Vec<_> = events
            .into_iter()
            .enumerate()
            .map(|(index, event)| {
                let scope = match &event {
                    Event::SessionStarted { .. } => (None, None),
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
    let mut review = record(ProviderCallDisposition::Finished);
    if let Event::ProviderCallRecorded { record, .. } = &mut review {
        record.phase = AgentPhase::ToolReview;
        record.input_tokens = Some(3);
        record.output_tokens = Some(1);
    }
    let mut reviewed = all.clone();
    reviewed.insert(6, review.clone());
    let view = replay(reviewed.clone()).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(
        view.runs[0].agents[0].provider_calls[1].input_tokens,
        Some(3)
    );
    for case in 0..6 {
        let mut row = reviewed.clone();
        match case {
            0 => {
                row.remove(6);
                row.insert(5, review.clone());
            }
            1 => {
                row.remove(6);
                row.insert(7, review.clone());
            }
            2 => row.insert(7, review.clone()),
            3 => {
                row.remove(8);
            }
            4 | 5 => {
                if let Event::ProviderCallRecorded { record, .. } = &mut row[6] {
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
    let interrupted = replay(prefix[..6].to_vec()).unwrap().unwrap();
    assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
    assert!(interrupted.runs[0].tools[0].observation.is_none());
    for case in 0..11 {
        let mut row = all.clone();
        match case {
            0 => {
                row.remove(5);
            }
            1 => row.insert(7, row[6].clone()),
            2 => {
                if let Event::ToolStarted { intent, .. } = &mut row[5] {
                    intent.policy_digest = [9; 32];
                }
            }
            3 => {
                if let Event::ToolStarted { intent, .. } = &mut row[5] {
                    intent.workspace_inode += 1;
                }
            }
            4 => {
                if let Event::ToolStarted { intent, .. } = &mut row[5] {
                    intent.use_count = 2;
                }
            }
            5 => {
                if let Event::ToolFinished { observation, .. } = &mut row[6] {
                    observation.guard.as_mut().unwrap().intent_digest = [9; 32];
                }
            }
            6 => {
                if let Event::ToolFinished { observation, .. } = &mut row[6] {
                    observation.disposition = ToolDisposition::Uncertain;
                }
            }
            7 => {
                if let Event::ToolFinished { observation, .. } = &mut row[6] {
                    observation.disposition = ToolDisposition::Cancelled;
                }
            }
            8 => {
                if let Event::ToolStarted {
                    agent_run_id,
                    intent,
                    ..
                } = &mut row[5]
                {
                    *agent_run_id = AgentRunId::new();
                    intent.agent_run_id = *agent_run_id;
                }
            }
            9 => {
                if let Event::RunStarted { config, .. } = &mut row[2] {
                    config.tool_policy = None;
                }
            }
            10 => {
                if let Event::ToolFinished { observation, .. } = &mut row[6] {
                    observation.disposition = ToolDisposition::Failed;
                    observation.guard = None;
                }
            }
            _ => unreachable!(),
        }
        assert!(replay(row).is_err(), "Tool journal transition {case}");
    }
    for (guarded, later_failure) in [(true, true), (false, true), (false, false)] {
        let mut denied = prefix.to_vec();
        if let Event::ToolFinished { observation, .. } = &mut denied[6] {
            observation.disposition = ToolDisposition::Denied;
            observation.output = "UNTRUSTED_TOOL_ERROR_CANARY".into();
            if !guarded {
                observation.guard = None;
            }
        }
        if later_failure {
            let mut failure = record(ProviderCallDisposition::InvalidResponse);
            if let Event::ProviderCallRecorded { record, .. } = &mut failure {
                record.failure_reason = Some(arany::ProviderFailureReason::OutcomeContract);
            }
            denied.push(failure);
        }
        denied.extend([
            Event::AgentFinished {
                run_id: run,
                agent_run_id: primary,
                disposition: AgentDisposition::Failed,
                summary: None,
                result: None,
            },
            Event::RunFinished {
                run_id: run,
                disposition: RunDisposition::Failed,
            },
        ]);
        let view = replay(denied).unwrap().unwrap();
        let feedback = arany::render_run_feedback(&view.runs[0]).unwrap();
        if later_failure {
            assert!(
                feedback.contains("structured outcome contract"),
                "recoverable denial must not mask the later Provider failure"
            );
            assert!(!feedback.contains("approval expired"));
        } else {
            assert!(feedback.contains("Action denied or approval expired"));
        }
        assert!(!feedback.contains("CANARY"));
    }
    let mut observed_failure = all.clone();
    if let Event::ToolFinished { observation, .. } = &mut observed_failure[6] {
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
    let mut command_intent = match &prefix[5] {
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
        source_intent_digest: command_digest,
        changeset: changeset.clone(),
    };
    let observed = |intent: arany::EffectIntent, disposition, output: String| Event::ToolFinished {
        run_id: run,
        agent_run_id: primary,
        observation: arany::ToolObservation {
            disposition,
            output,
            guard: Some(arany::GuardReceipt {
                contract_version: 1,
                intent_digest: Sha256::digest(serde_json::to_vec(&intent).unwrap()).into(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            }),
            intent,
        },
    };
    let command_result = serde_json::json!({
        "exit_code":7,"stdout":"","stderr":"","changeset":changeset
    })
    .to_string();
    let mut integrated = prefix[..5].to_vec();
    integrated.extend([
        Event::ToolStarted {
            run_id: run,
            agent_run_id: primary,
            intent: command_intent.clone(),
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
    assert_eq!(view.runs[0].agents[0].provider_calls.len(), 2);
    let mut reviewed_integration = integrated.clone();
    reviewed_integration.insert(6, review.clone());
    reviewed_integration.insert(9, review.clone());
    let view = replay(reviewed_integration).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(view.runs[0].agents[0].provider_calls.len(), 4);
    let mut duplicate_review = integrated.clone();
    duplicate_review.splice(8..8, [review.clone(), review.clone()]);
    assert!(
        replay(duplicate_review).is_err(),
        "integration admits one review even when its command had none"
    );
    let interrupted = replay(integrated[..8].to_vec()).unwrap().unwrap();
    assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
    assert!(interrupted.runs[0].tools[1].observation.is_none());
    let mut unpublished = integrated[..7].to_vec();
    unpublished.extend([
        Event::AgentFinished {
            run_id: run,
            agent_run_id: primary,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Failed,
        },
    ]);
    let view = replay(unpublished).unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Failed);
    assert_eq!(view.runs[0].tools.len(), 1);
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
                let Event::ToolStarted { intent, .. } = &mut row[7] else {
                    unreachable!()
                };
                let ToolCall::ApplyChangeset {
                    source_intent_digest,
                    changeset,
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
                let Event::ToolFinished { observation, .. } = &mut row[8] else {
                    unreachable!()
                };
                observation.intent = altered;
                observation.guard.as_mut().unwrap().intent_digest =
                    Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
            }
            "missing-command" => {
                row.drain(5..7);
            }
            "unattested" | "denied" => {
                let Event::ToolFinished { observation, .. } = &mut row[6] else {
                    unreachable!()
                };
                if case == "unattested" {
                    observation.guard = None;
                } else {
                    observation.disposition = ToolDisposition::Denied;
                }
            }
            "reused" => {
                let mut reuse = integrated[7..9].to_vec();
                let Event::ToolStarted { intent, .. } = &mut reuse[0] else {
                    unreachable!()
                };
                intent.id = uuid::Uuid::now_v7();
                let altered = intent.clone();
                let Event::ToolFinished { observation, .. } = &mut reuse[1] else {
                    unreachable!()
                };
                observation.intent = altered;
                observation.guard.as_mut().unwrap().intent_digest =
                    Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
                row.splice(9..9, reuse);
            }
            "model-request" => {
                row.insert(7, record(ProviderCallDisposition::ToolRequested));
            }
            "new-inference" => row.insert(7, record(ProviderCallDisposition::Finished)),
            "omitted-integration" => {
                row.drain(7..9);
            }
            _ => unreachable!(),
        }
        assert!(
            replay(row).is_err(),
            "integration journal correlation: {case}"
        );
    }
    for pairs in [8, 9] {
        let mut rows = prefix[..4].to_vec();
        for _ in 0..pairs {
            let mut command = command_intent.clone();
            command.id = uuid::Uuid::now_v7();
            let mut integration = integration_intent.clone();
            integration.id = uuid::Uuid::now_v7();
            let ToolCall::ApplyChangeset {
                source_intent_digest,
                ..
            } = &mut integration.call
            else {
                unreachable!()
            };
            *source_intent_digest = Sha256::digest(serde_json::to_vec(&command).unwrap()).into();
            rows.extend([
                record(ProviderCallDisposition::ToolRequested),
                Event::ToolStarted {
                    run_id: run,
                    agent_run_id: primary,
                    intent: command.clone(),
                },
                observed(command, ToolDisposition::Failed, command_result.clone()),
                Event::ToolStarted {
                    run_id: run,
                    agent_run_id: primary,
                    intent: integration.clone(),
                },
                observed(
                    integration,
                    ToolDisposition::Succeeded,
                    "{\"applied\":1}".into(),
                ),
            ]);
        }
        rows.extend(suffix.clone());
        assert_eq!(
            replay(rows).is_ok(),
            pairs == 8,
            "integration consumes an effect slot: {pairs}"
        );
    }
    for (count, output, accepted) in [
        (16, "", true),
        (17, "", false),
        (16, &"x".repeat(8192), false),
    ] {
        let mut rows = prefix[..4].to_vec();
        for _ in 0..count {
            let mut start = prefix[5].clone();
            let Event::ToolStarted { intent, .. } = &mut start else {
                unreachable!()
            };
            intent.id = uuid::Uuid::now_v7();
            let mut result = prefix[6].clone();
            let Event::ToolFinished { observation, .. } = &mut result else {
                unreachable!()
            };
            observation.intent = intent.clone();
            observation.output = output.into();
            observation.guard.as_mut().unwrap().intent_digest =
                Sha256::digest(serde_json::to_vec(intent).unwrap()).into();
            rows.extend([
                record(ProviderCallDisposition::ToolRequested),
                start,
                result,
            ]);
        }
        rows.extend(suffix.clone());
        assert_eq!(
            replay(rows).is_ok(),
            accepted,
            "aggregate Tool journal bound"
        );
    }
}

#[cfg(any(target_os = "linux", all(target_os = "macos", debug_assertions)))]
pub(super) async fn native_command_changeset_corpus(
    root: &std::path::Path,
    config: &serde_json::Value,
    program: &str,
) {
    use arany::{
        ApprovalMode, Engine, Finish, Provider, ProviderError, ProviderOutcome, ProviderRequest,
        ProviderResponse, RunCancellation, RunProgress, RunRequest, StateRoot, Store,
        ToolApprovals,
    };
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    struct CommandJourney {
        calls: Vec<ToolCall>,
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
        review: bool,
    }
    impl Provider for CommandJourney {
        fn profile_name(&self) -> &str {
            "scripted"
        }
        fn model_name(&self) -> &str {
            "changeset-model"
        }
        fn max_concurrent_calls(&self) -> u8 {
            1
        }
        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let mut requests = self.requests.lock().unwrap();
            let review = request.phase == AgentPhase::ToolReview;
            let step = requests
                .iter()
                .filter(|request| request.phase != AgentPhase::ToolReview)
                .count();
            if step > self.calls.len() || review && !self.review {
                return Err(ProviderError::InvalidOutcome);
            }
            let outcome = if review {
                ProviderOutcome::Finish(Finish {
                    summary: "Approved exact action".into(),
                    result: "approve".into(),
                })
            } else {
                self.calls.get(step).cloned().map_or_else(
                    || {
                        ProviderOutcome::Finish(Finish {
                            summary: "Observed changeset".into(),
                            result: "Observed both effect stages.".into(),
                        })
                    },
                    ProviderOutcome::Tool,
                )
            };
            requests.push(request);
            Ok(ProviderResponse {
                outcome,
                response_id: None,
                input_tokens: review.then_some(3),
                output_tokens: review.then_some(1),
                wire_provenance: None,
            })
        }
    }
    for (case, suffix, rejected) in [
        ("nonzero", "exit 7", None),
        ("ai-approve", "", None),
        ("deny", "", None),
        ("drop", "", None),
        ("cancel", "", None),
        ("conflict", "", None),
        ("scope", "printf forbidden > unselected", None),
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
        ("budget", "", None),
    ] {
        let case_root = root.join(format!("changeset-{case}"));
        let workspace = case_root.join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        crate::private_file(&workspace.join("src/m-modified"), b"before\n");
        crate::private_file(&workspace.join("src/z-deleted"), b"delete\n");
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
        config["workspace_paths"] = json!(["src"]);
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
        if case == "budget" {
            calls.splice(
                0..0,
                std::iter::repeat_n(ToolCall::List { path: "src".into() }, 15),
            );
        }
        if case == "nonzero" {
            calls.push(ToolCall::Command {
                program: program.into(),
                args: vec!["-c".into(), "set -eu; test -x src/a-created; printf '\\000\\377' | cmp - src/a-created; test \"$(cat src/m-modified)\" = modified; test ! -e src/z-deleted; printf fresh-binary-snapshot".into()],
                cwd: "".into(),
            });
        }
        let mut engine = Engine::open(
            state,
            CommandJourney {
                calls,
                requests: requests.clone(),
                review: case == "ai-approve",
            },
        )
        .unwrap();
        engine
            .enable_tools(env!("CARGO_BIN_EXE_arany").into())
            .unwrap();
        let interactive = matches!(case, "deny" | "drop" | "cancel" | "conflict");
        let (approvals, mut inbox) = ToolApprovals::new(if case == "ai-approve" {
            ApprovalMode::Auto
        } else {
            ApprovalMode::Request
        });
        if interactive || case == "ai-approve" {
            engine.set_tool_approvals(approvals);
        }
        let cancellation = RunCancellation::new();
        let pending = cancellation.clone();
        let decision = async {
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
                source_intent_digest,
                changeset,
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
                engine.run_with_progress(
                    RunRequest {
                        session_id: None,
                        title: Some(format!("Changeset {case}")),
                        objective: "Exercise native changeset integration".into(),
                        images: Vec::new(),
                        workspace: workspace.clone(),
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Single,
                    },
                    cancellation,
                    RunProgress::new()
                ),
                decision
            )
        })
        .await
        .unwrap_or_else(|_| panic!("{case}: native changeset deadline"));
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        let human_denied = matches!(case, "deny" | "drop" | "cancel");
        let stopped = human_denied || matches!(case, "partial" | "budget");
        assert_eq!(
            outcome.run.status,
            if case == "cancel" {
                RunStatus::Cancelled
            } else if stopped {
                RunStatus::Failed
            } else {
                RunStatus::Finished
            },
            "{case}: {:?}",
            outcome.run.tools
        );
        let requests = requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            if stopped {
                if case == "budget" { 16 } else { 1 }
            } else if case == "nonzero" {
                3
            } else if case == "ai-approve" {
                4
            } else {
                2
            },
            "{case}: no retry"
        );
        for request in &requests {
            assert_eq!(request.model, "changeset-model");
            if request.phase == AgentPhase::ToolReview {
                assert!(
                    request.tools.is_none()
                        && request.includes.is_empty()
                        && request.images.is_empty()
                        && request.history.is_empty()
                        && request.child_results.is_empty()
                        && request.context_summary.is_none()
                );
                assert!(request.instructions.is_some());
                assert_eq!(request.max_output_tokens, 256);
                let payload: serde_json::Value = serde_json::from_str(&request.objective).unwrap();
                assert_eq!(
                    payload["user_request"],
                    "Exercise native changeset integration"
                );
                continue;
            }
            assert_eq!(request.phase, AgentPhase::RootPlan);
            assert_eq!(request.objective, "Exercise native changeset integration");
            assert!(
                request.instructions.is_none()
                    && request.history.is_empty()
                    && request.includes.is_empty()
                    && request.context_summary.is_none()
                    && request.child_results.is_empty()
                    && request.images.is_empty()
            );
            assert_eq!(request.max_output_tokens, 4096);
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        store.close().await.unwrap();
        let view = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(view.runs[0], outcome.run, "{case}: closed replay");
        assert!(
            requests
                .iter()
                .all(|request| request.run_id == view.runs[0].id
                    && request.agent_run_id == view.runs[0].agents[0].id)
        );
        let tools = &view.runs[0].tools;
        if case == "ai-approve" {
            let reviews = requests
                .iter()
                .filter(|request| request.phase == AgentPhase::ToolReview)
                .collect::<Vec<_>>();
            assert_eq!(reviews.len(), 2);
            for (request, tool) in reviews.iter().zip(tools) {
                let payload: serde_json::Value = serde_json::from_str(&request.objective).unwrap();
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
        if case == "budget" {
            assert_eq!(
                tools.len(),
                15,
                "command reserves its integration slot before dispatch"
            );
            assert!(
                tools
                    .iter()
                    .all(|tool| matches!(tool.intent.call, ToolCall::List { .. })
                        && tool.observation.as_ref().unwrap().disposition
                            == ToolDisposition::Succeeded
                        && tool.observation.as_ref().unwrap().guard.is_some())
            );
            assert_eq!(
                requests
                    .last()
                    .unwrap()
                    .tools
                    .as_ref()
                    .unwrap()
                    .observations
                    .len(),
                15
            );
            assert!(requests.iter().enumerate().all(|(index, request)| {
                request.tools.as_ref().unwrap().observations.len() == index
            }));
            assert!(view.runs[0].assistant_message.is_none());
            assert!(!workspace.join("src/a-created").exists());
            assert_eq!(
                std::fs::read(workspace.join("src/m-modified")).unwrap(),
                b"before\n"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/z-deleted")).unwrap(),
                b"delete\n"
            );
            continue;
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
        assert!(command.guard.is_some(), "{case}: native command receipt");
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
            let ToolCall::ApplyChangeset {
                source_intent_digest,
                changeset,
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
                } else if human_denied || case == "scope" {
                    ToolDisposition::Denied
                } else if case == "conflict" {
                    ToolDisposition::Conflict
                } else {
                    ToolDisposition::Succeeded
                },
                "{case}: publication outcome"
            );
            assert_eq!(
                integration.guard.is_some(),
                !human_denied && case != "scope"
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
                    .tools
                    .as_ref()
                    .unwrap()
                    .observations,
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
        } else if matches!(case, "nonzero" | "ai-approve") {
            if case == "nonzero" {
                assert_eq!(requests[1].tools.as_ref().unwrap().observations.len(), 2);
                let fresh = tools[2].observation.as_ref().unwrap();
                assert_eq!(fresh.disposition, ToolDisposition::Succeeded);
                assert!(fresh.guard.is_some());
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
