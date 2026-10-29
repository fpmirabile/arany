use super::super::super::test_support::*;
use super::super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::{ChildGrant, CollaborationPolicy};
use crate::tools::{CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation};

#[test]
fn proposal_only_tools_allow_failed_agent_and_run_termination() {
    let (session, goal, run, agent) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
    );
    let mut events = vec![goal_started(goal, GoalBudget::default())];
    events.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, true),
    ));
    events.extend([
        reserve_child_call(goal, run, agent),
        Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: agent,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::ToolRequested,
                Some(0),
            ),
        },
        tool_proposed(run, agent, &read_intent(run, agent, "src/lib.rs")),
        Event::AgentFinished {
            run_id: run,
            agent_run_id: agent,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
            worker: None,
        },
        Event::RunFinished {
            run_id: run,
            disposition: RunDisposition::Failed,
            failure: Some(crate::session::RunFailure::BudgetExhausted),
            active_ms: 0,
        },
    ]);
    let view = SessionView::replay(session, &session_with(session, events))
        .unwrap()
        .unwrap();
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.agents[0].status, AgentStatus::Failed);
    assert_eq!(
        run.failure,
        Some(crate::session::RunFailure::BudgetExhausted)
    );
    assert_eq!(run.tools.len(), 1);
    assert_eq!(run.tools[0].started_sequence, None);
    assert!(run.tools[0].observation.is_none());
}

#[test]
fn child_research_tools_replay_in_order_and_reject_widening() {
    let (mut view, goal, run, child) = research_session();
    let read = child_intent(
        run,
        child,
        ToolCall::Read {
            path: "src/lib.rs".into(),
            offset: None,
            limit: Some(1024),
        },
    );
    apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
    assert!(rejected(
        &view,
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: child,
            intent: read.clone().into(),
            active_ceiling_ms: 0,
        }
    ));
    let mut no_tools = delegated_session(0);
    apply_child_spawn(&mut no_tools, ChildGrant::NoTools).expect("no-tools child spawn");
    let (no_tools_goal, no_tools_run, no_tools_child) = (
        no_tools.goals[0].id,
        no_tools.runs[0].id,
        no_tools.runs[0].agents[1].id,
    );
    apply(
        &mut no_tools,
        reserve_child_call(no_tools_goal, no_tools_run, no_tools_child),
    )
    .expect("no-tools child call");
    assert!(rejected(
        &no_tools,
        child_record(
            no_tools_run,
            no_tools_child,
            ProviderCallDisposition::ToolRequested
        ),
    ));
    apply(
        &mut view,
        child_record(run, child, ProviderCallDisposition::ToolRequested),
    )
    .expect("child Tool request");
    let started = Event::ToolStarted {
        verdict: None,
        run_id: run,
        agent_run_id: child,
        intent: read.clone().into(),
        active_ceiling_ms: 0,
    };
    let finished = Event::ToolFinished {
        checkpoint: None,
        run_id: run,
        agent_run_id: child,
        observation: observation_for(read.clone(), b"ok"),
    };
    let after_request = view.clone();
    let write = child_intent(
        run,
        child,
        ToolCall::Write {
            path: "src/lib.rs".into(),
            expected_digest: None,
            content: "x".into(),
        },
    );
    assert!(rejected(
        &after_request,
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: child,
            intent: write.into(),
            active_ceiling_ms: 0,
        }
    ));
    let mcp = child_intent(
        run,
        child,
        ToolCall::McpCall {
            server: "local".into(),
            tool: "probe".into(),
            schema_digest: "a".repeat(64),
            arguments: "{}".into(),
        },
    );
    assert!(rejected(
        &after_request,
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: child,
            intent: mcp.into(),
            active_ceiling_ms: 0,
        }
    ));
    for call in [
        ToolCall::GitStatus { path: ".".into() },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 1,
        },
    ] {
        let intent = child_intent(run, child, call);
        assert!(rejected(
            &after_request,
            Event::ToolStarted {
                verdict: None,
                run_id: run,
                agent_run_id: child,
                intent: intent.into(),
                active_ceiling_ms: 0,
            }
        ));
    }
    apply(&mut view, started.clone()).expect("child Tool starts");
    let mut open = view.clone();
    apply(&mut open, reserve_child_call(goal, run, child)).expect("second reservation");
    assert!(rejected(
        &open,
        child_record(run, child, ProviderCallDisposition::Finished)
    ));
    assert!(rejected(
        &view,
        Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: child,
            disposition: AgentDisposition::Cancelled,
            summary: None,
            result: None,
        }
    ));
    let mismatched = ToolObservation {
        spill: None,
        enforcement_cause: None,
        limit_cause: None,
        intent: child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: None,
                limit: Some(2),
            },
        ),
        ..observation_for(read.clone(), b"ok")
    };
    assert!(rejected(
        &view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: child,
            observation: mismatched,
        }
    ));
    apply(&mut view, finished).expect("child Tool finishes");

    apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
    apply(
        &mut view,
        child_record(run, child, ProviderCallDisposition::Finished),
    )
    .expect("child finishes");
    apply(
        &mut view,
        Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: child,
            disposition: AgentDisposition::Finished,
            summary: Some("report".into()),
            result: Some("report".into()),
        },
    )
    .expect("child report");
    assert!(view.runs[0].tools.is_empty());
    assert_eq!(view.runs[0].agents[1].tools.len(), 1);
    assert!(!view.goals[0].awaiting_plan_contract);
    assert!(!view.goals[0].success_criterion_satisfied);
}

