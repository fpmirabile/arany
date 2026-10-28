use super::super::super::test_support::*;
use super::super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::CollaborationPolicy;
use crate::session::{ProviderSetupDisposition, ProviderSetupTarget, ProviderSetupTargetKind};
use crate::tools::{CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation};

fn assert_setup_recovery_with_open_lifecycle_hook(
    session_id: SessionId,
    goal_id: GoalId,
    prefix: &[Event],
) {
    for disposition in [
        ProviderSetupDisposition::Applied,
        ProviderSetupDisposition::Failed,
        ProviderSetupDisposition::Cancelled,
        ProviderSetupDisposition::Uncertain,
    ] {
        let operation_id = uuid::Uuid::now_v7();
        let mut events = prefix.to_vec();
        events.push(Event::ProviderSetupEditStarted {
            operation_id,
            target: ProviderSetupTarget {
                kind: ProviderSetupTargetKind::CustomProfile,
                identity: "example-route".into(),
            },
            expected_digest: None,
        });
        let pending = SessionView::replay_open(session_id, &journal(session_id, events.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(pending.pending_provider_setup_edit, Some(operation_id));
        events.push(Event::ProviderSetupEditFinished {
            operation_id,
            resulting_digest: (disposition == ProviderSetupDisposition::Applied).then_some([1; 32]),
            disposition,
        });
        let journal = journal(session_id, events);
        let mut view = SessionView::replay_open(session_id, &journal)
            .expect("setup outcome after an unfinished lifecycle hook")
            .unwrap();
        assert_eq!(view.pending_provider_setup_edit, None);
        assert_eq!(
            view.provider_setup_uncertain,
            disposition == ProviderSetupDisposition::Uncertain
        );
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert!(view.runs[0].lifecycle_hooks[0].outcome.is_none());
        assert_eq!(view.goal(goal_id).unwrap().status, GoalStatus::Interrupted);
        assert!(!view.goal(goal_id).unwrap().can_resume(&view));
        let recovered = SessionView::replay(session_id, &journal).unwrap().unwrap();
        assert_eq!(recovered.pending_provider_setup_edit, None);
        assert_eq!(
            recovered.provider_setup_uncertain,
            view.provider_setup_uncertain
        );
        let renamed = view.apply_committed(&EventEnvelope {
            sequence: view.last_sequence + 1,
            session_id,
            run_id: None,
            agent_run_id: None,
            event: Event::SessionRenamed {
                title: "Renamed".into(),
            },
            created_at_ms: 1,
        });
        assert_eq!(
            renamed.is_ok(),
            disposition != ProviderSetupDisposition::Uncertain
        );
    }
}

#[test]
fn crash_with_an_open_lifecycle_hook_leaves_the_session_usable() {
    use crate::tools::permissions::{HookEvent, HookInvocation, stop_subject};
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut pinned = config(CollaborationPolicy::Single, true);
    pinned.tool_policy.as_mut().unwrap().permissions_digest = Some([3; 32]);
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
    events.extend(start_run(goal_id, run_id, agent_run_id, pinned));
    events.extend([
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms: 7_000,
        },
        Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                Some(0),
            ),
        },
        Event::HookStarted {
            run_id,
            agent_run_id,
            hook: HookInvocation {
                event: HookEvent::Stop,
                group: 0,
                handler: 0,
                subject_digest: stop_subject(agent_run_id, 0),
                intent: child_intent(
                    run_id,
                    agent_run_id,
                    ToolCall::Command {
                        program: "sh".into(),
                        args: vec!["-c".into(), "exit 2".into()],
                        cwd: String::new(),
                        resource_class: None,
                        timeout_ms: None,
                    },
                ),
                input_digest: [9; 32],
                active_ceiling_ms: Some(5_000),
            },
        },
    ]);
    let view = SessionView::replay(session_id, &journal(session_id, events.clone()))
        .expect("crash-left prefix")
        .expect("Session");
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert!(!view.runs[0].hooks_settled());
    assert!(!view.goal(goal_id).unwrap().can_resume(&view));
    assert_setup_recovery_with_open_lifecycle_hook(session_id, goal_id, &events);

    let next_goal = GoalId::new();
    events.push(goal_started(next_goal, GoalBudget::default()));
    events.extend(start_run(
        next_goal,
        RunId::new(),
        AgentRunId::new(),
        config(CollaborationPolicy::Single, true),
    ));
    let view = SessionView::replay_open(session_id, &journal(session_id, events))
        .expect("a new task after the crash")
        .expect("Session");
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
}

fn actor_tool(view: &mut SessionView, actor: AgentRunId) -> &mut ToolView {
    let run = &mut view.runs[0];
    if run.primary().is_some_and(|agent| agent.id == actor) {
        run.tools.last_mut().unwrap()
    } else {
        run.agents
            .iter_mut()
            .find(|agent| agent.id == actor)
            .unwrap()
            .tools
            .last_mut()
            .unwrap()
    }
}

