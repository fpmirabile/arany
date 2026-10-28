use super::super::test_support::*;
use super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::{ChildGrant, CollaborationPolicy, GoalSuccessCriterion};
use crate::tools::{CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation};

#[test]
fn interrupted_goal_without_admissible_run_closes_as_failed() {
    let goal = GoalId::new();
    let (run, agent) = (RunId::new(), AgentRunId::new());
    let budget = GoalBudget {
        max_runs: 1,
        ..GoalBudget::default()
    };
    let close = Event::GoalFinished {
        goal_id: goal,
        status: GoalStatus::Failed,
        active_ms: 0,
        report: GoalReport {
            summary: "Out of budget.".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        },
        exhausted: true,
        budget_refusal: budget.refusal(
            crate::session::GoalUsage {
                runs: 1,
                provider_calls: 0,
                active_ms: 0,
                known_tokens: 0,
                token_usage_unknown: false,
            },
            crate::BudgetAction::Resume,
        ),
    };
    let mut events = vec![goal_started(goal, budget)];
    events.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(cancel_run(run, agent));
    let session = SessionId::new();
    let mut closing = events.clone();
    closing.push(close.clone());
    for mutate in [
        |r: &mut crate::BudgetRefusal| r.limit = 2,
        |r: &mut crate::BudgetRefusal| r.used = 0,
        |r: &mut crate::BudgetRefusal| r.run_id = Some(RunId::new()),
        |r: &mut crate::BudgetRefusal| {
            r.charge_basis = crate::BudgetChargeBasis::Conservative;
        },
    ] {
        let mut forged = closing.clone();
        if let Event::GoalFinished {
            budget_refusal: Some(refusal),
            ..
        } = forged.last_mut().unwrap()
        {
            mutate(refusal);
        }
        assert!(SessionView::replay_open(session, &session_with(session, forged)).is_err());
    }
    let view = SessionView::replay_open(session, &session_with(session, closing))
        .expect("an exhausted paused Goal closes as Failed")
        .expect("Session exists");
    assert_eq!(view.goal(goal).unwrap().status, GoalStatus::Failed);
    assert_eq!(view.interrupted_goal_id(), None);

    let session = SessionId::new();
    let mut admissible = vec![goal_started(goal, GoalBudget::default())];
    admissible.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, false),
    ));
    admissible.extend(cancel_run(run, agent));
    admissible.push(close);
    assert!(
        SessionView::replay_open(session, &session_with(session, admissible)).is_err(),
        "a paused Goal that still admits a Run cannot close as Failed"
    );
}

#[test]
fn exhausted_goal_closes_over_a_crash_left_provider_call() {
    let (goal, run, agent) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let report = GoalReport {
        summary: "Goal budget is exhausted.".into(),
        changes: Vec::new(),
        evidence: Vec::new(),
        unresolved: Vec::new(),
        cached_input_tokens: 0,
    };
    let close = Event::GoalFinished {
        goal_id: goal,
        status: GoalStatus::Failed,
        active_ms: 0,
        report,
        exhausted: true,
        budget_refusal: None,
    };
    let budget = GoalBudget {
        max_provider_calls: Some(1),
        ..GoalBudget::default()
    };
    // The process died inside the only admitted provider call: no RunFinished was journaled.
    let mut crashed = vec![goal_started(goal, budget)];
    crashed.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, false),
    ));
    crashed.push(Event::GoalProviderCallReserved {
        goal_id: goal,
        run_id: run,
        agent_run_id: agent,
        active_ceiling_ms: 0,
    });
    let session = SessionId::new();
    let mut closing = crashed.clone();
    closing.push(close.clone());
    let view = SessionView::replay_open(session, &session_with(session, closing))
        .expect("an exhausted crash-left Goal closes as Failed")
        .expect("Session exists");
    let goal_view = view.goal(goal).expect("Goal");
    assert_eq!(goal_view.status, GoalStatus::Failed);
    assert_eq!(goal_view.active_run, None);
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert_eq!(view.interrupted_goal_id(), None);

    let session = SessionId::new();
    let mut admissible = vec![goal_started(goal, GoalBudget::default())];
    admissible.extend(start_run(
        goal,
        run,
        agent,
        config(CollaborationPolicy::Single, false),
    ));
    admissible.push(Event::GoalProviderCallReserved {
        goal_id: goal,
        run_id: run,
        agent_run_id: agent,
        active_ceiling_ms: 0,
    });
    admissible.push(close);
    assert!(
        SessionView::replay_open(session, &session_with(session, admissible)).is_err(),
        "a crash-left Goal that still admits a Run cannot close as Failed"
    );
}