#[test]
fn tool_limits_require_a_cause_and_other_dispositions_forbid_one() {
    use crate::tools::LimitCause;
    let (mut view, goal, run, child) = research_session();
    let intent = child_intent(
        run,
        child,
        ToolCall::Read {
            path: "src/lib.rs".into(),
            offset: None,
            limit: Some(16),
        },
    );
    apply(&mut view, reserve_child_call(goal, run, child)).unwrap();
    apply(
        &mut view,
        child_record(run, child, ProviderCallDisposition::ToolRequested),
    )
    .unwrap();
    apply(
        &mut view,
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: child,
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
        },
    )
    .unwrap();
    for disposition in [ToolDisposition::Limit, ToolDisposition::Succeeded] {
        for cause in [
            None,
            Some(LimitCause::Timeout),
            Some(LimitCause::OutputCeiling),
            Some(LimitCause::ContextFull),
            Some(LimitCause::ReadCapacity),
            Some(LimitCause::RequestedTime),
            Some(LimitCause::Resource),
        ] {
            let mut observation = observation_for(intent.clone(), b"ok");
            observation.disposition = disposition;
            observation.limit_cause = cause;
            let event = Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: child,
                observation,
            };
            let expected = (disposition == ToolDisposition::Limit) == cause.is_some();
            assert_eq!(
                apply(&mut view.clone(), event).is_ok(),
                expected,
                "{disposition:?} {cause:?}"
            );
        }
    }
}

#[test]
fn child_tool_outside_the_plan_replays_only_as_denied() {
    let (mut view, goal, run, child) = research_session();
    let outside = child_intent(
        run,
        child,
        ToolCall::Read {
            path: "src/main.rs".into(),
            offset: None,
            limit: Some(1024),
        },
    );
    apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
    apply(
        &mut view,
        child_record(run, child, ProviderCallDisposition::ToolRequested),
    )
    .expect("child Tool request");
    apply(
        &mut view,
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: child,
            intent: outside.clone().into(),
            active_ceiling_ms: 0,
        },
    )
    .expect("child Tool starts");
    assert!(rejected(
        &view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: child,
            observation: observation_for(outside.clone(), b"x"),
        }
    ));
    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: child,
            observation: ToolObservation {
                spill: None,
                enforcement_cause: None,
                limit_cause: None,
                intent: outside,
                disposition: ToolDisposition::Denied,
                output: "The effect is outside the active Goal plan; no operation was dispatched"
                    .into(),
                attestation: None,
                approval_cause: None,
                guard_stage: None,
                cache_update: CacheUpdate::NotApplicable,
            },
        },
    )
    .expect("out-of-plan child read is denied");
}

#[test]
fn child_research_tool_bound_admits_sixteen_requests_and_no_seventeenth() {
    let (mut view, goal, run, child) = research_session();
    for _ in 0..crate::session::MAX_CHILD_TOOL_CALLS {
        let read = child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: None,
                limit: Some(1024),
            },
        );
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::ToolRequested),
        )
        .expect("child Tool request");
        apply(
            &mut view,
            Event::ToolStarted {
                verdict: None,
                run_id: run,
                agent_run_id: child,
                intent: read.clone().into(),
                active_ceiling_ms: 0,
            },
        )
        .expect("child Tool starts");
        apply(
            &mut view,
            Event::ToolFinished {
                checkpoint: None,
                run_id: run,
                agent_run_id: child,
                observation: observation_for(read, b"ok"),
            },
        )
        .expect("child Tool finishes");
    }
    apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
    assert!(rejected(
        &view,
        child_record(run, child, ProviderCallDisposition::ToolRequested)
    ));
    apply(
        &mut view,
        child_record(run, child, ProviderCallDisposition::Finished),
    )
    .expect("child may still finish at the bound");
}

#[test]
fn crash_left_child_tool_interrupts_the_run_and_blocks_goal_resume() {
    let (mut view, goal, run, child) = research_session();
    let read = child_intent(
        run,
        child,
        ToolCall::Read {
            path: "src/lib.rs".into(),
            offset: None,
            limit: Some(1024),
        },
    );
    apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
    apply(
        &mut view,
        child_record(run, child, ProviderCallDisposition::ToolRequested),
    )
    .expect("child Tool request");
    apply(
        &mut view,
        Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: child,
            intent: read.clone().into(),
            active_ceiling_ms: 0,
        },
    )
    .expect("child Tool starts");
    let mut crashed = view.clone();
    apply(
        &mut crashed,
        goal_started(GoalId::new(), GoalBudget::default()),
    )
    .expect("a new Goal seals the crashed Goal");
    assert_eq!(crashed.goals[0].status, GoalStatus::Interrupted);
    assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
    assert_eq!(crashed.runs[0].agents[1].status, AgentStatus::Interrupted);
    assert!(!crashed.goals[0].can_resume(&crashed));

    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: child,
            observation: observation_for(read, b"ok"),
        },
    )
    .expect("child Tool finishes");
    let mut settled = view.clone();
    apply(
        &mut settled,
        goal_started(GoalId::new(), GoalBudget::default()),
    )
    .expect("a new Goal seals the settled Goal");
    assert!(settled.goals[0].can_resume(&settled));
}