#[test]
fn hook_events_replay_in_order_and_bind_the_verdict_to_their_outcomes() {
    use crate::tools::permissions::{
        Decision, HookEvent, HookIgnoreReason, HookInvocation, HookOutcome, HookResult,
        PermissionMode, PermissionSource, PermissionVerdict,
    };
    for researcher in [false, true] {
        let (mut view, goal, run, agent) = if researcher {
            research_session()
        } else {
            primary_tool_session(true)
        };
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(
            &mut view,
            if researcher {
                child_record(run, agent, ProviderCallDisposition::ToolRequested)
            } else {
                requesting_step(run, agent, 1)
            },
        )
        .unwrap();
        let intent = read_intent(run, agent, "src/lib.rs");
        let hook_intent = child_intent(
            run,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
            },
        );
        let started = |subject: [u8; 32], hook_intent: &EffectIntent| Event::HookStarted {
            run_id: run,
            agent_run_id: agent,
            hook: HookInvocation {
                active_ceiling_ms: None,
                event: HookEvent::PreToolUse,
                group: 0,
                handler: 0,
                subject_digest: subject,
                intent: hook_intent.clone(),
                input_digest: [9; 32],
            },
        };
        let finished = |observation: ToolObservation, result: HookResult| Event::HookFinished {
            run_id: run,
            agent_run_id: agent,
            outcome: HookOutcome {
                boundary_active_ms: None,
                observation,
                result,
            },
        };
        let deny = HookResult::Restricted {
            decision: Decision::Deny,
            reason: Some("blocked".into()),
        };
        let verdict = |decision, restriction, hooks, basis| Event::ToolStarted {
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
            run_id: run,
            agent_run_id: agent,
            verdict: Some(PermissionVerdict {
                host: None,
                intent_digest: intent.digest(),
                decision,
                mode: Some(PermissionMode::Default),
                basis,
                source: basis,
                approved: false,
                hook_restriction: restriction,
                hooks,
                narrowed: false,
            }),
        };
        assert!(rejected(&view, started(intent.digest(), &hook_intent)));
        apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
        let mut hook_observation = observation_for(hook_intent.clone(), b"");
        hook_observation.disposition = ToolDisposition::Failed;
        hook_observation.output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 2, "stdout": "", "stderr": "blocked", "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":7,"lines":1},"changeset": {"files": []}}).to_string();
        for event in [
            started(intent.digest(), &hook_intent),
            finished(hook_observation.clone(), deny.clone()),
        ] {
            let version = event.version();
            let payload = event.payload().unwrap();
            assert_eq!(
                Event::decode(event.kind(), version, &payload, Some(run), Some(agent)).unwrap(),
                event
            );
        }
        assert!(rejected(
            &view,
            finished(hook_observation.clone(), deny.clone())
        ));
        assert!(rejected(&view, started([4; 32], &hook_intent)));
        let mut foreign = hook_intent.clone();
        foreign.policy_digest = [8; 32];
        assert!(rejected(&view, started(intent.digest(), &foreign)));
        apply(&mut view, started(intent.digest(), &hook_intent)).unwrap();
        assert!(!view.runs[0].hooks_settled());
        assert!(
            rejected(&view, reserve_child_call(goal, run, agent)),
            "an open PreToolUse hook blocks the actor's review reservation"
        );
        let mut wrong_actor = started(intent.digest(), &hook_intent);
        if let Event::HookStarted { agent_run_id, .. } = &mut wrong_actor {
            *agent_run_id = AgentRunId::new();
        }
        assert!(rejected(&view, wrong_actor));
        let second = child_intent(run, agent, hook_intent.call.clone());
        assert!(rejected(&view, started(intent.digest(), &second)));
        assert!(rejected(
            &view,
            verdict(
                Decision::Deny,
                Some(Decision::Deny),
                1,
                PermissionSource::Hook
            )
        ));
        let other = observation_for(second.clone(), b"");
        assert!(rejected(&view, finished(other, deny.clone())));
        let ask_verdict = |source| Event::ToolStarted {
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
            run_id: run,
            agent_run_id: agent,
            verdict: Some(PermissionVerdict {
                host: None,
                intent_digest: intent.digest(),
                decision: Decision::Ask,
                mode: Some(PermissionMode::Auto),
                basis: PermissionSource::Mode,
                source,
                approved: true,
                hook_restriction: Some(Decision::Ask),
                hooks: 1,
                narrowed: false,
            }),
        };
        assert!(ask_verdict(PermissionSource::Human).payload().is_ok());
        assert!(ask_verdict(PermissionSource::AiReview).payload().is_err());
        let forged_pass = finished(hook_observation.clone(), HookResult::Passed);
        assert!(forged_pass.payload().is_err());
        let mid = view.clone();
        assert_refused_pre_hook_stop(
            mid.clone(),
            &hook_intent,
            verdict(
                Decision::Deny,
                Some(Decision::Deny),
                1,
                PermissionSource::Hook,
            ),
        );
        apply(&mut view, finished(hook_observation.clone(), deny.clone())).unwrap();
        assert!(rejected(&view, started(intent.digest(), &second)));
        for (decision, restriction, hooks, basis) in [
            (
                Decision::Deny,
                Some(Decision::Deny),
                0,
                PermissionSource::Hook,
            ),
            (
                Decision::Deny,
                Some(Decision::Deny),
                2,
                PermissionSource::Hook,
            ),
            (Decision::Allow, None, 1, PermissionSource::Mode),
            (Decision::Deny, None, 1, PermissionSource::Mode),
            (
                Decision::Ask,
                Some(Decision::Ask),
                1,
                PermissionSource::Hook,
            ),
        ] {
            assert!(
                rejected(&view, verdict(decision, restriction, hooks, basis)),
                "{decision:?} {restriction:?} {hooks}"
            );
        }
        let mut forged = view.clone();
        actor_tool(&mut forged, agent).hooks[0].outcome = Some(HookOutcome {
            boundary_active_ms: None,
            observation: hook_observation.clone(),
            result: HookResult::Passed,
        });
        assert!(rejected(
            &forged,
            verdict(
                Decision::Ask,
                Some(Decision::Ask),
                1,
                PermissionSource::Hook
            )
        ));
        assert!(rejected(
            &forged,
            verdict(
                Decision::Deny,
                Some(Decision::Deny),
                1,
                PermissionSource::Hook
            )
        ));
        let accepted = verdict(
            Decision::Deny,
            Some(Decision::Deny),
            1,
            PermissionSource::Hook,
        );
        apply(&mut view, accepted.clone()).unwrap();
        assert!(rejected(&view, accepted));

        let allow_restriction = finished(
            hook_observation.clone(),
            HookResult::Restricted {
                decision: Decision::Allow,
                reason: None,
            },
        );
        assert!(allow_restriction.payload().is_err());
        let started_event = started(intent.digest(), &hook_intent);
        assert!(started_event.payload().is_ok());
        assert!(
            started(
                intent.digest(),
                &child_intent(run, agent, read_intent(run, agent, "a").call)
            )
            .payload()
            .is_err()
        );
        let payload = verdict(
            Decision::Deny,
            Some(Decision::Deny),
            1,
            PermissionSource::Hook,
        )
        .payload()
        .unwrap();
        assert!(matches!(
            Event::decode("ToolStarted", 2, &payload, Some(run), Some(agent)),
            Err(ReplayError::UnknownEvent)
        ));

        // An unknown hook outcome stops the Run exactly like an uncertain Tool.
        let mut uncertain = observation_for(hook_intent.clone(), b"");
        uncertain.disposition = ToolDisposition::Uncertain;
        uncertain.attestation = None;
        let not_completed = HookResult::Ignored {
            why: HookIgnoreReason::NotCompleted,
        };
        // A Guard failure after dispatch carries the not-attempted cache outcome of any process.
        uncertain.cache_update = CacheUpdate::NotAttempted;
        let not_completed_event = finished(uncertain.clone(), not_completed.clone());
        assert!(not_completed_event.payload().is_ok());
        let mut after_uncertain = mid;
        apply(&mut after_uncertain, not_completed_event).unwrap();
        apply(
            &mut after_uncertain,
            verdict(
                Decision::Deny,
                Some(Decision::Deny),
                1,
                PermissionSource::Hook,
            ),
        )
        .unwrap();
        assert!(!actor_tool(&mut after_uncertain, agent).settled_without_stop());
        assert!(!after_uncertain.runs[0].hooks_settled());
        let mut stopped = view.clone();
        actor_tool(&mut stopped, agent).hooks[0].outcome = Some(HookOutcome {
            boundary_active_ms: None,
            observation: uncertain,
            result: not_completed,
        });
        assert!(!actor_tool(&mut stopped, agent).settled_without_stop());
    }
}