#[test]
fn exhausted_goal_closes_over_a_crash_left_compaction_call() {
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let budget = GoalBudget {
        max_provider_calls: Some(1),
        ..GoalBudget::default()
    };
    let mut events = vec![goal_started(goal_id, budget)];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend(cancel_run(run_id, agent_run_id));
    // The only admitted call is the compaction call, whose outcome never committed.
    events.push(Event::GoalCompactionCallReserved {
        goal_id,
        run_id,
        active_ceiling_ms: 0,
    });
    events.push(Event::GoalFinished {
        goal_id,
        status: GoalStatus::Failed,
        active_ms: 0,
        report: GoalReport {
            summary: "Goal budget is exhausted.".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        },
        exhausted: true,
        budget_refusal: None,
    });

    let session = SessionId::new();
    let view = SessionView::replay_open(session, &session_with(session, events))
        .expect("an exhausted Goal closes over its crash-left compaction call")
        .expect("Session exists");
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(goal.status, GoalStatus::Failed);
    assert_eq!(goal.compaction_call_reserved, None);
    assert_eq!(goal.uncertain_compaction_calls, 1);
}

#[test]
fn active_time_stop_margin_counts_as_spent_before_the_limit() {
    let budget = GoalBudget {
        max_active_ms: Some(60_000),
        ..GoalBudget::default()
    };
    let usage = |active_ms| crate::session::GoalUsage {
        runs: 0,
        provider_calls: 0,
        active_ms,
        known_tokens: 0,
        token_usage_unknown: false,
    };
    let margin = crate::session::STORE_ACTION_OVERHEAD_MS;
    assert!(!budget.spent(usage(60_000 - margin - 1)));
    assert!(budget.spent(usage(60_000 - margin)));
    assert!(budget.admits_next_run(usage(60_000 - margin)));
    assert!(budget.spent(usage(60_000)));
}

fn budget_closed_proposal_prefix(
    goal: GoalId,
    run: RunId,
    agent: AgentRunId,
) -> (Vec<Event>, EffectIntent) {
    let mut pinned = config(CollaborationPolicy::Single, true);
    pinned.tool_policy.as_mut().unwrap().permissions_digest = Some([3; 32]);
    let mut events = vec![goal_started(
        goal,
        GoalBudget {
            max_provider_calls: Some(1),
            ..GoalBudget::default()
        },
    )];
    events.extend(start_run(goal, run, agent, pinned));
    let intent = read_intent(run, agent, "src/lib.rs");
    events.extend([
        reserve_child_call(goal, run, agent),
        requesting_step(run, agent, 1),
        tool_proposed(run, agent, &intent),
    ]);
    (events, intent)
}

