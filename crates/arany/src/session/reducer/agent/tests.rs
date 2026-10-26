use super::super::test_support::*;
use super::super::*;
use crate::provider::AgentPhase;
use crate::session::events::{ChildGrant, CollaborationPolicy};
use crate::tools::ToolCall;

#[test]
fn assigned_profile_replay_checks_spawn_chain_and_exact_output_cap() {
    use crate::provider::{
        NamedProviderRoutes, ProviderFallback, ProviderRole, Providers, RosterSnapshot,
    };
    for inherited in [false, true] {
        let (session, goal, run, primary, child) = (
            SessionId::new(),
            GoalId::new(),
            RunId::new(),
            AgentRunId::new(),
            AgentRunId::new(),
        );
        let mut config = config(
            CollaborationPolicy::Team {
                max_active_children: 2,
            },
            false,
        );
        let main = config.identity().unwrap();
        let mut first = main.clone();
        first.model = "shared-first".into();
        let mut default = main.clone();
        default.model = "default-fallback".into();
        let mut spare = main.clone();
        spare.model = "spare-fallback".into();
        spare.limits.max_output_tokens = 16;
        let mut snapshot = RosterSnapshot {
            preset: "coding".into(),
            selection_digest: [0; 32],
            profiles: vec![
                NamedProviderRoutes {
                    name: "child".into(),
                    routes: if inherited {
                        vec![main.clone()]
                    } else {
                        vec![first.clone(), default.clone()]
                    },
                },
                NamedProviderRoutes {
                    name: "main".into(),
                    routes: vec![main],
                },
                NamedProviderRoutes {
                    name: "spare".into(),
                    routes: vec![first.clone(), spare.clone()],
                },
            ],
            providers: Providers {
                primary: "main".into(),
                child: "child".into(),
                review: "main".into(),
                compaction: "main".into(),
            },
            child_profiles: vec!["child".into(), "spare".into()],
        };
        snapshot.selection_digest = snapshot.digest().unwrap();
        config.providers = if inherited {
            None
        } else {
            snapshot.role_routes()
        };
        config.roster = Some(Box::new(snapshot));
        let mut facts = vec![goal_started(goal, GoalBudget::default())];
        facts.extend(start_run(goal, run, primary, config));
        facts.extend([
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: primary,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Delegated,
                    Some(1),
                ),
            },
            Event::AgentSpawned {
                run_id: run,
                agent_run_id: child,
                role: AgentRole::Child,
                ordinal: 1,
                objective: Some("Inspect".into()),
                grant: Some(ChildGrant::NoTools),
                definition: None,
                profile: Some("spare".into()),
                assignment: None,
            },
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: child,
                active_ceiling_ms: 0,
            },
            Event::ProviderFallbackUsed {
                run_id: run,
                agent_run_id: child,
                fallback: ProviderFallback {
                    rate_limits: None,
                    role: ProviderRole::Child,
                    call_index: 0,
                    failure: ProviderCallDisposition::Unavailable,
                    from: first,
                    to: spare,
                },
            },
            Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: child,
                record: record(
                    AgentPhase::ChildWork,
                    ProviderCallDisposition::Finished,
                    Some(16),
                ),
            },
        ]);
        let valid = session_with(session, facts);
        let view = SessionView::replay_open(session, &valid).unwrap().unwrap();
        assert_eq!(view.runs[0].agents[1].profile.as_deref(), Some("spare"));
        assert_eq!(view.goals[0].provider_calls, 3);
        assert!(view.goals[0].token_usage_unknown);
        for change in 0..6 {
            let mut invalid = valid.clone();
            for event in &mut invalid {
                match &mut event.event {
                    Event::AgentSpawned {
                        agent_run_id,
                        profile,
                        ..
                    } if *agent_run_id == child && change < 3 => {
                        *profile = match change {
                            0 => None,
                            1 => Some("unknown".into()),
                            _ => Some("main".into()),
                        };
                    }
                    Event::AgentSpawned {
                        agent_run_id,
                        profile,
                        ..
                    } if *agent_run_id == primary && change == 3 => *profile = Some("child".into()),
                    Event::ProviderFallbackUsed { fallback, .. } if change == 4 => {
                        fallback.to = default.clone()
                    }
                    Event::ProviderCallRecorded {
                        agent_run_id,
                        record,
                        ..
                    } if *agent_run_id == child && change == 5 => record.output_tokens = Some(17),
                    _ => (),
                }
            }
            assert!(
                SessionView::replay_open(session, &invalid).is_err(),
                "tampering {change}"
            );
        }
    }
}