#[test]
fn tool_preflight_matches_reducer_rejections_without_changing_the_view() {
    use crate::session::ProposalRefusalCause::{InvalidArguments, Phase, PlanScopeExcludesCall};
    for (case, expected) in [
        ("valid", Ok(())),
        ("path", Err(InvalidArguments)),
        ("policy", Err(Phase)),
        ("workspace", Err(Phase)),
        ("phase", Err(Phase)),
        ("scope", Err(PlanScopeExcludesCall)),
        ("missing_verdict", Err(Phase)),
    ] {
        let (mut view, _, run, agent) = primary_tool_session(false);
        view.runs[0].agents[0].provider_calls.push(record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::ToolRequested,
            Some(0),
        ));
        let mut intent = read_intent(run, agent, "src/lib.rs");
        match case {
            "path" => {
                intent.call = ToolCall::Read {
                    path: "../outside".into(),
                    offset: None,
                    limit: None,
                }
            }
            "policy" => intent.policy_digest = [3; 32],
            "workspace" => intent.workspace_inode += 1,
            "phase" => {
                view.goals[0].phase = GoalPhase::Research;
                intent.call = ToolCall::Mkdir { path: "src".into() };
            }
            "scope" => {
                view.goals[0].plan_contract = delegated_session(0).goals[0].plan_contract.clone();
                intent.call = ToolCall::Read {
                    path: "outside.rs".into(),
                    offset: None,
                    limit: None,
                };
            }
            "missing_verdict" => {
                view.runs[0]
                    .config
                    .as_mut()
                    .unwrap()
                    .tool_policy
                    .as_mut()
                    .unwrap()
                    .permissions_digest = Some([4; 32])
            }
            _ => (),
        }
        let event = tool_started(run, agent, &intent);
        let original = view.clone();
        assert_eq!(
            crate::session::validate_tool_started(&view, &event).map_err(|refusal| refusal.cause),
            expected,
            "{case}"
        );
        assert_eq!(view, original, "preflight must be read-only: {case}");
        let mut attempted = view.clone();
        assert_eq!(
            apply(&mut attempted, event.clone()).is_ok(),
            expected.is_ok(),
            "{case}"
        );
    }
}

#[test]
fn preflight_refusal_closes_proposals_but_never_unknown_started_effects() {
    for stage in ["prepared", "proposed", "reserved", "started"] {
        let (mut view, _, run, actor) = primary_tool_session(false);
        view.goals[0].active_deadline = Some((run, None));
        view.runs[0].agents[0].provider_calls.push(record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::ToolRequested,
            Some(0),
        ));
        let intent = read_intent(run, actor, "src/lib.rs");
        let start = tool_started(run, actor, &intent);
        assert!(crate::session::validate_tool_started(&view, &start).is_ok());
        if stage != "prepared" {
            apply(&mut view, tool_proposed(run, actor, &intent)).unwrap();
        }
        if stage == "reserved" {
            apply(
                &mut view,
                Event::ToolActiveTimeReserved {
                    run_id: run,
                    agent_run_id: actor,
                    intent_digest: intent.digest(),
                    active_ceiling_ms: 1,
                },
            )
            .unwrap();
        }
        if stage == "started" {
            apply(&mut view, start.clone()).unwrap();
        }
        let refused = Event::ToolPreflightRefused {
            run_id: run,
            agent_run_id: actor,
            record: crate::session::ToolPreflightRefusal {
                intent_digest: intent.digest(),
                refusal: crate::session::ToolRefusal::INVALID_TRANSITION,
                verdict: None,
            },
        };
        if stage == "started" {
            assert!(rejected(&view, refused));
            continue;
        }
        apply(&mut view, refused.clone()).unwrap();
        assert!(rejected(&view, refused));
        assert!(rejected(&view, start));
        assert!(rejected(
            &view,
            Event::AgentFinished {
                run_id: run,
                agent_run_id: actor,
                disposition: AgentDisposition::StoppedByHook,
                summary: None,
                result: None,
                worker: None,
            }
        ));
        apply(
            &mut view,
            Event::AgentFinished {
                run_id: run,
                agent_run_id: actor,
                disposition: AgentDisposition::Failed,
                summary: None,
                result: None,
                worker: None,
            },
        )
        .unwrap();
        apply(
            &mut view,
            Event::RunFinished {
                run_id: run,
                disposition: RunDisposition::Failed,
                failure: Some(crate::session::RunFailure::ProposalRefused),
                active_ms: 0,
            },
        )
        .unwrap();
        assert_eq!(view.runs[0].status, RunStatus::Failed, "{stage}");
        assert!(
            view.runs[0]
                .tools
                .iter()
                .all(|tool| tool.observation.is_none())
        );
    }
}

#[test]
fn enforcement_causes_round_trip_and_reject_unsupported_claims() {
    use crate::tools::EnforcementCause;
    let (mut view, goal, run, agent) = primary_tool_session(false);
    let intent = read_intent(run, agent, "src/lib.rs");
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    apply(&mut view, tool_started(run, agent, &intent)).unwrap();
    let mut observation = observation_for(intent, b"");
    observation.disposition = ToolDisposition::Denied;
    observation.output =
        "File path is unavailable or outside the admitted regular-file boundary".into();
    observation.enforcement_cause = Some(EnforcementCause::FilesystemDenied);
    let finished = |observation| Event::ToolFinished {
        checkpoint: None,
        run_id: run,
        agent_run_id: agent,
        observation,
    };
    for disposition in [
        ToolDisposition::Succeeded,
        ToolDisposition::Failed,
        ToolDisposition::Uncertain,
        ToolDisposition::Limit,
    ] {
        let mut invalid = observation.clone();
        invalid.disposition = disposition;
        invalid.limit_cause =
            (disposition == ToolDisposition::Limit).then_some(crate::tools::LimitCause::Resource);
        assert!(
            rejected(&view, finished(invalid)),
            "unsupported cause on {disposition:?}"
        );
    }
    let mut unattested = observation.clone();
    unattested.attestation = None;
    assert!(rejected(&view, finished(unattested)));
    let event = finished(observation.clone());
    let payload = event.payload().unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap()["enforcement_cause"],
        "filesystem_denied"
    );
    let decoded = Event::decode(
        "ToolFinished",
        crate::session::events::payload_version("ToolFinished").unwrap(),
        &payload,
        Some(run),
        Some(agent),
    )
    .unwrap();
    assert_eq!(decoded, event);
    apply(&mut view, decoded).unwrap();
    assert_eq!(
        view.runs[0].tools.last().unwrap().observation.as_ref(),
        Some(&observation)
    );
}