fn proposal_pre_hook_events(
    intent: &EffectIntent,
    disposition: Option<ToolDisposition>,
) -> Vec<Event> {
    use crate::tools::permissions::{HookEvent, HookInvocation, HookOutcome, HookResult};
    let mut events = Vec::new();
    let hook_intent = child_intent(
        intent.run_id,
        intent.agent_run_id,
        ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: String::new(),
            resource_class: None,
            timeout_ms: None,
        },
    );
    events.push(Event::HookStarted {
        run_id: intent.run_id,
        agent_run_id: intent.agent_run_id,
        hook: HookInvocation {
            active_ceiling_ms: None,
            event: HookEvent::PreToolUse,
            group: 0,
            handler: 0,
            subject_digest: intent.digest(),
            intent: hook_intent.clone(),
            input_digest: [9; 32],
        },
    });
    if let Some(disposition) = disposition {
        let mut observation = observation_for(hook_intent, b"");
        observation.disposition = disposition;
        observation.attestation = None;
        observation.cache_update = CacheUpdate::NotAttempted;
        events.push(Event::HookFinished {
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            outcome: HookOutcome {
                boundary_active_ms: None,
                result: HookResult::for_event(HookEvent::PreToolUse, &observation),
                observation,
            },
        });
    }
    events
}

#[test]
fn budget_continuation_of_a_proposal_requires_settled_hooks() {
    #[derive(Clone, Copy, Debug)]
    enum Pending {
        Approval,
        Hook,
        UncertainHook,
    }
    for pending in [Pending::Approval, Pending::Hook, Pending::UncertainHook] {
        let (session, goal, run, agent) = (
            SessionId::new(),
            GoalId::new(),
            RunId::new(),
            AgentRunId::new(),
        );
        let (mut events, intent) = budget_closed_proposal_prefix(goal, run, agent);
        if !matches!(pending, Pending::Approval) {
            events.extend(proposal_pre_hook_events(
                &intent,
                matches!(pending, Pending::UncertainHook).then_some(ToolDisposition::Uncertain),
            ));
        }
        events.push(Event::GoalFinished {
            goal_id: goal,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "Goal budget is exhausted.".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: true,
            budget_refusal: None,
        });
        let mut history = session_with(session, events);
        let view = SessionView::replay(session, &history).unwrap().unwrap();
        let continuable = matches!(pending, Pending::Approval);
        assert_eq!(
            view.continuable_goal().map(|goal| goal.id),
            continuable.then_some(goal),
            "{pending:?}"
        );
        history.push(EventEnvelope {
            session_id: session,
            sequence: view.last_sequence + 1,
            run_id: None,
            agent_run_id: None,
            created_at_ms: 0,
            event: goal_started(GoalId::new(), GoalBudget::default()),
        });
        let Event::GoalStarted { continues, .. } = &mut history.last_mut().unwrap().event else {
            unreachable!()
        };
        *continues = Some(goal);
        assert_eq!(
            SessionView::replay_open(session, &history).is_ok(),
            continuable,
            "{pending:?}"
        );
    }
}