#[test]
fn provider_fallback_replay_requires_pinned_chain_reservation_role_and_budget() {
    use crate::provider::{ProviderFallback, ProviderRole, Providers};
    let (session, goal, run, actor) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
    );
    let mut config = config(CollaborationPolicy::Single, false);
    let primary = config.identity().unwrap();
    let mut alternate = primary.clone();
    alternate.model = "alternate-model".into();
    config.providers = Some(Providers::all(vec![primary.clone(), alternate.clone()]));
    let mut facts = vec![goal_started(goal, GoalBudget::default())];
    facts.extend(start_run(goal, run, actor, config));
    let mut completion = complete_run_and_goal(goal, run, actor);
    if let Event::GoalProviderCallReserved {
        active_ceiling_ms, ..
    } = &mut completion[0]
    {
        *active_ceiling_ms = 7_000;
    }
    completion.insert(
        1,
        Event::ProviderFallbackUsed {
            run_id: run,
            agent_run_id: actor,
            fallback: ProviderFallback {
                rate_limits: None,
                role: ProviderRole::Primary,
                call_index: 0,
                from: primary,
                to: alternate,
                failure: ProviderCallDisposition::Unavailable,
            },
        },
    );
    facts.extend(completion);
    let valid = session_with(session, facts);
    let switch = valid
        .iter()
        .position(|event| matches!(event.event, Event::ProviderFallbackUsed { .. }))
        .unwrap();
    let open = SessionView::replay_open(session, &valid[..=switch])
        .unwrap()
        .unwrap();
    assert_eq!(open.goals[0].provider_calls, 2);
    assert_eq!(open.goals[0].unobserved_provider_calls, [actor]);
    assert!(open.goals[0].active_ceiling_ms > 0);
    assert!(open.runs[0].primary().unwrap().provider_calls.is_empty());
    let closed = SessionView::replay(session, &valid[..=switch])
        .unwrap()
        .unwrap();
    assert_eq!(
        closed.goals[0].resume_usage(&closed).active_ms,
        open.goals[0].active_ceiling_ms
    );
    assert_eq!(closed.goals[0].provider_calls, 2);
    let view = SessionView::replay(session, &valid).unwrap().unwrap();
    assert_eq!(view.goals[0].provider_calls, 2);
    assert!(view.goals[0].token_usage_unknown);
    assert_eq!(view.runs[0].provider_fallbacks.len(), 1);
    assert_eq!(
        view.runs[0].provider_fallbacks[0].1.role,
        ProviderRole::Primary
    );
    for case in 0..9 {
        let mut invalid = valid.clone();
        match case {
            0..=4 => {
                let fallback = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::ProviderFallbackUsed { fallback, .. } => Some(fallback),
                        _ => None,
                    })
                    .unwrap();
                match case {
                    0 => fallback.from.model = "unknown".into(),
                    1 => fallback.to.model = "unknown".into(),
                    2 => fallback.role = ProviderRole::Child,
                    3 => fallback.call_index = 1,
                    _ => fallback.failure = ProviderCallDisposition::Rejected,
                }
            }
            5..=6 => {
                let budget = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::GoalStarted { budget, .. } => Some(budget),
                        _ => None,
                    })
                    .unwrap();
                if case == 5 {
                    budget.max_provider_calls = Some(1);
                } else {
                    budget.max_tokens = Some(100);
                }
            }
            _ => {
                let config = invalid
                    .iter_mut()
                    .find_map(|event| match &mut event.event {
                        Event::RunStarted { config, .. } => Some(config),
                        _ => None,
                    })
                    .unwrap();
                let providers = config.providers.as_mut().unwrap();
                if case == 7 {
                    providers.primary[0].model = "different".into();
                } else {
                    providers.child[0].profile = "custom:unadmitted".into();
                }
            }
        }
        assert!(
            SessionView::replay(session, &invalid).is_err(),
            "case {case}"
        );
    }
    for before in [true, false] {
        let mut invalid: Vec<_> = valid.iter().map(|event| event.event.clone()).collect();
        let switch = invalid
            .iter()
            .position(|event| matches!(event, Event::ProviderFallbackUsed { .. }))
            .unwrap();
        if before {
            invalid.insert(
                switch,
                Event::AssistantDisplayRecorded {
                    run_id: run,
                    agent_run_id: actor,
                    chunk: crate::session::DisplayChunk {
                        call_index: 0,
                        phase: AgentPhase::RootPlan,
                        category: crate::session::DisplayCategory::Answer,
                        item: 0,
                        offset: 0,
                        text: "partial".into(),
                    },
                },
            );
        } else {
            invalid.swap(switch, switch + 1);
        }
        assert!(SessionView::replay(session, &journal(session, invalid)).is_err());
    }
}