fn assert_refused_pre_hook_stop(mut view: SessionView, hook: &EffectIntent, mut candidate: Event) {
    use crate::tools::permissions::{HookOutcome, HookResult};
    let Event::ToolStarted {
        run_id,
        agent_run_id,
        intent,
        ..
    } = &candidate
    else {
        unreachable!()
    };
    let (run, actor, digest) = (
        *run_id,
        *agent_run_id,
        EffectIntent::from(intent.clone()).digest(),
    );
    let stdout = r#"{"continue":false,"stopReason":"halt"}"#;
    let mut observation = observation_for(hook.clone(), b"");
    observation.output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":stdout,"stderr":"","stdout_total":{"bytes":stdout.len(),"lines":1},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string();
    apply(
        &mut view,
        Event::HookFinished {
            run_id: run,
            agent_run_id: actor,
            outcome: HookOutcome {
                observation,
                result: HookResult::Stopped {
                    reason: Some("halt".into()),
                },
                boundary_active_ms: None,
            },
        },
    )
    .unwrap();
    assert!(crate::session::validate_tool_started(&view, &candidate).is_ok());
    if let Event::ToolStarted { intent, .. } = &mut candidate {
        intent.workspace_inode += 1;
    }
    let refusal = crate::session::validate_tool_started(&view, &candidate).unwrap_err();
    apply(
        &mut view,
        Event::ToolPreflightRefused {
            run_id: run,
            agent_run_id: actor,
            record: crate::session::ToolPreflightRefusal {
                intent_digest: digest,
                refusal,
                verdict: None,
            },
        },
    )
    .unwrap();
    assert!(rejected(&view, candidate));
    apply(
        &mut view,
        Event::AgentFinished {
            run_id: run,
            agent_run_id: actor,
            disposition: AgentDisposition::StoppedByHook,
            worker: None,
            summary: None,
            result: None,
        },
    )
    .unwrap();
    assert_eq!(
        view.runs[0]
            .agents
            .iter()
            .find(|agent| agent.id == actor)
            .unwrap()
            .status,
        AgentStatus::StoppedByHook
    );
}

#[test]
fn post_tool_use_hooks_need_a_committed_observation_and_feed_back_only_their_result() {
    use crate::tools::permissions::{
        Decision, HookEvent, HookInvocation, HookOutcome, HookResult, PermissionMode,
        PermissionSource, PermissionVerdict,
    };
    for researcher in [false, true] {
        let (mut view, goal, run, agent) = if researcher {
            research_session()
        } else {
            primary_tool_session(true)
        };
        if !researcher {
            view.runs[0].config.as_mut().unwrap().supervisor =
                Some(crate::supervisor::SupervisorPolicy::default());
            for _ in 0..2 {
                apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
                apply(&mut view, requesting_step(run, agent, 1)).unwrap();
                let intent = read_intent(run, agent, "src/lib.rs");
                apply(&mut view, tool_started(run, agent, &intent)).unwrap();
                apply(&mut view, tool_finished(run, agent, &intent, b"x")).unwrap();
            }
        }
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(
            &mut view,
            if researcher {
                child_record(run, agent, ProviderCallDisposition::ToolRequested)
            } else {
                requesting_step(run, agent, 1)
            },
        )
        .unwrap();
        let intent = read_intent(run, agent, "src/lib.rs");
        let hook_intent = child_intent(
            run,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
            },
        );
        let started = |event, subject: [u8; 32]| Event::HookStarted {
            run_id: run,
            agent_run_id: agent,
            hook: HookInvocation {
                active_ceiling_ms: None,
                event,
                group: 0,
                handler: 0,
                subject_digest: subject,
                intent: hook_intent.clone(),
                input_digest: [9; 32],
            },
        };
        let finished = |observation: ToolObservation, result: HookResult| Event::HookFinished {
            run_id: run,
            agent_run_id: agent,
            outcome: HookOutcome {
                boundary_active_ms: None,
                observation,
                result,
            },
        };
        apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
        assert!(rejected(
            &view,
            started(HookEvent::PostToolUse, intent.digest())
        ));
        apply(
            &mut view,
            Event::ToolStarted {
                intent: intent.clone().into(),
                active_ceiling_ms: 0,
                run_id: run,
                agent_run_id: agent,
                verdict: Some(PermissionVerdict {
                    host: None,
                    intent_digest: intent.digest(),
                    decision: Decision::Allow,
                    mode: Some(PermissionMode::Default),
                    basis: PermissionSource::Mode,
                    source: PermissionSource::Mode,
                    approved: true,
                    hook_restriction: None,
                    hooks: 0,
                    narrowed: false,
                }),
            },
        )
        .unwrap();
        apply(&mut view, tool_finished(run, agent, &intent, b"x")).unwrap();
        if !researcher {
            let verdict = recorded_supervisor_verdict(&view, agent);
            apply(&mut view, verdict).unwrap();
        }
        let mut later = view.clone();
        apply(&mut later, reserve_child_call(goal, run, agent)).unwrap();
        assert!(
            rejected(&later, started(HookEvent::PostToolUse, intent.digest())),
            "a hook after a later Provider call is forged ordering"
        );
        assert!(rejected(
            &view,
            started(HookEvent::PreToolUse, intent.digest())
        ));
        assert!(rejected(&view, started(HookEvent::PostToolUse, [4; 32])));
        apply(&mut view, started(HookEvent::PostToolUse, intent.digest())).unwrap();
        assert!(rejected(
            &view,
            started(HookEvent::PostToolUse, intent.digest())
        ));
        let continuation = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: record(
                if researcher {
                    AgentPhase::ChildWork
                } else {
                    AgentPhase::RootPlan
                },
                ProviderCallDisposition::Finished,
                Some(0),
            ),
        };
        assert!(
            rejected(&view, reserve_child_call(goal, run, agent)),
            "no Provider call is reserved while the actor's PostToolUse hook is open"
        );
        assert!(
            rejected(&view, continuation.clone()),
            "no continuation is recorded while a PostToolUse hook is open"
        );
        let mut hook_observation = observation_for(hook_intent.clone(), b"");
        hook_observation.disposition = ToolDisposition::Failed;
        hook_observation.output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code": 2, "stdout": "", "stderr": "blocked", "stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":7,"lines":1},"changeset": {"files": []}}).to_string();
        for forged in [
            HookResult::Passed,
            HookResult::Restricted {
                decision: Decision::Deny,
                reason: Some("blocked".into()),
            },
            HookResult::Feedback {
                lines: vec!["other".into()],
            },
        ] {
            assert!(
                rejected(&view, finished(hook_observation.clone(), forged.clone())),
                "{forged:?}"
            );
        }
        let feedback = HookResult::Feedback {
            lines: vec!["blocked".into()],
        };
        assert!(
            finished(hook_observation.clone(), feedback.clone())
                .payload()
                .is_ok()
        );
        let mut accepted = view.clone();
        apply(
            &mut accepted,
            finished(hook_observation.clone(), feedback.clone()),
        )
        .unwrap();
        let mut settled = accepted.clone();
        apply(&mut settled, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut settled, continuation).unwrap();
        let tool = actor_tool(&mut accepted, agent);
        assert_eq!(
            tool.hook_feedback(),
            vec!["hook feedback: blocked".to_owned()]
        );
        assert!(tool.settled_without_stop());

        let mut uncertain = observation_for(hook_intent.clone(), b"");
        uncertain.disposition = ToolDisposition::Uncertain;
        uncertain.attestation = None;
        uncertain.cache_update = CacheUpdate::NotAttempted;
        let unknown = HookResult::Ignored {
            why: crate::tools::permissions::HookIgnoreReason::NotCompleted,
        };
        apply(&mut view, finished(uncertain, unknown)).unwrap();
        assert!(!actor_tool(&mut view, agent).settled_without_stop());
        assert!(actor_tool(&mut view, agent).hook_feedback().is_empty());
    }
}