#[test]
fn tool_started_requires_pending_request() {
    const FIRST_ACTIVE_CEILING_MS: u64 = 1000;
    const SECOND_ACTIVE_CEILING_MS: u64 = 500;
    let (mut view, goal, run, agent) = primary_tool_session(false);
    let (first, second, third) = (
        read_intent(run, agent, "src/lib.rs"),
        read_intent(run, agent, "src/main.rs"),
        read_intent(run, agent, "README.md"),
    );
    apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
    assert!(
        rejected(&view, tool_started(run, agent, &first)),
        "no request is declared before the step"
    );
    apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
    apply(
        &mut view,
        Event::ToolProposed {
            run_id: run,
            agent_run_id: agent,
            intent: first.clone().into(),
            active_ceiling_ms: FIRST_ACTIVE_CEILING_MS,
        },
    )
    .expect("first request is proposed");
    assert_exhausted_crash_receipt(&view, FIRST_ACTIVE_CEILING_MS);
    assert!(
        rejected(&view, tool_started(run, agent, &first)),
        "all declared intents must be proposed before a start"
    );
    assert!(
        rejected(&view, tool_finished(run, agent, &first, b"one")),
        "an observation requires a folded start"
    );
    for stage in ["active", "reviewed"] {
        let mut busy = view.clone();
        let tool = busy.runs[0].tools.last_mut().unwrap();
        if stage == "active" {
            tool.active_time_reserved = true;
        } else {
            tool.review_call_index = Some(0);
        }
        assert!(
            rejected(&busy, tool_proposed(run, agent, &second)),
            "no new intent during {stage} Tool work"
        );
    }
    apply(&mut view, tool_proposed(run, agent, &second)).expect("second request is proposed");
    let reservation = |intent: &EffectIntent, active_ceiling_ms| Event::ToolActiveTimeReserved {
        run_id: run,
        agent_run_id: agent,
        intent_digest: intent.digest(),
        active_ceiling_ms,
    };
    apply(&mut view, reservation(&first, FIRST_ACTIVE_CEILING_MS)).expect("first reserved");
    let started = |intent: &EffectIntent, active_ceiling_ms| Event::ToolStarted {
        run_id: run,
        agent_run_id: agent,
        intent: intent.clone().into(),
        active_ceiling_ms,
        verdict: None,
    };
    apply(&mut view, started(&first, FIRST_ACTIVE_CEILING_MS)).expect("first starts");
    apply(&mut view, reservation(&second, SECOND_ACTIVE_CEILING_MS))
        .expect("read reservations may overlap");
    assert_eq!(view.goals[0].active_ceiling_ms, FIRST_ACTIVE_CEILING_MS);
    assert!(
        rejected(&view, reservation(&second, SECOND_ACTIVE_CEILING_MS)),
        "one reservation per intent"
    );
    apply(&mut view, started(&second, SECOND_ACTIVE_CEILING_MS)).expect("second starts");
    assert_eq!(view.goals[0].active_ceiling_ms, FIRST_ACTIVE_CEILING_MS);
    apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second completes first");
    apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first completes last");
    assert!(
        rejected(&view, tool_started(run, agent, &third)),
        "every declared request is already started"
    );
}

fn stopped_batch(
    mode: Option<crate::tools::permissions::PermissionMode>,
    disposition: ToolDisposition,
) -> (SessionView, EffectIntent) {
    let (mut view, goal, run, agent) = primary_tool_session(false);
    view.runs[0]
        .config
        .as_mut()
        .unwrap()
        .tool_policy
        .as_mut()
        .unwrap()
        .permissions_digest = mode.map(|_| [3; 32]);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 2)).unwrap();
    let first = read_intent(run, agent, "src/lib.rs");
    let second = read_intent(run, agent, "src/main.rs");
    for intent in [&first, &second] {
        apply(&mut view, tool_proposed(run, agent, intent)).unwrap();
    }
    apply(&mut view, batch_start(&first, mode, true)).unwrap();
    let mut stopped = tool_finished(run, agent, &first, b"stopped");
    if let Event::ToolFinished { observation, .. } = &mut stopped {
        observation.disposition = disposition;
        observation.attestation = None;
    }
    apply(&mut view, stopped).unwrap();
    (view, second)
}