#[test]
fn provider_retry_replay_requires_an_open_reservation_bounded_attempts_and_budget() {
    use crate::provider::{MAX_PROVIDER_RETRY_DELAY_MS, ProviderRetry, ProviderRole, RetryCause};
    let (session, goal, run, actor) = (
        SessionId::new(),
        GoalId::new(),
        RunId::new(),
        AgentRunId::new(),
    );
    let retry = |attempt: u8| Event::ProviderRetryScheduled {
        run_id: run,
        agent_run_id: actor,
        retry: ProviderRetry {
            rate_limits: None,
            role: ProviderRole::Primary,
            call_index: 0,
            attempt,
            cause: RetryCause::ServerError,
            delay_ms: 1000 << attempt,
        },
    };
    let mut facts = vec![goal_started(goal, GoalBudget::default())];
    facts.extend(start_run(
        goal,
        run,
        actor,
        config(CollaborationPolicy::Single, false),
    ));
    let mut completion = complete_run_and_goal(goal, run, actor);
    // The reservation admits 20 s of active time; the valid waits use 14 s of it.
    if let Event::GoalProviderCallReserved {
        active_ceiling_ms, ..
    } = &mut completion[0]
    {
        *active_ceiling_ms = 20_000;
    }
    for attempt in 1..=3 {
        completion.insert(usize::from(attempt), retry(attempt));
    }
    facts.extend(completion);
    let events: Vec<_> = facts.clone();
    let valid = session_with(session, facts);
    let view = SessionView::replay(session, &valid).unwrap().unwrap();
    assert_eq!(view.goals[0].provider_calls, 4);
    assert!(view.goals[0].token_usage_unknown);
    assert_eq!(view.runs[0].provider_retries.len(), 3);
    assert_eq!(view.usage.groups[0].input.unreported, 4);
    assert!(view.usage.pending.is_empty());
    for (end, _) in valid
        .iter()
        .enumerate()
        .filter(|(_, envelope)| matches!(envelope.event, Event::ProviderRetryScheduled { .. }))
    {
        // A crash after any retry fact keeps the open reservation and replays without dispatch.
        let open = SessionView::replay_open(session, &valid[..=end])
            .unwrap()
            .unwrap();
        assert_eq!(open.goals[0].unobserved_provider_calls, [actor]);
        assert_eq!(open.usage.pending, [(session, run, Some(actor))]);
        assert!(open.runs[0].primary().unwrap().provider_calls.is_empty());
    }
    let position = |events: &[Event], attempt: u8| {
        events
            .iter()
            .position(|event| {
                matches!(event, Event::ProviderRetryScheduled { retry, .. } if retry.attempt == attempt)
            })
            .unwrap()
    };
    let edit = |events: &mut [Event], attempt: u8, change: &dyn Fn(&mut ProviderRetry)| {
        let at = position(events, attempt);
        if let Event::ProviderRetryScheduled { retry, .. } = &mut events[at] {
            change(retry);
        }
    };
    for case in 0..13 {
        let mut invalid = events.clone();
        match case {
            12 => edit(&mut invalid, 3, &|retry| retry.delay_ms = 15_000),
            0 => edit(&mut invalid, 1, &|retry| retry.attempt = 2),
            1 => {
                let mut fourth = retry(3);
                if let Event::ProviderRetryScheduled { retry, .. } = &mut fourth {
                    retry.attempt = 4;
                }
                invalid.insert(position(&invalid, 3) + 1, fourth);
            }
            2 => edit(&mut invalid, 1, &|retry| retry.call_index = 1),
            3 => edit(&mut invalid, 1, &|retry| retry.role = ProviderRole::Child),
            4 => edit(&mut invalid, 1, &|retry| {
                retry.role = ProviderRole::Compaction
            }),
            5 => edit(&mut invalid, 1, &|retry| retry.delay_ms = 1999),
            6 => edit(&mut invalid, 3, &|retry| {
                retry.delay_ms = MAX_PROVIDER_RETRY_DELAY_MS + 1;
            }),
            7 | 8 => {
                let budget = invalid
                    .iter_mut()
                    .find_map(|event| match event {
                        Event::GoalStarted { budget, .. } => Some(budget),
                        _ => None,
                    })
                    .unwrap();
                if case == 7 {
                    budget.max_provider_calls = Some(3);
                } else {
                    budget.max_tokens = Some(100);
                }
            }
            9 => {
                let reserved = invalid
                    .iter()
                    .position(|event| matches!(event, Event::GoalProviderCallReserved { .. }))
                    .unwrap();
                let first = invalid.remove(position(&invalid, 1));
                invalid.insert(reserved, first);
            }
            10 => invalid.insert(
                position(&invalid, 1),
                Event::AssistantDisplayRecorded {
                    run_id: run,
                    agent_run_id: actor,
                    chunk: crate::session::DisplayChunk {
                        call_index: 0,
                        phase: AgentPhase::RootPlan,
                        category: crate::session::DisplayCategory::Answer,
                        item: 0,
                        offset: 0,
                        text: "partial".into(),
                    },
                },
            ),
            _ => {
                let recorded = invalid
                    .iter()
                    .position(|event| matches!(event, Event::ProviderCallRecorded { .. }))
                    .unwrap();
                let last = invalid.remove(position(&invalid, 3));
                invalid.insert(recorded, last);
            }
        }
        assert!(
            SessionView::replay(session, &session_with(session, invalid)).is_err(),
            "case {case}"
        );
    }
}