fn post_tool_hook(intent: &EffectIntent) -> crate::tools::permissions::HookInvocation {
    crate::tools::permissions::HookInvocation {
        active_ceiling_ms: None,
        event: crate::tools::permissions::HookEvent::PostToolUse,
        group: 0,
        handler: 0,
        subject_digest: intent.digest(),
        intent: child_intent(
            intent.run_id,
            intent.agent_run_id,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
            },
        ),
        input_digest: [9; 32],
    }
}

#[test]
fn no_tool_starts_while_a_post_tool_use_hook_is_open() {
    use crate::tools::permissions::{
        Decision, PermissionMode, PermissionSource, PermissionVerdict,
    };
    let (mut view, goal, run, agent) = primary_tool_session(true);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let (first, second) = (
        read_intent(run, agent, "src/lib.rs"),
        read_intent(run, agent, "src/lib.rs"),
    );
    apply(&mut view, tool_proposed(run, agent, &first)).unwrap();
    apply(
        &mut view,
        Event::ToolStarted {
            intent: first.clone().into(),
            active_ceiling_ms: 0,
            run_id: run,
            agent_run_id: agent,
            verdict: Some(PermissionVerdict {
                host: None,
                intent_digest: first.digest(),
                decision: Decision::Allow,
                mode: Some(PermissionMode::Default),
                basis: PermissionSource::Mode,
                source: PermissionSource::Mode,
                approved: true,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            }),
        },
    )
    .unwrap();
    apply(&mut view, tool_finished(run, agent, &first, b"x")).unwrap();
    let mut settled = view.clone();
    apply(&mut settled, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut settled, requesting_step(run, agent, 1)).unwrap();
    apply(&mut settled, tool_proposed(run, agent, &second)).unwrap();
    apply(
        &mut view,
        Event::HookStarted {
            run_id: run,
            agent_run_id: agent,
            hook: post_tool_hook(&first),
        },
    )
    .unwrap();
    assert!(rejected(&view, reserve_child_call(goal, run, agent)));
    assert!(rejected(&view, tool_proposed(run, agent, &second)));
}

#[test]
fn concurrent_read_post_hooks_stay_in_their_step_after_interleaved_finishes() {
    use crate::tools::permissions::{HookOutcome, HookResult};
    for researcher in [false, true] {
        let (mut view, goal, [first, second]) = settled_read_hook_pair(researcher, false);
        let (run, agent) = (first.run_id, first.agent_run_id);
        for source in [&first, &second] {
            let hook = post_tool_hook(source);
            let started = Event::HookStarted {
                run_id: run,
                agent_run_id: agent,
                hook: hook.clone(),
            };
            let mut next = view.clone();
            apply(&mut next, reserve_child_call(goal, run, agent)).unwrap();
            assert!(
                rejected(&next, started.clone()),
                "a new Provider reservation closes the hook window"
            );
            apply(&mut view, started)
                .expect("read hooks may follow sibling admission and finishes");
            apply(
                &mut view,
                Event::HookFinished {
                    run_id: run,
                    agent_run_id: agent,
                    outcome: HookOutcome {
                        boundary_active_ms: None,
                        observation: observation_for(hook.intent, b""),
                        result: HookResult::Passed,
                    },
                },
            )
            .unwrap();
        }
    }
}