#[test]
fn goal_continuation_replays_only_from_the_unclaimed_budget_closed_source() {
    let (source, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let mut events = vec![started_with(
        source,
        GoalBudget {
            max_provider_calls: Some(2),
            ..GoalBudget::default()
        },
        true,
        None,
    )];
    events.extend(start_run(
        source,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
    ));
    events.extend([
        Event::GoalProviderCallReserved {
            goal_id: source,
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
        Event::GoalCompactionCallReserved {
            goal_id: source,
            run_id,
            active_ceiling_ms: 0,
        },
        Event::GoalFinished {
            goal_id: source,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "Goal budget is exhausted.".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: true,
            budget_refusal: None,
        },
    ]);
    let session = SessionId::new();
    let replay = |extra: Vec<Event>| {
        let mut all = events.clone();
        all.extend(extra);
        SessionView::replay_open(session, &session_with(session, all))
    };
    let continued = |goal_id, edit: &dyn Fn(&mut Event)| {
        let mut event = started_with(goal_id, GoalBudget::default(), true, Some(source));
        edit(&mut event);
        event
    };

    let base = replay(Vec::new())
        .expect("source replays")
        .expect("Session");
    assert_eq!(base.continuable_goal().map(|goal| goal.id), Some(source));
    let mut failed = base.clone();
    failed.goals[0].budget_closed = false;
    assert!(
        failed.continuable_goal().is_none(),
        "a Goal not closed by exhaustion is not continuable"
    );

    for (disposition, cache_update) in [
        (
            ToolDisposition::Succeeded,
            crate::tools::CacheUpdate::Uncertain,
        ),
        (
            ToolDisposition::Uncertain,
            crate::tools::CacheUpdate::NotApplicable,
        ),
    ] {
        let mut unknown = base.clone();
        let intent = read_intent(run_id, agent_run_id, "src/lib.rs");
        unknown.runs[0].tools.push(ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: Some(ToolObservation {
                spill: None,
                intent,
                disposition,
                enforcement_cause: None,
                limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
                    .then_some(crate::tools::LimitCause::Resource),
                output: String::new(),
                attestation: None,
                approval_cause: None,
                guard_stage: None,
                cache_update,
            }),
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(0),
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: Some(2),
        });
        assert!(
            unknown.continuable_goal().is_none(),
            "an unknown Tool outcome in the last Run ({disposition:?}, {cache_update:?})"
        );
    }

    let mut newer = base.clone();
    let mut newer_goal = newer.goals[0].clone();
    let mut newer_run = newer.runs[0].clone();
    newer_goal.id = GoalId::new();
    newer_run.id = RunId::new();
    newer_goal.run_ids = vec![newer_run.id];
    let intent = read_intent(newer_run.id, agent_run_id, "src/lib.rs");
    newer_run.tools.push(ToolView {
        checkpoint: None,
        intent: intent.clone(),
        observation: Some(ToolObservation {
            spill: None,
            intent,
            disposition: ToolDisposition::Uncertain,
            enforcement_cause: None,
            limit_cause: None,
            output: String::new(),
            attestation: None,
            approval_cause: None,
            guard_stage: None,
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        }),
        review_call_index: None,
        active_time_reserved: false,
        narrowing_source: None,
        permission_verdict: None,
        started_sequence: Some(0),
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: Some(2),
    });
    newer.goals.push(newer_goal);
    newer.runs.push(newer_run);
    assert!(
        newer.continuable_goal().is_none(),
        "an unknown outcome in the latest budget-closed Goal never falls back to an older one"
    );

    let mut cancelled = base.clone();
    let intent = read_intent(run_id, agent_run_id, "src/lib.rs");
    cancelled.runs[0].tools.push(ToolView {
        checkpoint: None,
        intent: intent.clone(),
        observation: Some(ToolObservation {
            spill: None,
            intent,
            disposition: ToolDisposition::Cancelled,
            enforcement_cause: None,
            limit_cause: None,
            output: String::new(),
            attestation: None,
            approval_cause: None,
            guard_stage: None,
            cache_update: crate::tools::CacheUpdate::NotApplicable,
        }),
        review_call_index: None,
        active_time_reserved: false,
        narrowing_source: None,
        permission_verdict: None,
        started_sequence: Some(0),
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: Some(2),
    });
    assert_eq!(
        cancelled.continuable_goal().map(|goal| goal.id),
        Some(source),
        "a Tool cancelled by the user has a known outcome"
    );

    let target = GoalId::new();
    let view = replay(vec![continued(target, &|_| {})])
        .expect("a matching continuation replays")
        .expect("Session");
    assert_eq!(
        view.goal(target).and_then(|goal| goal.continues),
        Some(source)
    );
    assert!(view.continuable_goal().is_none(), "the source is claimed");

    assert!(
        replay(vec![
            continued(target, &|_| {}),
            continued(GoalId::new(), &|_| {}),
        ])
        .is_err(),
        "a second continuation of one source"
    );
    for paths in [None, Some(vec!["new-input.txt".into()])] {
        assert!(
            replay(vec![continued(GoalId::new(), &|event| {
                if let Event::GoalStarted { include_paths, .. } = event {
                    *include_paths = paths.clone();
                }
            })])
            .is_err(),
            "continuation cannot replace admitted inputs"
        );
    }
    let mut migrated = events.clone();
    migrated.push(continued(GoalId::new(), &|_| {}));
    for event in &mut migrated {
        if let Event::GoalStarted { include_paths, .. } = event {
            *include_paths = None;
        }
    }
    assert!(
        SessionView::replay_open(session, &session_with(session, migrated)).is_ok(),
        "already-recorded legacy continuations replay with unknown inputs"
    );
    let mismatches: [&dyn Fn(&mut Event); 3] = [
        &|event| {
            if let Event::GoalStarted { objective, .. } = event {
                objective.push('!');
            }
        },
        &|event| {
            if let Event::GoalStarted { phase, .. } = event {
                *phase = GoalPhase::Plan;
            }
        },
        &|event| {
            if let Event::GoalStarted { autonomous, .. } = event {
                *autonomous = false;
            }
        },
    ];
    for edit in mismatches {
        assert!(replay(vec![continued(GoalId::new(), edit)]).is_err());
    }
    let unknown = continued(GoalId::new(), &|event| {
        if let Event::GoalStarted { continues, .. } = event {
            *continues = Some(GoalId::new());
        }
    });
    assert!(replay(vec![unknown]).is_err());
}

#[test]
fn exhausted_autonomous_goal_closes_over_a_crash_left_compaction_call() {
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let budget = GoalBudget {
        max_provider_calls: Some(2),
        ..GoalBudget::default()
    };
    let mut events = vec![Event::GoalStarted {
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
        autonomous: true,
        continues: None,
    }];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
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
            ProviderCallDisposition::Finished,
            None,
        ),
    });
    events.extend([
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
    ]);
    // The Run finished, but its between-Run compaction call never committed: the Goal is paused.
    events.push(Event::GoalCompactionCallReserved {
        goal_id,
        run_id,
        active_ceiling_ms: 0,
    });
    events.push(Event::GoalFinished {
        goal_id,
        status: GoalStatus::Failed,
        active_ms: 0,
        report: GoalReport {
            summary: "Goal budget is exhausted.".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        },
        exhausted: true,
        budget_refusal: None,
    });

    let session = SessionId::new();
    let view = SessionView::replay_open(session, &session_with(session, events))
        .expect("an exhausted autonomous Goal closes over its crash-left compaction call")
        .expect("Session exists");
    let goal = view.goal(goal_id).expect("Goal");
    assert_eq!(goal.status, GoalStatus::Failed);
    assert_eq!(goal.uncertain_compaction_calls, 1);
}