#[test]
fn research_child_review_fallback_requires_its_own_pending_tool() {
    use crate::provider::{ProviderFallback, ProviderRole, Providers};
    let (mut view, goal, run, child) = research_session();
    view.goals[0]
        .plan_contract
        .as_mut()
        .unwrap()
        .budget
        .max_tokens = None;
    let primary_agent = view.runs[0].primary().unwrap().id;
    let config = view.runs[0].config.as_mut().unwrap();
    let primary = config.identity().unwrap();
    let mut alternate = primary.clone();
    alternate.model = "alternate-model".into();
    config.providers = Some(Providers::all(vec![primary.clone(), alternate.clone()]));
    let fallback = Event::ProviderFallbackUsed {
        run_id: run,
        agent_run_id: child,
        fallback: ProviderFallback {
            rate_limits: None,
            role: ProviderRole::Review,
            call_index: 1,
            from: primary,
            to: alternate,
            failure: ProviderCallDisposition::Unavailable,
        },
    };
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
    let mut elsewhere = view.clone();
    elsewhere.runs[0].tools.push(ToolView {
        checkpoint: None,
        intent: child_intent(
            run,
            primary_agent,
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: None,
                limit: Some(1024),
            },
        ),
        observation: None,
        review_call_index: None,
        active_time_reserved: false,
        narrowing_source: None,
        permission_verdict: None,
        started_sequence: Some(0),
        hooks: Vec::new(),
        hook_sequence: None,
        finished_sequence: None,
    });
    apply(&mut elsewhere, reserve_child_call(goal, run, child)).expect("review reservation");
    assert!(rejected(&elsewhere, fallback.clone()));
    apply(
        &mut view,
        Event::ToolProposed {
            run_id: run,
            agent_run_id: child,
            intent: read.clone().into(),
            active_ceiling_ms: 0,
        },
    )
    .expect("child Tool starts");
    apply(&mut view, reserve_child_call(goal, run, child)).expect("review reservation");
    apply(&mut view, fallback).expect("child review fallback");
    assert_eq!(view.runs[0].provider_fallbacks.len(), 1);
    apply(&mut view, tool_started(run, child, &read)).unwrap();
    apply(
        &mut view,
        Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: child,
            observation: observation_for(read, b"ok"),
        },
    )
    .expect("child Tool finishes after the review switch");
}