fn settled_read_hook_pair(
    researcher: bool,
    reverse: bool,
) -> (SessionView, GoalId, [EffectIntent; 2]) {
    let (mut view, goal, run, agent) = if researcher {
        research_session()
    } else {
        primary_tool_session(true)
    };
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    let mut requested = if researcher {
        child_record(run, agent, ProviderCallDisposition::ToolRequested)
    } else {
        requesting_step(run, agent, 2)
    };
    let Event::ProviderCallRecorded { record, .. } = &mut requested else {
        unreachable!()
    };
    record.requested_tools = 2;
    apply(&mut view, requested).unwrap();
    let first = read_intent(run, agent, "src/lib.rs");
    let second = read_intent(run, agent, "src/lib.rs");
    for intent in [&first, &second] {
        apply(&mut view, tool_proposed(run, agent, intent)).unwrap();
    }
    let reads = [first, second];
    for intent in &reads {
        use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
        apply(
            &mut view,
            verdict_event(
                intent,
                Some(PermissionMode::Default),
                Decision::Allow,
                PermissionSource::Mode,
                PermissionSource::Mode,
                true,
            ),
        )
        .unwrap();
    }
    let open = view.runs[0]
        .tools
        .iter()
        .chain(view.runs[0].agents.iter().flat_map(|agent| &agent.tools))
        .filter(|tool| {
            tool.intent.agent_run_id == agent
                && tool.started_sequence.is_some()
                && tool.observation.is_none()
        })
        .count();
    assert_eq!(open, reads.len());
    for index in if reverse { [1, 0] } else { [0, 1] } {
        finish_in_process_read(&mut view, &reads[index]);
    }
    (view, goal, reads)
}

fn apply_post_hook(
    view: &mut SessionView,
    hook: crate::tools::permissions::HookInvocation,
) -> Result<(), ReplayError> {
    use crate::tools::permissions::{HookOutcome, HookResult};
    let (run_id, agent_run_id) = (hook.intent.run_id, hook.intent.agent_run_id);
    apply(
        view,
        Event::HookStarted {
            run_id,
            agent_run_id,
            hook: hook.clone(),
        },
    )?;
    apply(
        view,
        Event::HookFinished {
            run_id,
            agent_run_id,
            outcome: HookOutcome {
                boundary_active_ms: None,
                observation: observation_for(hook.intent, b""),
                result: HookResult::Passed,
            },
        },
    )
}

#[test]
fn concurrent_read_post_hook_sets_follow_completion_and_handler_order() {
    for researcher in [false, true] {
        for reverse in [false, true] {
            let (view, _, reads) = settled_read_hook_pair(researcher, reverse);
            for (name, handlers, accepted) in [
                (
                    "distinct handlers",
                    vec![(0, 0, 0), (0, 0, 1), (0, 1, 0), (1, 0, 0)],
                    true,
                ),
                ("repeated handler", vec![(0, 0, 0), (0, 0, 0)], false),
                ("reversed handlers", vec![(0, 0, 1), (0, 0, 0)], false),
                ("reversed groups", vec![(0, 1, 0), (0, 0, 1)], false),
                (
                    "reopened source",
                    vec![(0, 0, 0), (1, 0, 0), (0, 0, 1)],
                    false,
                ),
                ("reversed sources", vec![(1, 0, 0), (0, 0, 0)], false),
            ] {
                let mut candidate = view.clone();
                let result = handlers
                    .into_iter()
                    .try_for_each(|(index, group, handler)| {
                        let source = if reverse { 1 - index } else { index };
                        let mut hook = post_tool_hook(&reads[source]);
                        hook.group = group;
                        hook.handler = handler;
                        apply_post_hook(&mut candidate, hook)
                    });
                assert_eq!(
                    result.is_ok(),
                    accepted,
                    "{name}, researcher={researcher}, reverse={reverse}"
                );
            }
        }
    }
}

fn finish_in_process_read(view: &mut SessionView, intent: &EffectIntent) {
    let (run, agent) = (intent.run_id, intent.agent_run_id);
    let mut finished = tool_finished(run, agent, intent, b"Fact");
    let Event::ToolFinished { observation, .. } = &mut finished else {
        unreachable!()
    };
    observation.attestation = Some(crate::tools::Attestation::InProcess {
        receipt: None,
        policy_digest: intent.policy_digest,
        workspace_device: intent.workspace_device,
        workspace_inode: intent.workspace_inode,
    });
    apply(view, finished).unwrap();
}

#[test]
fn batch_reservations_wait_for_post_tool_hook_settlement() {
    use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
    const FIRST_CEILING_MS: u64 = 1000;
    const SECOND_CEILING_MS: u64 = 500;
    let (mut view, goal, run, agent) = primary_tool_session(true);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 2)).unwrap();
    let first = read_intent(run, agent, "src/lib.rs");
    let second = read_intent(run, agent, "src/lib.rs");
    for intent in [&first, &second] {
        apply(&mut view, tool_proposed(run, agent, intent)).unwrap();
    }
    let reservation = |intent: &EffectIntent, active_ceiling_ms| Event::ToolActiveTimeReserved {
        run_id: run,
        agent_run_id: agent,
        intent_digest: intent.digest(),
        active_ceiling_ms,
    };
    apply(&mut view, reservation(&first, FIRST_CEILING_MS)).unwrap();
    let mut started = verdict_event(
        &first,
        Some(PermissionMode::Default),
        Decision::Allow,
        PermissionSource::Mode,
        PermissionSource::Mode,
        true,
    );
    if let Event::ToolStarted {
        active_ceiling_ms, ..
    } = &mut started
    {
        *active_ceiling_ms = FIRST_CEILING_MS;
    }
    apply(&mut view, started).unwrap();
    apply(&mut view, tool_finished(run, agent, &first, b"Fact")).unwrap();
    let hook = post_tool_hook(&first);
    apply(
        &mut view,
        Event::HookStarted {
            run_id: run,
            agent_run_id: agent,
            hook: hook.clone(),
        },
    )
    .unwrap();
    let before = view.clone();
    assert!(apply(&mut view, reservation(&second, SECOND_CEILING_MS)).is_err());
    assert_eq!(view, before, "an open hook's ceiling remains intact");
    assert_post_hook_reservation_after_finish(
        &view,
        &hook,
        reservation(&second, SECOND_CEILING_MS),
        SECOND_CEILING_MS,
    );
}

fn assert_post_hook_reservation_after_finish(
    view: &SessionView,
    hook: &crate::tools::permissions::HookInvocation,
    reservation: Event,
    expected: u64,
) {
    use crate::tools::permissions::{HookOutcome, HookResult};
    let run = hook.intent.run_id;
    let agent = hook.intent.agent_run_id;
    for completed in [false, true] {
        let mut settled = view.clone();
        let mut observation = observation_for(hook.intent.clone(), b"");
        if !completed {
            observation.disposition = ToolDisposition::Uncertain;
            observation.attestation = None;
            observation.cache_update = CacheUpdate::NotAttempted;
        }
        let result = HookResult::for_event(hook.event, &observation);
        apply(
            &mut settled,
            Event::HookFinished {
                run_id: run,
                agent_run_id: agent,
                outcome: HookOutcome {
                    boundary_active_ms: None,
                    observation,
                    result,
                },
            },
        )
        .unwrap();
        assert_eq!(
            apply(&mut settled, reservation.clone()).is_ok(),
            completed,
            "hook completed: {completed}"
        );
        assert_eq!(
            settled.goals[0].active_ceiling_ms,
            if completed {
                expected
            } else {
                view.goals[0].active_ceiling_ms
            }
        );
    }
}