#[test]
fn finish_with_unknown_usage_is_not_a_tool_or_delegate_outcome() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let budget = GoalBudget {
        max_tokens: Some(1000),
        ..GoalBudget::default()
    };
    let mut events = vec![
        Event::SessionStarted {
            title: "Goal".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        goal_started(goal_id, budget),
    ];
    events.extend(start_run(
        goal_id,
        run_id,
        agent_run_id,
        config(CollaborationPolicy::Single, false),
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
            ProviderCallDisposition::Finished,
            None,
        ),
    });
    events.push(Event::AgentFinished {
        worker: None,
        run_id,
        agent_run_id,
        disposition: AgentDisposition::Finished,
        summary: Some("done".into()),
        result: Some("done".into()),
    });

    let mut forged = events.clone();
    if let Some(Event::AgentFinished { summary, .. }) = forged.last_mut() {
        *summary = Some("another summary".into());
    }
    assert!(
        SessionView::replay_open(session_id, &journal(session_id, forged)).is_err(),
        "the summary must be derived from the result"
    );
    let view = SessionView::replay_open(session_id, &journal(session_id, events))
        .expect("unknown usage does not block an Answer")
        .expect("Session exists");
    assert_eq!(view.runs[0].agents[0].status, AgentStatus::Finished);
}