#[test]
fn stopped_batches_settle_proposals_without_authorizing_execution() {
    use crate::tools::permissions::PermissionMode;
    const ACTIVE_CEILING_MS: u64 = 1000;
    for mode in [None, Some(PermissionMode::Default)] {
        for disposition in [ToolDisposition::Failed, ToolDisposition::Uncertain] {
            let (mut view, second) = stopped_batch(mode, disposition);
            let (run, agent) = (second.run_id, second.agent_run_id);
            let goal = view.goals[0].id;
            assert!(rejected(&view, reserve_child_call(goal, run, agent)));
            let third = read_intent(run, agent, "README.md");
            assert!(rejected(&view, tool_proposed(run, agent, &third)));
            assert!(rejected(&view, batch_start(&third, mode, false)));
            assert!(rejected(
                &view,
                Event::ToolActiveTimeReserved {
                    run_id: run,
                    agent_run_id: agent,
                    intent_digest: second.digest(),
                    active_ceiling_ms: ACTIVE_CEILING_MS,
                }
            ));
            if mode.is_some() {
                assert!(rejected(&view, batch_start(&second, mode, true)));
            }
            apply(&mut view, batch_start(&second, mode, false))
                .expect("folded non-dispatch settlement");
            assert!(rejected(
                &view,
                tool_finished(run, agent, &second, b"executed")
            ));
            let mut denied = tool_finished(run, agent, &second, b"not dispatched");
            if let Event::ToolFinished { observation, .. } = &mut denied {
                observation.disposition = ToolDisposition::Denied;
                observation.attestation = None;
                observation.output = crate::tools::types::UNDISPATCHED_STEP_OUTPUT.into();
            }
            apply(&mut view, denied).unwrap();
            assert!(
                view.runs[0]
                    .tools
                    .iter()
                    .all(|tool| tool.observation.is_some())
            );
            assert_eq!(
                view.runs[0].tools[0]
                    .observation
                    .as_ref()
                    .unwrap()
                    .disposition,
                disposition
            );
        }
    }
}

#[test]
fn stopping_tool_keeps_the_batch_cause_after_siblings_settle() {
    for disposition in [
        ToolDisposition::Failed,
        ToolDisposition::Uncertain,
        ToolDisposition::Limit,
    ] {
        for succeeded in [false, true] {
            let (mut view, [first, second]) = reserved_read_pair();
            let (run, agent) = (first.run_id, first.agent_run_id);
            let mut stopped = tool_finished(run, agent, &first, b"stopped");
            if let Event::ToolFinished { observation, .. } = &mut stopped {
                observation.disposition = disposition;
                observation.attestation = None;
                observation.limit_cause = (disposition == ToolDisposition::Limit)
                    .then_some(crate::tools::LimitCause::Timeout);
            }
            apply(&mut view, stopped).unwrap();
            let mut settled = tool_finished(run, agent, &second, b"settled");
            if !succeeded && let Event::ToolFinished { observation, .. } = &mut settled {
                observation.disposition = ToolDisposition::Denied;
                observation.attestation = None;
                observation.output = crate::tools::types::UNDISPATCHED_STEP_OUTPUT.into();
            }
            apply(&mut view, settled).unwrap();
            assert_eq!(
                view.runs[0].stopping_tool().map(|tool| tool.intent.id),
                Some(first.id),
                "{disposition:?}, succeeded sibling: {succeeded}"
            );
        }
    }
}

#[test]
fn an_in_process_attestation_cannot_finish_a_write() {
    let (mut view, goal, run, agent) = primary_tool_session(true);
    let write = child_intent(
        run,
        agent,
        ToolCall::Write {
            path: "src/lib.rs".into(),
            expected_digest: None,
            content: "after".into(),
        },
    );
    apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
    apply(&mut view, requesting_step(run, agent, 1)).expect("one-request step");
    apply(&mut view, tool_started(run, agent, &write)).expect("write starts");
    let mut read_attested = observation_for(write.clone(), b"after");
    read_attested.attestation = Some(crate::tools::Attestation::InProcess {
        receipt: None,
        policy_digest: write.policy_digest,
        workspace_device: write.workspace_device,
        workspace_inode: write.workspace_inode,
    });
    assert!(rejected(
        &view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation: read_attested,
        }
    ));
    let finished = tool_finished(run, agent, &write, b"after");
    let bytes = finished.payload().unwrap();
    let decoded = Event::decode(
        finished.kind(),
        finished.version(),
        &bytes,
        Some(run),
        Some(agent),
    )
    .unwrap();
    apply(&mut view, decoded).expect("historical Guard receipt");
    assert_eq!(finished.payload().unwrap(), bytes);
}

#[test]
fn file_versions_require_intent_bound_in_process_receipts() {
    use crate::tools::{Attestation, EnforcementCause, FileReceipt, GuardReceipt};
    let (mut view, goal, run, agent) = primary_tool_session(true);
    let call = ToolCall::Write {
        path: "src/lib.rs".into(),
        expected_digest: None,
        content: "after".into(),
    };
    let mut intent = child_intent(run, agent, call);
    intent.enforcement_digest = FileReceipt::enforcement_digest();
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    apply(&mut view, tool_started(run, agent, &intent)).unwrap();
    let finish = |observation: ToolObservation| Event::ToolFinished {
        checkpoint: observation
            .attestation
            .is_some()
            .then(crate::Checkpoint::not_needed),
        run_id: run,
        agent_run_id: agent,
        observation,
    };
    let observed = observation_for(intent.clone(), b"after");
    let mut guarded = observed.clone();
    guarded.attestation = Some(Attestation::Guard(GuardReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        enforcement_digest: intent.enforcement_digest,
        limits: intent.limits.clone(),
    }));
    assert!(rejected(&view, finish(guarded)));
    let mut bootstrap = observed.clone();
    bootstrap.disposition = ToolDisposition::Failed;
    bootstrap.enforcement_cause = Some(EnforcementCause::GuardBootstrap);
    bootstrap.attestation = None;
    assert!(rejected(&view, finish(bootstrap)));
    let receipt = FileReceipt {
        intent_digest: intent.digest(),
        before: None,
        after: Some(crate::validate::digest(b"after")),
        executable: false,
    };
    let mut forged = receipt.clone();
    forged.intent_digest = [9; 32];
    for candidate in [None, Some(forged), Some(receipt.clone())] {
        let correct = candidate.as_ref() == Some(&receipt);
        let mut observation = observed.clone();
        observation.attestation = Some(Attestation::InProcess {
            receipt: candidate,
            policy_digest: intent.policy_digest,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
        });
        if correct {
            apply(&mut view, finish(observation)).expect("in-process File receipt");
        } else {
            assert!(rejected(&view, finish(observation)));
        }
    }
}