#[test]
fn primary_host_approval_preserves_ignored_hook_outcomes() {
    use crate::tools::EnforcementCause;
    use crate::tools::permissions::{
        Decision, HookEvent, HookIgnoreReason, HookInvocation, HookOutcome, HookResult,
        PermissionMode, PermissionSource,
    };
    for cause in [
        EnforcementCause::FilesystemDenied,
        EnforcementCause::GuardBootstrap,
    ] {
        let (mut view, goal, run, agent) = primary_tool_session(true);
        view.goals[0]
            .plan_contract
            .as_mut()
            .unwrap()
            .commands
            .push("sh".into());
        let policy = view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap();
        policy.permissions_digest = Some([3; 32]);
        policy.unguarded_rules_digest = Some([5; 32]);
        policy.host_epoch = Some(uuid::Uuid::from_u128(0x01a15ecfa92b736a91a6560b8bc09501));
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let intent = read_intent(run, agent, "src/lib.rs");
        apply(&mut view, tool_proposed(run, agent, &intent)).unwrap();
        let hook = child_intent(
            run,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 7".into()],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
            },
        );
        apply(
            &mut view,
            Event::HookStarted {
                run_id: run,
                agent_run_id: agent,
                hook: HookInvocation {
                    active_ceiling_ms: None,
                    event: HookEvent::PreToolUse,
                    group: 0,
                    handler: 0,
                    subject_digest: intent.digest(),
                    intent: hook.clone(),
                    input_digest: [9; 32],
                },
            },
        )
        .unwrap();
        let mut observation = observation_for(hook, b"");
        observation.enforcement_cause = Some(cause);
        observation.disposition = if cause == EnforcementCause::GuardBootstrap {
            observation.attestation = None;
            ToolDisposition::Failed
        } else {
            ToolDisposition::Denied
        };
        let ignored = HookResult::Ignored {
            why: HookIgnoreReason::NotCompleted,
        };
        apply(
            &mut view,
            Event::HookFinished {
                run_id: run,
                agent_run_id: agent,
                outcome: HookOutcome {
                    boundary_active_ms: None,
                    observation,
                    result: ignored.clone(),
                },
            },
        )
        .unwrap();
        let mut verdict = verdict_event(
            &intent,
            Some(PermissionMode::Default),
            Decision::Allow,
            PermissionSource::Mode,
            PermissionSource::Mode,
            true,
        );
        if let Event::ToolStarted {
            verdict: Some(verdict),
            ..
        } = &mut verdict
        {
            verdict.hooks = 1;
        }
        apply(&mut view, verdict).unwrap();
        apply(&mut view, tool_finished(run, agent, &intent, b"ok")).unwrap();
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let host = child_intent(
            run,
            agent,
            ToolCall::CommandUnguarded {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 0".into()],
                cwd: String::new(),
                justification: "Run a primary host command".into(),
                prefix_rule: None,
            },
        );
        apply(&mut view, tool_proposed(run, agent, &host)).unwrap();
        for source in [
            PermissionSource::AiReview,
            PermissionSource::Rule,
            PermissionSource::Mode,
        ] {
            assert!(rejected(
                &view,
                verdict_event(
                    &host,
                    Some(PermissionMode::Default),
                    Decision::Ask,
                    PermissionSource::Mode,
                    source,
                    true
                )
            ));
        }
        let human = verdict_event(
            &host,
            Some(PermissionMode::Default),
            Decision::Ask,
            PermissionSource::Mode,
            PermissionSource::Human,
            true,
        );
        assert!(!rejected(&view, human.clone()));
        let mut denied = view.clone();
        apply(
            &mut denied,
            verdict_event(
                &host,
                Some(PermissionMode::Default),
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                false,
            ),
        )
        .unwrap();
        let mut ran = observation_for(host.clone(), b"ok");
        ran.output =
            r#"{"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":"","stderr":"","changeset":{"files":[]},"stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":0,"lines":0}}"#.into();
        assert!(rejected(
            &denied,
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation: ran.clone()
            }
        ));
        apply(&mut view, human).unwrap();
        apply(
            &mut view,
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: agent,
                observation: ran,
            },
        )
        .unwrap();
        assert_eq!(
            view.runs[0].tools[0].hooks[0]
                .outcome
                .as_ref()
                .unwrap()
                .result,
            ignored
        );
        assert_eq!(view.runs[0].unguarded_tools(), 1);
    }
}