#[test]
fn finished_run_fails_only_an_unsatisfied_planned_goal_at_its_active_limit() {
    let limit = 1000;
    // (plan contract, its criterion satisfied, settled active time, Failed accepted)
    for (planned, satisfied, settled, accepted) in [
        (true, false, limit, true),
        (true, false, limit - 1, false),
        (false, false, limit, false),
        (true, true, limit, false),
    ] {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let budget = GoalBudget {
            max_active_ms: Some(limit),
            ..GoalBudget::default()
        };
        let mut events = vec![
            Event::SessionStarted {
                title: "Goal".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
            },
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
                autonomous: true,
                continues: None,
            },
        ];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        if let Some(Event::RunStarted {
            active_deadline_ms, ..
        }) = events.get_mut(4)
        {
            *active_deadline_ms = Some(limit);
        }
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
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        events.push(Event::AgentFinished {
            worker: None,
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
        });
        events.push(Event::MessageCommitted {
            run_id,
            text: "done".into(),
        });
        events.push(Event::RunFinished {
            run_id,
            disposition: RunDisposition::Finished,
            failure: None,
            active_ms: settled,
        });
        let mut view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("Finished Run before the Goal receipt")
            .expect("Session");
        if planned {
            let goal = view.goals.last_mut().expect("Goal");
            goal.plan_contract = Some(GoalPlanContract {
                mission: GOAL_OBJECTIVE.into(),
                resource_class: crate::tools::ToolResourceClass::Default,
                command_timeout_ms: None,
                paths: Vec::new(),
                commands: Vec::new(),
                skills: Vec::new(),
                mcp_tools: Vec::new(),
                success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                    path: "src/lib.rs".into(),
                    sha256: crate::validate::hex_digest(b"done"),
                },
                budget: GoalBudget {
                    max_active_ms: Some(limit),
                    ..GoalBudget::default()
                },
            });
            goal.success_criterion_satisfied = satisfied;
        }
        let report = GoalReport {
            summary: "budget".into(),
            changes: Vec::new(),
            evidence: Vec::new(),
            unresolved: Vec::new(),
            cached_input_tokens: 0,
        };
        let mut unflagged = view.clone();
        let receipt = view.finish_goal(
            &goal_id,
            &GoalStatus::Failed,
            &settled,
            &report,
            true,
            None,
            0,
        );
        assert!(
            unflagged
                .finish_goal(
                    &goal_id,
                    &GoalStatus::Failed,
                    &settled,
                    &report,
                    false,
                    None,
                    0
                )
                .is_err(),
            "a budget close of a Finished Run must carry exhausted"
        );
        assert_eq!(
            receipt.is_ok(),
            accepted,
            "a Failed receipt over a Finished Run needs an unsatisfied plan and spent active time (planned {planned}, satisfied {satisfied}, {settled} of {limit})"
        );
    }
}

#[test]
fn child_admission_uses_the_effective_plan_token_ceiling() {
    let mut below_cap = delegated_session(999);
    assert!(apply_child_spawn(&mut below_cap, ChildGrant::NoTools).is_ok());

    let mut at_cap = delegated_session(1000);
    assert!(apply_child_spawn(&mut at_cap, ChildGrant::NoTools).is_err());
}

#[test]
fn goal_report_cached_tokens_must_match_the_goal_total() {
    let session_id = SessionId::new();
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let cached_call = ProviderCallRecord {
        cache_read_input_tokens: Some(5),
        ..record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::Finished,
            Some(5),
        )
    };
    let finish = |cached_input_tokens| {
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend([
            Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
                active_ceiling_ms: 0,
            },
            Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: cached_call.clone(),
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
                    cached_input_tokens,
                },
                exhausted: false,
                budget_refusal: None,
            },
        ]);
        SessionView::replay(session_id, &session_with(session_id, events))
    };
    let view = finish(5).expect("matching cache total").expect("Session");
    assert_eq!(
        view.goals[0].report.as_ref().map(|r| r.cached_input_tokens),
        Some(5)
    );
    assert!(
        finish(0).is_err(),
        "a report may not understate cache reads"
    );
    assert!(finish(6).is_err(), "a report may not overstate cache reads");
}