#[test]
fn file_receipts_preserve_guard_history_and_require_the_new_execution_site() {
    use crate::tools::{Attestation, EnforcementCause, FileReceipt, GuardReceipt};
    let (mut view, goal, run, agent) = primary_tool_session(true);
    let path = "src/new";
    let contract = view.goals[0].plan_contract.as_mut().unwrap();
    contract.paths.push(path.into());
    let call = ToolCall::Mkdir { path: path.into() };
    let mut intent = child_intent(run, agent, call);
    apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
    apply(&mut view, requesting_step(run, agent, 1)).unwrap();
    let finish = |observation| Event::ToolFinished {
        checkpoint: None,
        run_id: run,
        agent_run_id: agent,
        observation,
    };
    let mut historical = view.clone();
    apply(&mut historical, tool_started(run, agent, &intent)).unwrap();
    let mut observed = observation_for(intent.clone(), b"");
    observed.output = serde_json::json!({"created":true}).to_string();
    assert!(matches!(observed.attestation, Some(Attestation::Guard(_))));
    let finished = finish(observed);
    let bytes = finished.payload().unwrap();
    let (kind, version) = (finished.kind(), finished.version());
    let decoded = Event::decode(kind, version, &bytes, Some(run), Some(agent)).unwrap();
    assert_eq!(decoded.payload().unwrap(), bytes);
    apply(&mut historical, decoded).expect("historical Guard Mkdir receipt");
    intent.enforcement_digest = FileReceipt::enforcement_digest();
    apply(&mut view, tool_started(run, agent, &intent)).unwrap();
    let mut observed = observation_for(intent.clone(), b"");
    observed.output = serde_json::json!({"created":true}).to_string();
    let mut guarded = observed.clone();
    guarded.attestation = Some(Attestation::Guard(GuardReceipt {
        contract_version: 1,
        intent_digest: intent.digest(),
        enforcement_digest: intent.enforcement_digest,
        limits: intent.limits.clone(),
    }));
    assert!(rejected(&view, finish(guarded)));
    let mut bootstrap = observed.clone();
    bootstrap.disposition = ToolDisposition::Failed;
    bootstrap.enforcement_cause = Some(EnforcementCause::GuardBootstrap);
    bootstrap.attestation = None;
    assert!(rejected(&view, finish(bootstrap)));
    let receipt = FileReceipt {
        intent_digest: intent.digest(),
        before: None,
        after: None,
        executable: false,
    };
    let mut forged = receipt.clone();
    forged.intent_digest = [9; 32];
    for candidate in [None, Some(forged), Some(receipt.clone())] {
        let correct = candidate.as_ref() == Some(&receipt);
        let mut observation = observed.clone();
        observation.attestation = Some(Attestation::InProcess {
            receipt: candidate,
            policy_digest: intent.policy_digest,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
        });
        if correct {
            apply(&mut view, finish(observation)).expect("in-process File receipt");
        } else {
            assert!(rejected(&view, finish(observation)));
        }
    }
}

#[test]
fn discovery_replays_recorded_guard_observations_and_rejects_in_process_receipts() {
    let cases = [
        (
            ToolCall::Glob {
                pattern: "**/*.rs".into(),
                path: "src".into(),
                offset: 0,
                limit: 100,
            },
            r#"{"entries":[{"bytes":3,"path":"src/a.rs"}],"limit":100,"may_be_truncated":false,"next_offset":null}"#,
        ),
        (
            ToolCall::List { path: "src".into() },
            r#"{"entries":[{"bytes":3,"path":"src/a.rs"}],"may_be_truncated":false}"#,
        ),
    ];
    for (call, output) in cases {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        let intent = child_intent(run, agent, call);
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
        apply(&mut view, requesting_step(run, agent, 1)).expect("one-request step");
        apply(&mut view, tool_started(run, agent, &intent)).expect("discovery starts");
        let mut guarded = observation_for(intent.clone(), b"");
        guarded.output = output.into();
        guarded.attestation = Some(crate::tools::Attestation::Guard(
            crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            },
        ));
        let finished = |observation| Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation,
        };
        let mut in_process = guarded.clone();
        in_process.attestation = Some(crate::tools::Attestation::InProcess {
            receipt: None,
            policy_digest: intent.policy_digest,
            workspace_device: intent.workspace_device,
            workspace_inode: intent.workspace_inode,
        });
        assert!(rejected(&view, finished(in_process)));
        let mut invalid = guarded.clone();
        if let Some(crate::tools::Attestation::Guard(receipt)) = &mut invalid.attestation {
            receipt.intent_digest = [0; 32];
        }
        assert!(rejected(&view, finished(invalid)));
        apply(&mut view, finished(guarded.clone())).expect("Guard attestation");
        assert_eq!(
            view.runs[0].tools.last().unwrap().observation.as_ref(),
            Some(&guarded)
        );
    }
}