#[test]
fn lifecycle_hooks_replay_bind_call_subject_order_block_and_continuation() {
    use crate::tools::permissions::{
        HookEvent, HookInvocation, HookOutcome, HookResult, compact_subject, stop_subject,
    };
    for event in [
        HookEvent::Stop,
        HookEvent::SubagentStop,
        HookEvent::PreCompact,
    ] {
        let (mut view, goal, run, actor) = if event == HookEvent::SubagentStop {
            research_session()
        } else {
            primary_tool_session(true)
        };
        view.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        let phase = if event == HookEvent::SubagentStop {
            AgentPhase::ChildWork
        } else {
            AgentPhase::RootPlan
        };
        let index = view.runs[0]
            .agents
            .iter()
            .find(|agent| agent.id == actor)
            .unwrap()
            .provider_calls
            .len();
        let intent = child_intent(
            run,
            actor,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
                resource_class: None,
                timeout_ms: None,
            },
        );
        let subject = if event == HookEvent::PreCompact {
            compact_subject("auto", index + 1)
        } else {
            stop_subject(actor, index)
        };
        let started = Event::HookStarted {
            run_id: run,
            agent_run_id: actor,
            hook: HookInvocation {
                event,
                group: 0,
                handler: 0,
                subject_digest: subject,
                intent: intent.clone(),
                input_digest: [9; 32],
                active_ceiling_ms: Some(1),
            },
        };
        if event != HookEvent::PreCompact {
            assert!(rejected(&view, started.clone()));
        }
        apply(&mut view, reserve_child_call(goal, run, actor)).unwrap();
        apply(
            &mut view,
            Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: actor,
                record: record(phase, ProviderCallDisposition::Finished, Some(0)),
            },
        )
        .unwrap();
        let mut foreign = started.clone();
        if let Event::HookStarted { hook, .. } = &mut foreign {
            hook.subject_digest = [8; 32];
        }
        assert!(rejected(&view, foreign));
        let mut late = view.clone();
        late.last_sequence += 1;
        if event != HookEvent::PreCompact {
            assert!(rejected(&late, started.clone()));
        }
        apply(&mut view, started.clone()).unwrap();
        assert!(rejected(&view, started.clone()));
        assert!(rejected(&view, reserve_child_call(goal, run, actor)));
        let mut observation = observation_for(intent, b"");
        observation.disposition = ToolDisposition::Failed;
        observation.output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":2,"stdout":"","stderr":"verify","stdout_total":{"bytes":0,"lines":0},"stderr_total":{"bytes":6,"lines":1},"changeset":{"files":[]}}).to_string();
        let finished = Event::HookFinished {
            run_id: run,
            agent_run_id: actor,
            outcome: HookOutcome {
                observation,
                result: HookResult::Blocked {
                    reason: Some("verify".into()),
                },
                boundary_active_ms: None,
            },
        };
        let mut forged = finished.clone();
        if let Event::HookFinished { outcome, .. } = &mut forged {
            outcome.result = HookResult::Passed;
        }
        assert!(forged.payload().is_err());
        let mut uncertain_view = view.clone();
        let mut uncertain = finished.clone();
        if let Event::HookFinished { outcome, .. } = &mut uncertain {
            outcome.observation.disposition = ToolDisposition::Uncertain;
            outcome.observation.attestation = None;
            outcome.observation.output = "Hook outcome unknown".into();
            outcome.result = HookResult::Ignored {
                why: crate::tools::permissions::HookIgnoreReason::NotCompleted,
            };
        }
        apply(&mut uncertain_view, uncertain).unwrap();
        assert!(!uncertain_view.runs[0].hooks_settled());
        assert!(rejected(
            &uncertain_view,
            reserve_child_call(goal, run, actor)
        ));
        {
            let mut stopped_view = view.clone();
            let mut stopped = finished.clone();
            if let Event::HookFinished { outcome, .. } = &mut stopped {
                outcome.observation.disposition = ToolDisposition::Succeeded;
                outcome.observation.output = serde_json::json!({"selection":{"elapsed_ms":null,"ignored_entries":0,"pruned_directories":0,"oversized_inputs":0,"discarded_outputs":0},"exit_code":0,"stdout":r#"{"continue":false,"stopReason":"halt"}"#,"stderr":"","stdout_total":{"bytes":(r#"{"continue":false,"stopReason":"halt"}"#).len(),"lines":(r#"{"continue":false,"stopReason":"halt"}"#).lines().count()},"stderr_total":{"bytes":0,"lines":0},"changeset":{"files":[]}}).to_string();
                outcome.result = HookResult::Stopped {
                    reason: Some("halt".into()),
                };
            }
            let terminal = Event::AgentFinished {
                worker: None,
                run_id: run,
                agent_run_id: actor,
                disposition: AgentDisposition::StoppedByHook,
                summary: None,
                result: None,
            };
            assert!(rejected(&stopped_view, terminal.clone()));
            apply(&mut stopped_view, stopped).unwrap();
            assert_eq!(stopped_view.runs[0].hook_stop_reason(), Some(Some("halt")));
            assert!(rejected(
                &stopped_view,
                reserve_child_call(goal, run, actor)
            ));
            assert!(rejected(&stopped_view, started.clone()));
            assert!(rejected(
                &stopped_view,
                Event::AgentFinished {
                    worker: None,
                    run_id: run,
                    agent_run_id: actor,
                    disposition: AgentDisposition::Finished,
                    summary: Some("done".into()),
                    result: Some("done".into()),
                }
            ));
            assert!(rejected(
                &stopped_view,
                Event::RunFinished {
                    run_id: run,
                    disposition: RunDisposition::Failed,
                    failure: Some(crate::session::RunFailure::ProviderCall),
                    active_ms: 0,
                }
            ));
            if event == HookEvent::SubagentStop {
                let mut foreign = stopped_view.clone();
                foreign.runs[0]
                    .lifecycle_hooks
                    .iter_mut()
                    .for_each(|hook| hook.invocation.intent.agent_run_id = AgentRunId::new());
                assert!(rejected(&foreign, terminal.clone()));
            }
            apply(&mut stopped_view, terminal).unwrap();
            if event == HookEvent::SubagentStop {
                assert!(rejected(
                    &stopped_view,
                    Event::RunFinished {
                        run_id: run,
                        disposition: RunDisposition::StoppedByHook,
                        failure: None,
                        active_ms: 0,
                    }
                ));
            }
            assert_eq!(
                stopped_view.runs[0]
                    .agents
                    .iter()
                    .find(|agent| agent.id == actor)
                    .unwrap()
                    .status,
                AgentStatus::StoppedByHook
            );
            if event == HookEvent::Stop {
                stopped_view.goals[0].active_deadline = Some((run, None));
                let terminal = Event::RunFinished {
                    run_id: run,
                    disposition: RunDisposition::StoppedByHook,
                    failure: None,
                    active_ms: 0,
                };
                apply(&mut stopped_view, terminal).unwrap();
                assert_eq!(stopped_view.runs[0].status, RunStatus::StoppedByHook);
                assert!(stopped_view.runs[0].assistant_message.is_none());
            }
        }
        apply(&mut view, finished.clone()).unwrap();
        assert!(rejected(&view, finished));
        if event == HookEvent::PreCompact {
            assert!(rejected(
                &view,
                Event::GoalCompactionCallReserved {
                    goal_id: goal,
                    run_id: run,
                    active_ceiling_ms: 1
                }
            ));
        } else {
            assert!(rejected(
                &view,
                Event::AgentFinished {
                    worker: None,
                    run_id: run,
                    agent_run_id: actor,
                    disposition: AgentDisposition::Finished,
                    summary: Some("done".into()),
                    result: Some("done".into())
                }
            ));
            apply(&mut view, reserve_child_call(goal, run, actor)).unwrap();
            apply(
                &mut view,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: actor,
                    record: record(phase, ProviderCallDisposition::Finished, Some(0)),
                },
            )
            .unwrap();
            assert!(!view.runs[0].stop_blocked(actor));
        }
    }
}