#[test]
fn a_pending_child_review_admits_only_review_retries_within_one_bound() {
    use crate::provider::{ProviderRetry, ProviderRole, RetryCause};
    let (mut view, goal, run, child) = research_session();
    view.goals[0]
        .plan_contract
        .as_mut()
        .unwrap()
        .budget
        .max_tokens = None;
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
        Event::ToolProposed {
            run_id: run,
            agent_run_id: child,
            intent: read.into(),
            active_ceiling_ms: 0,
        },
    )
    .expect("child Tool starts");
    apply(
        &mut view,
        Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: child,
            active_ceiling_ms: 60_000,
        },
    )
    .expect("review reservation");
    let retry = |role, attempt: u8| Event::ProviderRetryScheduled {
        run_id: run,
        agent_run_id: child,
        retry: ProviderRetry {
            rate_limits: None,
            role,
            call_index: 1,
            attempt,
            cause: RetryCause::ServerError,
            delay_ms: 1000 << attempt,
        },
    };
    // The hostile prefix of three child-turn and three review retries on one reservation fails
    // at its first child-turn retry, and no role mix extends the one bound of the call.
    assert!(rejected(&view, retry(ProviderRole::Child, 1)));
    for attempt in 1..=3 {
        apply(&mut view, retry(ProviderRole::Review, attempt)).expect("review retry");
    }
    for (role, attempt) in [
        (ProviderRole::Child, 1),
        (ProviderRole::Child, 4),
        (ProviderRole::Review, 4),
    ] {
        assert!(rejected(&view, retry(role, attempt)), "{role:?} {attempt}");
    }
    assert_eq!(view.runs[0].provider_retries.len(), 3);
    let mut mixed = view.clone();
    mixed.runs[0].provider_retries.clear();
    for attempt in 1..=3 {
        apply(&mut mixed, retry(ProviderRole::Review, attempt)).expect("review retry");
    }
    mixed.runs[0].provider_retries[0].1.role = ProviderRole::Child;
    assert!(
        rejected(&mixed, retry(ProviderRole::Review, 4)),
        "a forged mix still shares the call's bound"
    );
    // The eventual call record must be the review the retries named.
    assert!(rejected(
        &view,
        child_record(run, child, ProviderCallDisposition::ToolRequested)
    ));
}

#[test]
fn child_spawn_grant_follows_the_run_tool_policy() {
    let session_with_tools = |tools| {
        let mut view = delegated_session(0);
        view.runs[0].config = Some(config(
            CollaborationPolicy::Auto {
                max_active_children: 1,
            },
            tools,
        ));
        view
    };
    use crate::tools::UnguardedGrant::{Denied, HumanOnly};
    for parent in [Denied, HumanOnly] {
        for requested in [Denied, HumanOnly] {
            let mut view = session_with_tools(true);
            view.runs[0]
                .config
                .as_mut()
                .unwrap()
                .tool_policy
                .as_mut()
                .unwrap()
                .child_unguarded = parent;
            let started = Event::RunStarted {
                run_id: view.runs[0].id,
                config: view.runs[0].config.clone().unwrap(),
                active_deadline_ms: Some(crate::session::MAX_GOAL_ACTIVE_MS),
            };
            assert_eq!(started.validate().is_ok(), parent == Denied);
            if parent != Denied {
                continue;
            }
            let result = apply_child_spawn(
                &mut view,
                ChildGrant::ReadOnly {
                    unguarded: requested,
                },
            );
            assert_eq!(result.is_ok(), requested == Denied);
        }
    }
    let mut no_tools = session_with_tools(false);
    assert!(apply_child_spawn(&mut no_tools, ChildGrant::NoTools).is_ok());
    assert_eq!(no_tools.runs[0].agents[1].grant, Some(ChildGrant::NoTools));
    assert!(
        apply_child_spawn(
            &mut session_with_tools(false),
            ChildGrant::ReadOnly {
                unguarded: crate::tools::UnguardedGrant::Denied
            }
        )
        .is_err()
    );

    let mut with_tools = session_with_tools(true);
    assert!(
        apply_child_spawn(
            &mut with_tools,
            ChildGrant::ReadOnly {
                unguarded: crate::tools::UnguardedGrant::Denied
            }
        )
        .is_ok()
    );
    assert_eq!(
        with_tools.runs[0].agents[1].grant,
        Some(ChildGrant::ReadOnly {
            unguarded: crate::tools::UnguardedGrant::Denied
        })
    );
    assert!(apply_child_spawn(&mut session_with_tools(true), ChildGrant::NoTools).is_err());
}