#[test]
fn exact_file_search_replays_its_in_process_observation() {
    let (mut view, goal, run, agent) = primary_tool_session(false);
    let intent = child_intent(
        run,
        agent,
        ToolCall::Search {
            path: "src/lib.rs".into(),
            query: "needle".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
    );
    apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
    apply(&mut view, requesting_step(run, agent, 1)).expect("one-request step");
    apply(&mut view, tool_started(run, agent, &intent)).expect("exact Search starts");
    let mut observation = observation_for(intent.clone(), b"");
    observation.output =
        r#"{"entries":[{"path":"src/lib.rs","line":1,"text":"needle"}],"limit":100,"may_be_truncated":false,"next_offset":null}"#.into();
    assert!(matches!(
        observation.attestation,
        Some(crate::tools::Attestation::InProcess { .. })
    ));
    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: agent,
            observation: observation.clone(),
        },
    )
    .expect("in-process Search attestation");
    assert_eq!(
        view.runs[0].tools.last().unwrap().observation.as_ref(),
        Some(&observation)
    );
}

#[test]
fn continuation_requires_all_requested_observed() {
    let (mut view, goal, run, agent) = primary_tool_session(true);
    let (first, second) = (
        read_intent(run, agent, "src/lib.rs"),
        read_intent(run, agent, "src/lib.rs"),
    );
    apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
    apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
    apply(&mut view, tool_proposed(run, agent, &first)).expect("first request is proposed");
    apply(&mut view, tool_proposed(run, agent, &second)).expect("second request is proposed");
    apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
    apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
    apply(&mut view, reserve_child_call(goal, run, agent))
        .expect("a pending Tool can reserve its review call");
    let continuation = Event::ProviderCallRecorded {
        run_id: run,
        agent_run_id: agent,
        record: record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::Finished,
            Some(0),
        ),
    };
    assert!(
        rejected(&view, continuation.clone()),
        "the second declared request is unobserved"
    );
    apply(&mut view, tool_started(run, agent, &second)).expect("second request starts");
    apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second observed");
    apply(&mut view, continuation).expect("all declared requests observed");
}

fn definition_preparation(
    model: Option<&str>,
) -> (SessionView, GoalId, EffectIntent, ToolObservation) {
    let (session, goal, run, actor) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
    );
    let files = vec![crate::tools::definitions::DefinitionFilePin {
        private: false,
        filename: "reader.md".into(),
        digest: [3; 32],
    }];
    let mut configured = config(CollaborationPolicy::Single, true);
    configured.agent_definitions = Some(crate::tools::DefinitionRegistryPin {
        digest: crate::validate::digest(&serde_json::to_vec(&files).unwrap()),
        files,
    });
    let mut events = vec![goal_started(goal, GoalBudget::default())];
    events.extend(start_run(goal, run, actor, configured));
    let view = SessionView::replay_open(session, &session_with(session, events))
        .unwrap()
        .unwrap();
    let intent = child_intent(
        run,
        actor,
        ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: [3; 32],
        },
    );
    let output = serde_json::json!({"Definition":{"definition":{"version":1,"name":"reader","description":"Inspect","tools":["read"],"disallowed_tools":[],"model":model,"permission_mode":null,"max_turns":17,"isolation":null,"background":false,"body":"Untrusted guidance"},"notes":[]}}).to_string();
    let observation = ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        output,
        attestation: Some(crate::tools::Attestation::Guard(
            crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            },
        )),
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::NotApplicable,
        enforcement_cause: None,
        limit_cause: None,
    };
    (view, goal, intent, observation)
}

#[test]
fn agent_definition_initialization_replay_requires_pinned_guard_observations() {
    let (mut view, goal, intent, observation) = definition_preparation(None);
    let reserve = || reserve_child_call(goal, intent.run_id, intent.agent_run_id);
    assert!(
        rejected(&view, reserve()),
        "no Provider disclosure before parsing"
    );
    let start = |intent: EffectIntent| Event::ToolStarted {
        verdict: None,
        run_id: intent.run_id,
        agent_run_id: intent.agent_run_id,
        intent: intent.into(),
        active_ceiling_ms: 0,
    };
    for changed in [
        ToolCall::ParseAgentDefinition {
            index: 1,
            raw_digest: [3; 32],
        },
        ToolCall::ParseAgentDefinition {
            index: 0,
            raw_digest: [4; 32],
        },
    ] {
        let mut wrong = intent.clone();
        wrong.call = changed;
        assert!(rejected(&view, start(wrong)));
    }
    apply(&mut view, start(intent.clone())).unwrap();
    assert!(
        rejected(&view, reserve()),
        "unfinished parser is not permission"
    );
    let mut in_process = observation.clone();
    in_process.attestation = Some(crate::tools::Attestation::InProcess {
        receipt: None,
        policy_digest: intent.policy_digest,
        workspace_device: 1,
        workspace_inode: 2,
    });
    assert!(rejected(
        &view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            observation: in_process
        }
    ));
    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            observation,
        },
    )
    .unwrap();
    assert!(
        rejected(&view, start(intent.clone())),
        "duplicate parse rejected"
    );
    apply(&mut view, reserve()).unwrap();
    apply(
        &mut view,
        Event::ProviderCallRecorded {
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                Some(0),
            ),
        },
    )
    .unwrap();
    assert!(rejected(&view, start(intent)), "no late registry refresh");
}