#[test]
fn lifecycle_hook_precompact_between_runs_preserves_budget_and_crash_ceiling() {
    use crate::tools::permissions::{
        HookEvent, HookInvocation, HookOutcome, HookResult, compact_subject,
    };
    const ACTIVE_CEILING_MS: u64 = 1000;
    let (mut view, _, run, actor) = primary_tool_session(true);
    let current = &mut view.runs[0];
    current.status = RunStatus::Finished;
    current.agents[0].status = AgentStatus::Finished;
    current.agents[0].provider_calls.push(record(
        AgentPhase::RootPlan,
        ProviderCallDisposition::Finished,
        Some(0),
    ));
    current
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    view.goals[0].active_run = None;
    view.goals[0].active_deadline = None;
    let intent = child_intent(
        run,
        actor,
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 2".into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        },
    );
    apply(
        &mut view,
        Event::HookStarted {
            run_id: run,
            agent_run_id: actor,
            hook: HookInvocation {
                event: HookEvent::PreCompact,
                group: 0,
                handler: 0,
                subject_digest: compact_subject("manual", 1),
                input_digest: [9; 32],
                intent: intent.clone(),
                active_ceiling_ms: Some(ACTIVE_CEILING_MS),
            },
        },
    )
    .unwrap();
    assert_eq!(
        view.goals[0].resume_usage(&view).active_ms,
        ACTIVE_CEILING_MS
    );
    assert!(!view.runs[0].hooks_settled());
    assert_exhausted_crash_receipt(&view, ACTIVE_CEILING_MS);
    let mut observation = observation_for(intent, b"");
    observation.disposition = ToolDisposition::Denied;
    observation.attestation = None;
    observation.output = "hook limit".into();
    let finished = Event::HookFinished {
        run_id: run,
        agent_run_id: actor,
        outcome: HookOutcome {
            observation,
            result: HookResult::Blocked {
                reason: Some("hook limit".into()),
            },
            boundary_active_ms: Some(7),
        },
    };
    let mut excessive = finished.clone();
    if let Event::HookFinished { outcome, .. } = &mut excessive {
        outcome.boundary_active_ms = Some(ACTIVE_CEILING_MS + 1);
    }
    assert!(rejected(&view, excessive));
    apply(&mut view, finished).unwrap();
    assert_eq!(view.goals[0].active_ms, 7);
    assert_eq!(view.goals[0].active_ceiling_ms, 0);
}

#[test]
fn lifecycle_hook_finishing_after_its_run_was_interrupted_is_rejected() {
    use crate::tools::permissions::{
        HookEvent, HookInvocation, HookOutcome, HookResult, stop_subject,
    };
    const ACTIVE_CEILING_MS: u64 = 1000;
    let (mut view, goal, run, actor) = primary_tool_session(true);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    apply(&mut view, reserve_child_call(goal, run, actor)).unwrap();
    apply(
        &mut view,
        Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: actor,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                Some(0),
            ),
        },
    )
    .unwrap();
    let intent = child_intent(
        run,
        actor,
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 2".into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        },
    );
    let last_call = view.runs[0].agents[0].provider_calls.len() - 1;
    apply(
        &mut view,
        Event::HookStarted {
            run_id: run,
            agent_run_id: actor,
            hook: HookInvocation {
                event: HookEvent::Stop,
                group: 0,
                handler: 0,
                subject_digest: stop_subject(actor, last_call),
                input_digest: [9; 32],
                intent: intent.clone(),
                active_ceiling_ms: Some(ACTIVE_CEILING_MS),
            },
        },
    )
    .unwrap();
    apply(
        &mut view,
        Event::SessionRenamed {
            title: "Renamed".into(),
        },
    )
    .unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    let mut observation = observation_for(intent, b"");
    observation.disposition = ToolDisposition::Denied;
    observation.attestation = None;
    observation.output = "hook limit".into();
    for boundary_active_ms in [None, Some(0)] {
        assert!(rejected(
            &view,
            Event::HookFinished {
                run_id: run,
                agent_run_id: actor,
                outcome: HookOutcome {
                    observation: observation.clone(),
                    result: HookResult::Blocked {
                        reason: Some("hook limit".into()),
                    },
                    boundary_active_ms,
                },
            }
        ));
    }
    assert_eq!(
        view.goals[0].resume_usage(&view).active_ms,
        ACTIVE_CEILING_MS
    );
}

#[test]
fn crash_with_an_open_idle_precompact_leaves_the_session_usable() {
    use crate::tools::permissions::{HookEvent, HookInvocation, compact_subject};
    const ACTIVE_CEILING_MS: u64 = 1000;
    let (mut view, _, run, actor) = primary_tool_session(true);
    let current = &mut view.runs[0];
    current.status = RunStatus::Finished;
    current.agents[0].status = AgentStatus::Finished;
    current.agents[0].provider_calls.push(record(
        AgentPhase::RootPlan,
        ProviderCallDisposition::Finished,
        Some(0),
    ));
    current
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = Some([3; 32]);
    view.goals[0].active_run = None;
    view.goals[0].active_deadline = None;
    let intent = child_intent(
        run,
        actor,
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "exit 2".into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        },
    );
    apply(
        &mut view,
        Event::HookStarted {
            run_id: run,
            agent_run_id: actor,
            hook: HookInvocation {
                event: HookEvent::PreCompact,
                group: 0,
                handler: 0,
                subject_digest: compact_subject("manual", 1),
                input_digest: [9; 32],
                intent,
                active_ceiling_ms: Some(ACTIVE_CEILING_MS),
            },
        },
    )
    .unwrap();
    let next_goal = GoalId::new();
    apply(&mut view, goal_started(next_goal, GoalBudget::default())).unwrap();
    for event in start_run(
        next_goal,
        RunId::new(),
        AgentRunId::new(),
        config(CollaborationPolicy::Single, true),
    ) {
        apply(&mut view, event).unwrap();
    }
    assert_eq!(view.runs.len(), 2);
    assert!(!view.runs[0].hooks_settled());
    assert_eq!(
        view.goals[0].resume_usage(&view).active_ms,
        ACTIVE_CEILING_MS
    );
}