#[test]
fn researcher_refusal_continuations_reject_effects_and_exhausted_reservations() {
    use crate::session::{ProposalRefusal, ProposalRefusalCause};
    for attempts in [3, 8] {
        let (mut view, goal, run, child) = research_session();
        for index in 0..attempts {
            let mut refusal = record(
                AgentPhase::ChildWork,
                ProviderCallDisposition::Rejected,
                Some(0),
            );
            refusal.failure_reason = Some(crate::provider::ProviderFailureReason::ProposalRefused);
            refusal.refusal = Some(ProposalRefusal {
                operation: if attempts == 8 && index % 2 == 1 {
                    "arany_list"
                } else {
                    "arany_read"
                }
                .into(),
                cause: ProposalRefusalCause::InvalidArguments,
            });
            apply(&mut view, reserve_child_call(goal, run, child)).unwrap();
            let mut widened = refusal.clone();
            widened.refusal.as_mut().unwrap().cause = ProposalRefusalCause::InvalidPath;
            assert!(rejected(
                &view,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: child,
                    record: widened,
                }
            ));
            apply(
                &mut view,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: child,
                    record: refusal,
                },
            )
            .unwrap();
            assert!(rejected(
                &view,
                Event::ToolStarted {
                    run_id: run,
                    agent_run_id: child,
                    active_ceiling_ms: 0,
                    verdict: None,
                    intent: child_intent(
                        run,
                        child,
                        ToolCall::Read {
                            path: "src/lib.rs".into(),
                            offset: None,
                            limit: None,
                        }
                    )
                    .into(),
                }
            ));
        }
        assert!(rejected(&view, reserve_child_call(goal, run, child)));
        view.goals[0].unobserved_provider_calls.push(child);
        assert!(rejected(
            &view,
            child_record(run, child, ProviderCallDisposition::Finished)
        ));
    }
}

#[test]
fn primary_refusal_continuations_validate_phase_and_retry_limits() {
    use crate::session::{ProposalRefusal, ProposalRefusalCause};
    let same = ProposalRefusal {
        operation: "arany_write".into(),
        cause: ProposalRefusalCause::PlanInvalid,
    };
    for (middle, attempts, limited) in [
        (same.clone(), 3, true),
        (
            ProposalRefusal {
                operation: "arany_read".into(),
                ..same.clone()
            },
            3,
            false,
        ),
        (
            ProposalRefusal {
                cause: ProposalRefusalCause::InvalidArguments,
                ..same.clone()
            },
            3,
            false,
        ),
        (
            ProposalRefusal {
                cause: ProposalRefusalCause::InvalidArguments,
                ..same.clone()
            },
            8,
            true,
        ),
    ] {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        for index in 0..attempts {
            let refused = if index % 2 == 0 {
                same.clone()
            } else {
                middle.clone()
            };
            let mut refusal = record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Rejected,
                Some(0),
            );
            refusal.failure_reason = Some(crate::provider::ProviderFailureReason::ProposalRefused);
            refusal.refusal = Some(refused);
            apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
            if index > 0 {
                let mut wrong_phase = refusal.clone();
                wrong_phase.phase = AgentPhase::RootSynthesis;
                assert!(rejected(
                    &view,
                    Event::ProviderCallRecorded {
                        run_id: run,
                        agent_run_id: agent,
                        record: wrong_phase
                    }
                ));
            }
            apply(
                &mut view,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: agent,
                    record: refusal,
                },
            )
            .unwrap();
            assert!(rejected(
                &view,
                Event::ToolStarted {
                    verdict: None,
                    run_id: run,
                    agent_run_id: agent,
                    active_ceiling_ms: 0,
                    intent: read_intent(run, agent, "src/main.rs").into()
                }
            ));
        }
        assert_eq!(
            rejected(&view, reserve_child_call(goal, run, agent)),
            limited
        );
        if limited {
            let mut forged = view.clone();
            forged.goals[0].unobserved_provider_calls.push(agent);
            assert!(rejected(
                &forged,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: agent,
                    record: record(
                        AgentPhase::RootPlan,
                        ProviderCallDisposition::Finished,
                        Some(0)
                    ),
                }
            ));
            apply(
                &mut view,
                Event::AgentFinished {
                    worker: None,
                    run_id: run,
                    agent_run_id: agent,
                    disposition: AgentDisposition::Failed,
                    summary: None,
                    result: None,
                },
            )
            .unwrap();
            view.goals[0].active_deadline = Some((run, None));
            apply(
                &mut view,
                Event::RunFinished {
                    run_id: run,
                    disposition: RunDisposition::Failed,
                    failure: Some(crate::session::RunFailure::RefusalLimit),
                    active_ms: 0,
                },
            )
            .unwrap();
        }
    }
}