#[test]
fn agent_definition_replay_rejects_noncurrent_normalization() {
    for case in ["missing-field", "unknown-field", "failed"] {
        let (mut view, goal, intent, mut observation) = definition_preparation(None);
        let mut output: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        match case {
            "missing-field" => {
                output["Definition"]["definition"]
                    .as_object_mut()
                    .unwrap()
                    .remove("model");
            }
            "unknown-field" => {
                output["Definition"]["definition"]["hooks"] = serde_json::json!({});
            }
            "failed" => {
                observation.disposition = ToolDisposition::Failed;
            }
            _ => {}
        }
        observation.output = output.to_string();
        apply(
            &mut view,
            Event::ToolStarted {
                verdict: None,
                run_id: intent.run_id,
                agent_run_id: intent.agent_run_id,
                intent: intent.clone().into(),
                active_ceiling_ms: 0,
            },
        )
        .unwrap();
        apply(
            &mut view,
            Event::ToolFinished {
                checkpoint: None,
                run_id: intent.run_id,
                agent_run_id: intent.agent_run_id,
                observation,
            },
        )
        .unwrap();
        assert!(
            rejected(
                &view,
                reserve_child_call(goal, intent.run_id, intent.agent_run_id)
            ),
            "no egress: {case}"
        );
    }
}

#[test]
fn checkpoint_replay_requires_publication_status_and_tracks_first_touch_and_gaps() {
    use crate::{Checkpoint, CheckpointReason, CheckpointStatus};
    for multiedit in [false, true] {
        let (mut view, goal, run, agent) = primary_tool_session(true);
        view.goals[0].plan_contract.as_mut().unwrap().paths = vec!["src".into()];
        for (before, after, status) in [
            ("original", "one", CheckpointStatus::Captured),
            ("one", "two", CheckpointStatus::NotNeeded),
            ("external", "three", CheckpointStatus::Captured),
        ] {
            let intent = child_intent(
                run,
                agent,
                if multiedit {
                    ToolCall::MultiEdit {
                        file_path: "src/lib.rs".into(),
                        expected_digest: Some(crate::validate::hex_digest(before.as_bytes())),
                        edits: vec![crate::LiteralEdit {
                            old_string: before.into(),
                            new_string: after.into(),
                            replace_all: false,
                        }],
                    }
                } else {
                    ToolCall::Write {
                        path: "src/lib.rs".into(),
                        expected_digest: Some(crate::validate::hex_digest(before.as_bytes())),
                        content: after.into(),
                    }
                },
            );
            apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
            apply(&mut view, requesting_step(run, agent, 1)).unwrap();
            apply(&mut view, tool_started(run, agent, &intent)).unwrap();
            let observation = observation_for(intent, after.as_bytes());
            let finish = |checkpoint| Event::ToolFinished {
                run_id: run,
                agent_run_id: agent,
                observation: observation.clone(),
                checkpoint,
            };
            assert!(
                rejected(&view, finish(None)),
                "a GO publication requires checkpoint metadata"
            );
            let invalid = Checkpoint {
                status: if status == CheckpointStatus::Captured {
                    CheckpointStatus::NotNeeded
                } else {
                    CheckpointStatus::Captured
                },
                reason: None,
                pruned_before_run: None,
            };
            assert!(rejected(&view, finish(Some(invalid))));
            assert!(rejected(
                &view,
                finish(Some(Checkpoint {
                    status,
                    reason: Some(CheckpointReason::Quota),
                    pruned_before_run: None
                }))
            ));
            assert!(rejected(
                &view,
                finish(Some(Checkpoint {
                    status,
                    reason: None,
                    pruned_before_run: Some(RunId::new()),
                }))
            ));
            for field in ["before", "bytes", "sha256"] {
                let mut forged = observation.clone();
                let mut output: serde_json::Value = serde_json::from_str(&forged.output).unwrap();
                output.as_object_mut().unwrap().remove(field);
                forged.output = output.to_string();
                assert!(rejected(
                    &view,
                    Event::ToolFinished {
                        run_id: run,
                        agent_run_id: agent,
                        observation: forged,
                        checkpoint: Some(Checkpoint {
                            status,
                            reason: None,
                            pruned_before_run: None
                        })
                    }
                ));
            }
            let mut forged = observation.clone();
            if let Some(crate::tools::Attestation::Guard(receipt)) = &mut forged.attestation {
                receipt.intent_digest = [0; 32];
            }
            assert!(rejected(
                &view,
                Event::ToolFinished {
                    run_id: run,
                    agent_run_id: agent,
                    observation: forged,
                    checkpoint: Some(Checkpoint {
                        status,
                        reason: None,
                        pruned_before_run: None
                    })
                }
            ));
            let event = finish(Some(Checkpoint {
                status,
                reason: None,
                pruned_before_run: None,
            }));
            assert_eq!(
                Event::decode(
                    event.kind(),
                    crate::session::payload_version(event.kind()).unwrap(),
                    &event.payload().unwrap(),
                    Some(run),
                    Some(agent)
                )
                .unwrap(),
                event
            );
            apply(
                &mut view,
                finish(Some(Checkpoint {
                    status,
                    reason: None,
                    pruned_before_run: Some(run),
                })),
            )
            .unwrap();
        }
        let last = view.runs[0]
            .tools
            .last()
            .unwrap()
            .checkpoint
            .as_ref()
            .unwrap();
        assert_eq!(last.status, CheckpointStatus::Captured);
    }
}