#[test]
fn root_synthesis_after_refusal_or_plan_requires_settled_children() {
    for planned in [false, true] {
        for settled in [false, true] {
            let session = SessionId::new();
            let (goal, run, primary, child) = (
                GoalId::new(),
                RunId::new(),
                AgentRunId::new(),
                AgentRunId::new(),
            );
            let mut events = vec![
                Event::SessionStarted {
                    title: "Synthesis".into(),
                    workspace_identity: WorkspaceIdentity {
                        device: 1,
                        inode: 2,
                    },
                },
                goal_started(goal, GoalBudget::default()),
            ];
            events.extend(start_run(
                goal,
                run,
                primary,
                config(
                    CollaborationPolicy::Auto {
                        max_active_children: 1,
                    },
                    true,
                ),
            ));
            events.push(reserve_child_call(goal, run, primary));
            let mut first = record(
                AgentPhase::RootPlan,
                if planned {
                    ProviderCallDisposition::PlanRecorded
                } else {
                    ProviderCallDisposition::Rejected
                },
                Some(0),
            );
            if !planned {
                first.failure_reason =
                    Some(crate::provider::ProviderFailureReason::ProposalRefused);
                first.refusal = Some(crate::session::ProposalRefusal {
                    operation: "arany_plan".into(),
                    cause: crate::session::ProposalRefusalCause::PlanInvalid,
                });
            }
            events.push(Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: primary,
                record: first,
            });
            if planned {
                events.push(Event::GoalPlanContractRecorded {
                    goal_id: goal,
                    run_id: run,
                    contract: delegated_session(0).goals[0].plan_contract.clone().unwrap(),
                });
            }
            events.push(reserve_child_call(goal, run, primary));
            events.push(Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: primary,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Delegated,
                    Some(0),
                ),
            });
            events.push(Event::AgentSpawned {
                profile: None,
                assignment: None,
                definition: None,
                run_id: run,
                agent_run_id: child,
                role: AgentRole::Child,
                ordinal: 1,
                objective: Some("Check the child result".into()),
                grant: Some(ChildGrant::ReadOnly {
                    unguarded: crate::tools::UnguardedGrant::Denied,
                }),
            });
            if settled {
                events.push(reserve_child_call(goal, run, child));
                events.push(child_record(run, child, ProviderCallDisposition::Finished));
                events.push(Event::AgentFinished {
                    worker: None,
                    run_id: run,
                    agent_run_id: child,
                    disposition: AgentDisposition::Finished,
                    summary: Some("checked".into()),
                    result: Some("checked".into()),
                });
            }
            SessionView::replay_open(session, &journal(session, events.clone()))
                .unwrap()
                .unwrap();
            events.push(reserve_child_call(goal, run, primary));
            events.push(Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: primary,
                record: record(
                    AgentPhase::RootSynthesis,
                    ProviderCallDisposition::Finished,
                    Some(0),
                ),
            });
            assert_eq!(
                SessionView::replay_open(session, &journal(session, events)).is_ok(),
                settled,
                "planned={planned}, settled={settled}"
            );
        }
    }
}

#[test]
fn researcher_tool_request_count_is_bounded() {
    let (mut view, goal, run, child) = research_session();
    apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
    for requested_tools in 0..=crate::session::MAX_STEP_TOOL_CALLS + 1 {
        let event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: child,
            record: ProviderCallRecord {
                requested_tools,
                ..record(
                    AgentPhase::ChildWork,
                    ProviderCallDisposition::ToolRequested,
                    Some(0),
                )
            },
        };
        let mut candidate = view.clone();
        assert_eq!(
            apply(&mut candidate, event).is_ok(),
            (1..=crate::session::MAX_STEP_TOOL_CALLS).contains(&requested_tools),
            "researcher request count {requested_tools}"
        );
    }
}
