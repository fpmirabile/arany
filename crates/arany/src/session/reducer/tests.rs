use super::test_support::*;
use super::*;
use crate::session::{
    ProviderSetupDisposition, ProviderSetupTarget, ProviderSetupTargetKind, WorkspaceIdentity,
};

mod goal_recovery_tests {
    use super::*;
    use crate::tools::ToolDisposition;

    #[test]
    fn setup_recovery_preserves_unknown_tool_outcomes() {
        for disposition in [None, Some(ToolDisposition::Uncertain)] {
            let mut view = pending_plan_view(disposition);
            view.runs[0].status = RunStatus::Active;
            view.goals[0].status = GoalStatus::Running;
            view.goals[0].active_run = Some(view.runs[0].id);
            let operation_id = uuid::Uuid::now_v7();
            view.apply_committed(&EventEnvelope {
                sequence: view.last_sequence + 1,
                session_id: view.id,
                run_id: None,
                agent_run_id: None,
                event: Event::ProviderSetupEditStarted {
                    operation_id,
                    target: ProviderSetupTarget {
                        kind: ProviderSetupTargetKind::CustomProfile,
                        identity: "example-route".into(),
                    },
                    expected_digest: None,
                },
                created_at_ms: 1,
            })
            .unwrap();
            assert_eq!(view.pending_provider_setup_edit, Some(operation_id));
            assert_eq!(view.runs[0].status, RunStatus::Interrupted);
            assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
            assert!(!view.goals[0].can_resume(&view));
            assert_eq!(
                view.runs[0].tools[0]
                    .observation
                    .as_ref()
                    .map(|value| value.disposition),
                disposition
            );
        }
    }
}

mod goal_replay_tests {
    use super::*;
    use crate::provider::{AgentPhase, OutputTokenBound};
    use crate::session::events::{ChildGrant, CollaborationPolicy};
    use crate::tools::{CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation};

    async fn seed_setup_crash_prefix(
        root: &crate::StateRoot,
        workspace: &std::path::Path,
        prefix: usize,
    ) -> SessionId {
        let identity =
            crate::session::input::WorkspaceInputs::admit_identity(workspace, root).unwrap();
        let session = SessionId::new();
        let goal = GoalId::new();
        let mut config = config(CollaborationPolicy::Single, false);
        config.workspace_device = identity.device;
        config.workspace_inode = identity.inode;
        let mut facts = vec![goal_started(goal, GoalBudget::default())];
        facts.extend(
            start_run(goal, RunId::new(), AgentRunId::new(), config)
                .into_iter()
                .take(prefix),
        );
        let mut events = session_with(session, facts);
        for envelope in &mut events {
            match &mut envelope.event {
                Event::SessionStarted {
                    workspace_identity, ..
                }
                | Event::GoalStarted {
                    workspace_identity, ..
                } => *workspace_identity = identity,
                _ => {}
            }
        }
        let store = crate::Store::open(root.try_clone().unwrap()).unwrap();
        for envelope in &events {
            store.append(session, envelope.event.clone()).await.unwrap();
        }
        store.close().await.unwrap();
        session
    }

    #[tokio::test]
    async fn setup_edit_recovers_crash_prefixes_under_the_session_lock() {
        for prefix in 0..=4 {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let root = crate::StateRoot::admit(&temp.path().join("state")).unwrap();
            let session = seed_setup_crash_prefix(&root, &workspace, prefix).await;
            let target = ProviderSetupTarget {
                kind: ProviderSetupTargetKind::CustomProfile,
                identity: "example-route".into(),
            };
            let mut lock = root.lock_session(session).unwrap();
            lock.keep();
            assert!(matches!(
                crate::start_provider_setup_edit(
                    root.try_clone().unwrap(),
                    workspace.clone(),
                    session,
                    target.clone(),
                    None
                )
                .await,
                Err(crate::EngineError::SessionBusy)
            ));
            drop(lock);
            let operation_id = crate::start_provider_setup_edit(
                root.try_clone().unwrap(),
                workspace,
                session,
                target,
                None,
            )
            .await
            .unwrap();
            let store = crate::Store::open(root).unwrap();
            let view = store.load_view(session).await.unwrap().unwrap();
            assert_eq!(view.pending_provider_setup_edit, Some(operation_id));
            assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
            assert_eq!(view.goals[0].active_run, None);
            assert!(
                view.runs
                    .iter()
                    .all(|run| run.status == RunStatus::Interrupted)
            );
            assert_eq!(view.defaults, SessionDefaults::default());
            store
                .append(
                    session,
                    Event::ProviderSetupEditFinished {
                        operation_id,
                        resulting_digest: Some([1; 32]),
                        disposition: ProviderSetupDisposition::Applied,
                    },
                )
                .await
                .unwrap();
            let events = store.load_session(session).await.unwrap();
            let open = SessionView::replay_open(session, &events).unwrap().unwrap();
            assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
            assert!(
                open.runs
                    .iter()
                    .all(|run| run.status == RunStatus::Interrupted)
            );
            assert_eq!(open.pending_provider_setup_edit, None);
            store.close().await.unwrap();
        }
    }

    #[test]
    fn run_started_host_epochs_follow_live_runs_and_resume_boundaries() {
        let old = uuid::Uuid::now_v7();
        let newer = uuid::Uuid::now_v7();
        for (name, previous, resumed, next, accepted) in [
            ("consecutive", vec![old], false, old, true),
            ("resumed", vec![old], true, old, false),
            ("older", vec![old, newer], false, old, false),
            ("new", vec![old], false, newer, true),
            ("new-after-resume", vec![old], true, newer, true),
        ] {
            let (session, goal) = (SessionId::new(), GoalId::new());
            let mut events = vec![goal_started(
                goal,
                GoalBudget {
                    max_runs: 8,
                    ..Default::default()
                },
            )];
            for epoch in previous {
                let (run, agent) = (RunId::new(), AgentRunId::new());
                let mut config = config(CollaborationPolicy::Single, true);
                config.tool_policy.as_mut().unwrap().host_epoch = Some(epoch);
                events.extend(start_run(goal, run, agent, config));
                events.extend(cancel_run(run, agent));
            }
            if resumed {
                events.push(Event::GoalResumed { goal_id: goal });
            }
            let mut config = config(CollaborationPolicy::Single, true);
            config.tool_policy.as_mut().unwrap().host_epoch = Some(next);
            let run = RunId::new();
            let mut started = start_run(goal, run, AgentRunId::new(), config);
            started.truncate(3);
            let event = started.pop().unwrap();
            assert!(matches!(event, Event::RunStarted { .. }));
            events.extend(started);
            let envelopes = session_with(session, events);
            let mut view = SessionView::replay_open(session, &envelopes)
                .unwrap()
                .unwrap();
            assert!(view.runs.last().unwrap().config.is_none());
            let boundary = envelopes
                .iter()
                .find(|envelope| {
                    if resumed {
                        matches!(envelope.event, Event::GoalResumed { .. })
                    } else {
                        matches!(envelope.event, Event::SessionStarted { .. })
                    }
                })
                .unwrap();
            assert_eq!(view.host_epoch_floor, boundary.sequence, "{name}");
            assert_eq!(
                view.runs[0].accepted_sequence > view.host_epoch_floor,
                !resumed,
                "{name}"
            );
            let sequence = view.last_sequence + 1;
            let (run_id, agent_run_id) = event.scope();
            let result = view.apply_committed(&EventEnvelope {
                session_id: session,
                sequence,
                run_id,
                agent_run_id,
                event,
                created_at_ms: 0,
            });
            assert_eq!(result.is_ok(), accepted, "{name}: {result:?}");
        }
    }

    #[test]
    fn run_failure_is_present_exactly_on_failed_terminal_events() {
        use crate::session::RunFailure;
        let session = SessionId::new();
        let goal = GoalId::new();
        let run = RunId::new();
        let agent = AgentRunId::new();
        for (disposition, failure, accepted) in [
            (
                RunDisposition::Failed,
                Some(RunFailure::ProposalRefused),
                true,
            ),
            (
                RunDisposition::Failed,
                Some(RunFailure::RefusalLimit),
                false,
            ),
            (RunDisposition::Failed, None, false),
            (RunDisposition::Cancelled, None, true),
            (
                RunDisposition::Cancelled,
                Some(RunFailure::ProviderCall),
                false,
            ),
            (
                RunDisposition::Finished,
                Some(RunFailure::ProviderCall),
                false,
            ),
            (
                RunDisposition::StoppedByHook,
                Some(RunFailure::ProviderCall),
                false,
            ),
        ] {
            let mut events = vec![goal_started(goal, GoalBudget::default())];
            events.extend(start_run(
                goal,
                run,
                agent,
                config(CollaborationPolicy::Single, false),
            ));
            events.push(Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: agent,
                active_ceiling_ms: 0,
            });
            events.push(Event::AgentFinished {
                worker: None,
                run_id: run,
                agent_run_id: agent,
                disposition: if disposition == RunDisposition::Failed {
                    AgentDisposition::Failed
                } else {
                    AgentDisposition::Cancelled
                },
                summary: None,
                result: None,
            });
            events.push(Event::RunFinished {
                run_id: run,
                disposition,
                failure,
                active_ms: 0,
            });
            let result = SessionView::replay(session, &session_with(session, events));
            assert_eq!(result.is_ok(), accepted, "{disposition:?} {failure:?}");
            if accepted {
                let view = result.unwrap().unwrap();
                assert_eq!(view.runs[0].failure, failure);
                assert_eq!(view.usage.pending, [(session, run, Some(agent))]);
                assert!(view.usage.groups.is_empty());
            }
        }
    }

    #[test]
    fn run_output_caps_replay_inside_the_declared_range_only() {
        let session = SessionId::new();
        let (goal, run, agent) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let ceiling = crate::provider::MAX_OUTPUT_TOKEN_CAP;
        for (cap, accepted) in [
            (4096, true),
            (16_384, true),
            (ceiling, true),
            (ceiling + 1, false),
            (0, false),
        ] {
            let mut config = config(CollaborationPolicy::Single, false);
            config.output_token_cap = cap;
            let mut events = vec![goal_started(goal, GoalBudget::default())];
            events.extend(start_run(goal, run, agent, config));
            let result = SessionView::replay_open(session, &session_with(session, events));
            assert_eq!(result.is_ok(), accepted, "cap {cap}");
        }
    }

    #[test]
    fn role_chains_are_canonical_and_tool_history_stays_in_one_adapter_and_account() {
        use crate::provider::{Provenance, Providers};
        let mut config = config(CollaborationPolicy::Single, true);
        let primary = config.identity().unwrap();
        config.providers = Some(Providers::all(vec![primary.clone()]));
        assert!(config.validate().is_err());
        for child in [false, true] {
            for account_kind in 0..3 {
                let mut alternate = primary.clone();
                alternate.model = "alternate".into();
                if account_kind == 0 {
                    alternate.profile = "anthropic".into();
                } else if account_kind == 1 {
                    alternate.provenance = Provenance::Native {
                        saved_api_account_id: Some(uuid::Uuid::now_v7()),
                    };
                } else {
                    alternate.profile = "chatgpt".into();
                    alternate.effort = Some(crate::provider::Effort::High);
                    alternate.limits.output_token_bound = OutputTokenBound::LocalAcceptanceOnly;
                    alternate.provenance =
                        Provenance::ChatGpt(crate::provider::ChatGptProvenance {
                            account_id: uuid::Uuid::now_v7(),
                            evidence_fingerprint: [1; 32],
                            admission: crate::provider::ChatGptAdmission::AccountConsent,
                        });
                }
                let mut routes = Providers::all(vec![primary.clone()]);
                if child {
                    routes.child.push(alternate);
                } else {
                    routes.primary.push(alternate);
                }
                config.providers = Some(routes);
                assert!(config.validate().is_err());
                let mut no_tools = config.clone();
                no_tools.tool_policy = None;
                no_tools.supervisor = None;
                assert!(no_tools.validate().is_ok());
            }
        }
    }

    #[test]
    fn roster_resume_and_unused_candidates_cannot_widen_pinned_routing() {
        use crate::provider::{NamedProviderRoutes, Providers, RosterSnapshot};
        let mut pinned = config(CollaborationPolicy::Single, true);
        let identity = pinned.identity().unwrap();
        let mut snapshot = RosterSnapshot {
            preset: "coding".into(),
            selection_digest: [0; 32],
            profiles: vec![NamedProviderRoutes {
                name: "main".into(),
                routes: vec![identity.clone()],
            }],
            providers: Providers::all("main".into()),
            child_profiles: vec!["main".into()],
        };
        snapshot.selection_digest = snapshot.digest().unwrap();
        pinned.roster = Some(Box::new(snapshot));
        assert!(pinned.validate().is_ok());
        for custom in [false, true] {
            let mut invalid = pinned.clone();
            let snapshot = invalid.roster.as_mut().unwrap();
            let mut alternate = identity.clone();
            if custom {
                alternate.profile = "custom:exact".into();
                alternate.provenance =
                    crate::provider::Provenance::Custom(crate::provider::CustomProfileProvenance {
                        endpoint: "https://example.com/responses".into(),
                        evidence_digest: [1; 32],
                        capability_evidence_version: 2,
                    });
            } else {
                alternate.profile = "anthropic".into();
            }
            snapshot.profiles.push(NamedProviderRoutes {
                name: "spare".into(),
                routes: if custom {
                    vec![alternate]
                } else {
                    vec![identity.clone(), alternate]
                },
            });
            snapshot.child_profiles.push("spare".into());
            snapshot.selection_digest = snapshot.digest().unwrap();
            assert!(invalid.validate().is_err());
            invalid.tool_policy = None;
            invalid.supervisor = None;
            assert!(invalid.validate().is_ok());
        }
        let (session, goal, run, actor) = (
            SessionId::new(),
            GoalId::new(),
            RunId::new(),
            AgentRunId::new(),
        );
        let mut prefix = vec![goal_started(goal, GoalBudget::default())];
        prefix.extend(start_run(goal, run, actor, pinned.clone()));
        prefix.extend(cancel_run(run, actor));
        let paused = SessionView::replay_open(session, &session_with(session, prefix.clone()))
            .unwrap()
            .unwrap();
        for drifted in [false, true] {
            let mut compacted = prefix.clone();
            compacted.push(Event::GoalCompactionCallReserved {
                goal_id: goal,
                run_id: run,
                active_ceiling_ms: 0,
            });
            compacted.push(Event::ContextCompacted {
                active_ms: Some(0),
                record: crate::session::events::CompactionRecord {
                    rate_limits: None,
                    covered_run_id: run,
                    covered_sequence: paused.runs[0].finished_sequence.unwrap(),
                    source_digest: crate::session::lineage::prefix_digest(
                        &session_with(session, prefix.clone()),
                        paused.runs[0].finished_sequence.unwrap(),
                    )
                    .unwrap(),
                    recent_files: Vec::new(),
                    provider: "openai".into(),
                    model: if drifted { "different" } else { "gpt-5.4" }.into(),
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
            });
            let replayed = SessionView::replay_open(session, &session_with(session, compacted));
            assert_eq!(
                replayed.is_ok(),
                !drifted,
                "compaction drift {drifted}: {:?}",
                replayed.as_ref().err()
            );
        }
        prefix.push(Event::GoalResumed { goal_id: goal });
        for change in 0..3 {
            let mut next = pinned.clone();
            match change {
                1 => next.roster = None,
                2 => {
                    let roster = next.roster.as_mut().unwrap();
                    roster.preset = "drifted".into();
                    roster.selection_digest = roster.digest().unwrap();
                }
                _ => (),
            }
            for prestart in [false, true] {
                let mut events = prefix.clone();
                if prestart {
                    events.extend(
                        start_run(goal, RunId::new(), AgentRunId::new(), pinned.clone())
                            .into_iter()
                            .take(2),
                    );
                    events.push(Event::GoalResumed { goal_id: goal });
                }
                events.extend(start_run(
                    goal,
                    RunId::new(),
                    AgentRunId::new(),
                    next.clone(),
                ));
                assert_eq!(
                    SessionView::replay_open(session, &session_with(session, events)).is_ok(),
                    change == 0
                );
            }
        }
    }

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
                        } if *agent_run_id == primary && change == 3 => {
                            *profile = Some("child".into())
                        }
                        Event::ProviderFallbackUsed { fallback, .. } if change == 4 => {
                            fallback.to = default.clone()
                        }
                        Event::ProviderCallRecorded {
                            agent_run_id,
                            record,
                            ..
                        } if *agent_run_id == child && change == 5 => {
                            record.output_tokens = Some(17)
                        }
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
        use crate::provider::{
            MAX_PROVIDER_RETRY_DELAY_MS, ProviderRetry, ProviderRole, RetryCause,
        };
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
    fn a_forks_boundary_compaction_is_refused_while_its_own_run_is_open() {
        use crate::session::events::{CompactionFailure, CompactionStatus};
        let (fork, source, source_run, run, agent) = (
            SessionId::new(),
            SessionId::new(),
            RunId::new(),
            RunId::new(),
            AgentRunId::new(),
        );
        let mut events = vec![Event::SessionForked {
            title: "Fork".into(),
            source_session_id: source,
            source_run_id: source_run,
            source_sequence: 1,
            prefix_digest: [0; 32],
        }];
        let goal = GoalId::new();
        events.push(goal_started(goal, GoalBudget::default()));
        events.extend(start_run(
            goal,
            run,
            agent,
            config(CollaborationPolicy::Single, false),
        ));
        let open = journal(fork, events.clone());
        assert!(SessionView::replay_open(fork, &open).unwrap().is_some());
        events.push(Event::ContextCompacted {
            record: crate::session::events::CompactionRecord {
                rate_limits: None,
                covered_run_id: source_run,
                covered_sequence: 1,
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
                status: CompactionStatus::Failed {
                    reason: CompactionFailure::ProviderUnavailable,
                },
            },
            active_ms: None,
        });
        assert!(SessionView::replay_open(fork, &journal(fork, events)).is_err());
    }

    #[test]
    fn steering_note_is_accepted_only_inside_an_open_primary_reservation() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let started = || {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(CollaborationPolicy::Single, false),
            ));
            events
        };
        let open = || {
            let mut events = started();
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
                active_ceiling_ms: 0,
            });
            events
        };
        let note = |goal_id: GoalId, text: &str| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: text.into(),
        };
        let accepts = |events: Vec<Event>| {
            SessionView::replay_open(session, &session_with(session, events)).is_ok()
        };

        let mut accepted = open();
        accepted.push(note(goal_id, "use the smaller fixture"));
        let view = SessionView::replay_open(session, &session_with(session, accepted.clone()))
            .expect("a note inside the open reservation replays")
            .expect("Session exists");
        assert_eq!(view.runs[0].steering_notes, vec!["use the smaller fixture"]);

        // Every rejected case first proves its prefix is accepted, so the rejection is not vacuous.
        let prefix = started();
        assert!(accepts(prefix.clone()));
        let mut before = prefix;
        before.push(note(goal_id, "before the reservation"));
        assert!(!accepts(before));

        let recorded_prefix = {
            let mut events = open();
            events.push(Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Finished,
                    None,
                ),
            });
            events
        };
        assert!(accepts(recorded_prefix.clone()));
        let mut recorded = recorded_prefix;
        recorded.push(note(goal_id, "after the call settled"));
        assert!(!accepts(recorded));

        let child = AgentRunId::new();
        let child_prefix = {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
            events.extend(start_run(
                goal_id,
                run_id,
                agent_run_id,
                config(
                    CollaborationPolicy::Auto {
                        max_active_children: 1,
                    },
                    false,
                ),
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
                    ProviderCallDisposition::Delegated,
                    None,
                ),
            });
            events.push(Event::AgentSpawned {
                profile: None,
                assignment: None,
                definition: None,
                run_id,
                agent_run_id: child,
                role: AgentRole::Child,
                ordinal: 1,
                objective: Some("inspect the parser".into()),
                grant: Some(ChildGrant::NoTools),
            });
            events.push(Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id: child,
                active_ceiling_ms: 0,
            });
            events
        };
        assert!(accepts(child_prefix.clone()));
        let mut child_open = child_prefix;
        child_open.push(note(goal_id, "for the child"));
        assert!(!accepts(child_open));

        let mut full = open();
        full.extend((1..=4).map(|n| note(goal_id, &n.to_string())));
        assert!(accepts(full.clone()));
        full.push(note(
            goal_id,
            &"f".repeat(crate::session::MAX_STEERING_NOTE_BYTES),
        ));
        assert!(!accepts(full), "a Run's notes stay within the byte cap");

        // Each note charges its text plus its label, so exactly the per-Run byte cap is accepted.
        let label = crate::session::MAX_STEERING_LABEL_BYTES;
        let mut exact = open();
        exact.push(note(goal_id, &"a".repeat(4 * 1024 - label)));
        exact.push(note(goal_id, &"b".repeat(4 * 1024 - label)));
        assert!(accepts(exact.clone()));
        let mut oversized = open();
        oversized.push(note(goal_id, &"a".repeat(4 * 1024 - label)));
        oversized.push(note(goal_id, &"b".repeat(4 * 1024 - label + 1)));
        assert!(!accepts(oversized));

        let mut finished_prefix = open();
        finished_prefix.push(Event::ProviderCallRecorded {
            run_id,
            agent_run_id,
            record: record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        finished_prefix.push(Event::AgentFinished {
            worker: None,
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
        });
        assert!(accepts(finished_prefix.clone()));
        let mut between = finished_prefix;
        between.push(note(goal_id, "between AgentFinished and RunFinished"));
        assert!(!accepts(between));

        let mut ended_prefix = open();
        ended_prefix.extend(cancel_run(run_id, agent_run_id));
        assert!(accepts(ended_prefix.clone()));
        let mut finished = ended_prefix;
        finished.push(note(goal_id, "after the Run ended"));
        assert!(!accepts(finished));

        let other_prefix = open();
        assert!(accepts(other_prefix.clone()));
        let mut other = other_prefix;
        other.push(note(GoalId::new(), "another Goal"));
        assert!(!accepts(other));

        // A Tool review reserves under the primary id; its pending Tool observation must reject notes.
        let mut review = SessionView::replay_open(session, &session_with(session, open()))
            .expect("open reservation replays")
            .expect("Session exists");
        let intent = crate::tools::EffectIntent {
            host: None,
            host_command: None,
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id,
            agent_run_id,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call: crate::tools::ToolCall::Read {
                path: "src/main.rs".into(),
                offset: None,
                limit: Some(32),
            },
            limits: crate::tools::ToolLimits::default(),
            use_count: 1,
        };
        review.runs[0].tools.push(ToolView {
            checkpoint: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(0),
            hooks: Vec::new(),
            hook_sequence: None,
            intent,
            observation: None,
            review_call_index: None,
            finished_sequence: None,
        });
        assert!(
            review
                .record_goal_steering_note(&goal_id, &run_id, "during a Tool review")
                .is_err()
        );
    }

    fn child_note_prefix(
        goal_id: GoalId,
        run_id: RunId,
        primary: AgentRunId,
        child: AgentRunId,
    ) -> Vec<Event> {
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            primary,
            config(
                CollaborationPolicy::Auto {
                    max_active_children: 2,
                },
                false,
            ),
        ));
        events.extend([
            Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id: primary,
                active_ceiling_ms: 0,
            },
            Event::ProviderCallRecorded {
                run_id,
                agent_run_id: primary,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::Delegated,
                    None,
                ),
            },
            Event::AgentSpawned {
                profile: None,
                assignment: None,
                definition: None,
                run_id,
                agent_run_id: child,
                role: AgentRole::Child,
                ordinal: 1,
                objective: Some("Inspect facts".into()),
                grant: Some(ChildGrant::NoTools),
            },
        ]);
        events
    }

    #[test]
    fn child_steering_requires_its_open_work_reservation() {
        let (session, goal, run, primary, child) = (
            SessionId::new(),
            GoalId::new(),
            RunId::new(),
            AgentRunId::new(),
            AgentRunId::new(),
        );
        let base = child_note_prefix(goal, run, primary, child);
        let reservation = Event::GoalProviderCallReserved {
            goal_id: goal,
            run_id: run,
            agent_run_id: child,
            active_ceiling_ms: 0,
        };
        let note = Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: run,
            agent_run_id: child,
            text: "literal /grant all".into(),
        };
        let mut open = base.clone();
        open.push(reservation);
        let accepts = |events: Vec<Event>| {
            SessionView::replay_open(session, &session_with(session, events)).is_ok()
        };
        assert!(accepts(open.clone()));
        let mut valid = open.clone();
        valid.push(note.clone());
        let view = SessionView::replay_open(session, &session_with(session, valid))
            .unwrap()
            .unwrap();
        assert!(view.runs[0].steering_notes.is_empty());
        assert_eq!(
            view.runs[0].agents[1].steering_notes,
            ["literal /grant all"]
        );
        assert_eq!(view.runs[0].agents[1].grant, Some(ChildGrant::NoTools));
        let mut settled = open.clone();
        settled.push(Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: child,
            record: record(
                AgentPhase::ChildWork,
                ProviderCallDisposition::Finished,
                None,
            ),
        });
        let mut finished = settled.clone();
        finished.push(Event::AgentFinished {
            worker: None,
            run_id: run,
            agent_run_id: child,
            disposition: AgentDisposition::Finished,
            summary: Some("done".into()),
            result: Some("done".into()),
        });
        for prefix in [base, settled, finished] {
            assert!(accepts(prefix.clone()), "valid rejection prefix");
            let mut rejected = prefix;
            rejected.push(note.clone());
            assert!(!accepts(rejected));
        }
        for invalid in [
            Event::AgentSteeringNoteRecorded {
                goal_id: GoalId::new(),
                run_id: run,
                agent_run_id: child,
                text: "x".into(),
            },
            Event::AgentSteeringNoteRecorded {
                goal_id: goal,
                run_id: RunId::new(),
                agent_run_id: child,
                text: "x".into(),
            },
            Event::AgentSteeringNoteRecorded {
                goal_id: goal,
                run_id: run,
                agent_run_id: primary,
                text: "x".into(),
            },
            Event::AgentSteeringNoteRecorded {
                goal_id: goal,
                run_id: run,
                agent_run_id: AgentRunId::new(),
                text: "x".into(),
            },
        ] {
            assert!(accepts(open.clone()));
            let mut rejected = open.clone();
            rejected.push(invalid);
            assert!(!accepts(rejected));
        }
        let mut capped = open.clone();
        capped.push(Event::AgentSteeringNoteRecorded {
            goal_id: goal,
            run_id: run,
            agent_run_id: child,
            text: "x".repeat(
                crate::session::MAX_STEERING_NOTE_BYTES
                    - crate::session::MAX_CHILD_STEERING_LABEL_BYTES,
            ),
        });
        assert!(accepts(capped.clone()));
        capped.push(note);
        assert!(!accepts(capped));
        let mut prefix = SessionView::replay_open(session, &session_with(session, open))
            .unwrap()
            .unwrap();
        let pending = pending_plan_view(None).runs[0].tools[0].clone();
        prefix.runs[0].tools.push(pending.clone());
        assert!(
            prefix
                .record_agent_steering_note(&goal, &run, &child, "unrelated primary review")
                .is_ok()
        );
        prefix.runs[0].agents[1].tools.push(pending);
        assert!(
            prefix
                .record_agent_steering_note(&goal, &run, &child, "own review")
                .is_err()
        );
        prefix.runs[0].agents[1].tools.pop();
        prefix.runs[0].tools.pop();
        assert!(
            prefix
                .clone()
                .record_agent_steering_note(&goal, &run, &child, "control")
                .is_ok()
        );
        type Mutate = fn(&mut SessionView, RunId, AgentRunId);
        let rejections: [(&str, Mutate); 6] = [
            ("paused goal", |view, _, _| {
                view.goals[0].status = GoalStatus::Interrupted;
            }),
            ("finished goal", |view, _, _| {
                view.goals[0].status = GoalStatus::Completed;
            }),
            ("non-latest run", |view, _, _| {
                let mut later = view.runs[0].clone();
                later.id = RunId::new();
                view.runs.push(later);
            }),
            ("workers stop", |view, _, _| {
                view.runs[0].workers.stop =
                    Some((1, crate::session::worker::WorkerStopReason::Paused));
            }),
            ("hook stop", |view, run, child| {
                let intent = child_intent(
                    run,
                    child,
                    ToolCall::Command {
                        program: "sh".into(),
                        args: Vec::new(),
                        cwd: String::new(),
                    },
                );
                view.runs[0].lifecycle_hooks.push(HookView {
                    sequence: 99,
                    invocation: crate::tools::permissions::HookInvocation {
                        event: crate::tools::permissions::HookEvent::SubagentStop,
                        group: 0,
                        handler: 0,
                        subject_digest: [0; 32],
                        intent: intent.clone(),
                        input_digest: [0; 32],
                        active_ceiling_ms: None,
                    },
                    outcome: Some(crate::tools::permissions::HookOutcome {
                        observation: observation_for(intent, b""),
                        result: crate::tools::permissions::HookResult::Stopped { reason: None },
                        boundary_active_ms: None,
                    }),
                    idle: false,
                });
            }),
            ("finished child", |view, _, _| {
                view.runs[0].agents[1].status = AgentStatus::Finished;
            }),
        ];
        for (name, mutate) in rejections {
            let mut view = prefix.clone();
            mutate(&mut view, run, child);
            assert!(
                view.record_agent_steering_note(&goal, &run, &child, "x")
                    .is_err(),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn child_steering_replays_as_history_without_redelivery() {
        let (session, goal, run, primary, child) = (
            SessionId::new(),
            GoalId::new(),
            RunId::new(),
            AgentRunId::new(),
            AgentRunId::new(),
        );
        let mut prefix = child_note_prefix(goal, run, primary, child);
        prefix.extend([
            Event::GoalProviderCallReserved {
                goal_id: goal,
                run_id: run,
                agent_run_id: child,
                active_ceiling_ms: 0,
            },
            Event::AgentSteeringNoteRecorded {
                goal_id: goal,
                run_id: run,
                agent_run_id: child,
                text: "keep the fixture".into(),
            },
        ]);
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let store = crate::Store::open(crate::StateRoot::admit(&state).unwrap()).unwrap();
        for envelope in session_with(session, prefix) {
            store.append(session, envelope.event).await.unwrap();
        }
        store.close().await.unwrap();
        let store =
            crate::Store::open_read_only(crate::StateRoot::open_existing(&state).unwrap()).unwrap();
        let mut events = store.load_session(session).await.unwrap();
        store.close().await.unwrap();
        let crashed = SessionView::replay(session, &events).unwrap().unwrap();
        assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
        assert_eq!(crashed.runs[0].agents[1].status, AgentStatus::Interrupted);
        assert_eq!(
            crashed.runs[0].agents[1].steering_notes,
            ["keep the fixture"]
        );
        assert!(
            crashed.runs[0]
                .history_user_text()
                .contains("[child 1 / 1] keep the fixture")
        );
        let mut next = vec![Event::GoalResumed { goal_id: goal }];
        next.extend(start_run(
            goal,
            RunId::new(),
            AgentRunId::new(),
            config(CollaborationPolicy::Single, false),
        ));
        let first = events.last().unwrap().sequence + 1;
        for (sequence, event) in (first..).zip(next) {
            let (run_id, agent_run_id) = event.scope();
            events.push(EventEnvelope {
                session_id: session,
                sequence,
                run_id,
                agent_run_id,
                event,
                created_at_ms: 0,
            });
        }
        let resumed = SessionView::replay_open(session, &events).unwrap().unwrap();
        assert!(resumed.runs[1].steering_notes.is_empty());
        assert!(
            resumed.runs[1]
                .agents
                .iter()
                .all(|actor| actor.steering_notes.is_empty())
        );
    }

    #[test]
    fn steering_notes_charge_labels_and_stay_within_the_run_byte_bound() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
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
        let note = |text: &str| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: text.into(),
        };
        // One byte of text plus its label is the densest packing the bound allows.
        let most = crate::session::MAX_STEERING_NOTE_BYTES
            / (1 + crate::session::MAX_STEERING_LABEL_BYTES);
        events.extend((0..most).map(|_| note("n")));
        let view = SessionView::replay_open(session, &session_with(session, events.clone()))
            .expect("the maximum tiny notes replay")
            .expect("Session exists");
        let run = &view.runs[0];
        assert_eq!(run.steering_notes.len(), most);
        let text = run.history_user_text();
        assert!(text.len() - run.objective.len() <= crate::session::MAX_STEERING_CONTEXT_BYTES);
        events.push(note("n"));
        assert!(
            SessionView::replay_open(session, &session_with(session, events)).is_err(),
            "the next tiny note exceeds the Run's bound with its label"
        );
    }

    #[test]
    fn steering_notes_have_no_count_cap_but_keep_the_run_byte_bound() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let open = || {
            let mut events = vec![goal_started(goal_id, GoalBudget::default())];
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
            events
        };
        let note = |len: usize| Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: "n".repeat(len),
        };
        let mut events = open();
        events.extend((0..9).map(|_| note(800)));
        let view = SessionView::replay_open(session, &session_with(session, events.clone()))
            .expect("nine notes within 8 KiB replay")
            .expect("Session exists");
        assert_eq!(view.runs[0].steering_notes.len(), 9);
        // Nine notes of 800 bytes charge 9 x (800 + 8) = 7272; a 912-byte tenth charges 920 and
        // reaches exactly 8192, while 913 bytes would charge 921 and reach 8193.
        events.push(note(912));
        assert!(
            SessionView::replay_open(session, &session_with(session, events.clone())).is_ok(),
            "the Run's notes together reach exactly 8 KiB with labels"
        );
        events.push(note(913));
        assert!(
            SessionView::replay_open(session, &session_with(session, events)).is_err(),
            "one more byte exceeds the Run's bound"
        );
    }

    #[test]
    fn history_user_text_labels_notes_within_the_steering_reserve() {
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let session = SessionId::new();
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
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
        for _ in 0..4 {
            events.push(Event::GoalSteeringNoteRecorded {
                goal_id,
                run_id,
                text: "n".repeat(2000),
            });
        }
        let view = SessionView::replay_open(session, &session_with(session, events))
            .expect("notes replay")
            .expect("Session exists");
        let run = &view.runs[0];
        let text = run.history_user_text();
        assert!(text.starts_with(&run.objective));
        assert!(text.contains("\n[4] nnnn"));
        // The reserve covers every note byte plus its labels and header, so compaction and history agree on size.
        assert!(text.len() - run.objective.len() <= crate::session::MAX_STEERING_CONTEXT_BYTES);
        let mut bare = run.clone();
        bare.steering_notes.clear();
        assert_eq!(bare.history_user_text(), run.objective);
        let mut mixed = bare;
        let primary = mixed.agents[0].clone();
        for ordinal in 1..=8 {
            let mut child = primary.clone();
            child.role = AgentRole::Child;
            child.ordinal = ordinal;
            child.id = AgentRunId::new();
            mixed.agents.push(child);
        }
        let mut next = 0;
        while mixed.steering_bytes() + 1 + crate::session::MAX_CHILD_STEERING_LABEL_BYTES
            <= crate::session::MAX_STEERING_NOTE_BYTES
        {
            if next % 9 == 0 {
                mixed.steering_notes.push("n".into());
            } else {
                mixed.agents[next % 9].steering_notes.push("n".into());
            }
            next += 1;
        }
        assert!(
            mixed.history_user_text().len() - mixed.objective.len()
                <= crate::session::MAX_STEERING_CONTEXT_BYTES
        );
        assert!(mixed.history_user_text().contains("[child 8 /"));
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

    use crate::supervisor::{MAX_VERDICTS, SupervisorState};

    fn supervisor_session() -> (SessionView, Vec<EventEnvelope>, GoalId, RunId, AgentRunId) {
        let session = SessionId::new();
        let (goal, run, actor) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let mut pin = config(CollaborationPolicy::Single, true);
        pin.supervisor = Some(crate::supervisor::SupervisorPolicy::default());
        let mut events = vec![
            Event::SessionStarted {
                title: "Supervisor".into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
            },
            goal_started(
                goal,
                GoalBudget {
                    max_provider_calls: None,
                    ..Default::default()
                },
            ),
        ];
        events.extend(start_run(goal, run, actor, pin));
        let envelopes = journal(session, events);
        let view = SessionView::replay_open(session, &envelopes)
            .unwrap()
            .unwrap();
        (view, envelopes, goal, run, actor)
    }

    fn append_record(view: &mut SessionView, envelopes: &mut Vec<EventEnvelope>, event: Event) {
        apply(view, event.clone()).unwrap();
        let (run_id, agent_run_id) = event.scope();
        envelopes.push(EventEnvelope {
            sequence: view.last_sequence,
            session_id: view.id,
            run_id,
            agent_run_id,
            event,
            created_at_ms: 0,
        });
    }
    #[test]
    fn supervisor_replay_requires_exact_adjacent_verdicts_and_accepts_capped_and_interrupted_prefixes()
     {
        let (mut view, mut envelopes, goal, run, actor) = supervisor_session();
        let mut evaluator = SupervisorState::default();
        for i in 0..260 {
            append_record(
                &mut view,
                &mut envelopes,
                reserve_child_call(goal, run, actor),
            );
            append_record(
                &mut view,
                &mut envelopes,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: actor,
                    record: record(
                        AgentPhase::RootPlan,
                        ProviderCallDisposition::ToolRequested,
                        Some(0),
                    ),
                },
            );
            let intent = read_intent(run, actor, "src/lib.rs");
            append_record(
                &mut view,
                &mut envelopes,
                Event::ToolStarted {
                    verdict: None,
                    run_id: run,
                    agent_run_id: actor,
                    intent: intent.clone().into(),
                    active_ceiling_ms: 0,
                },
            );
            append_record(
                &mut view,
                &mut envelopes,
                Event::ToolFinished {
                    checkpoint: None,
                    run_id: run,
                    agent_run_id: actor,
                    observation: observation_for(intent, b"ok"),
                },
            );
            supervisor_boundary(&mut view, &mut envelopes, goal, i, &mut evaluator);
        }
        assert_eq!(
            view.runs[0].supervisor.verdicts().len(),
            usize::from(MAX_VERDICTS)
        );
        assert_eq!(
            SessionView::replay_open(view.id, &envelopes)
                .unwrap()
                .unwrap(),
            view
        );
        supervisor_corpus_labels(view, envelopes, goal);
    }

    fn supervisor_interrupted_prefix(
        view: &SessionView,
        envelopes: &[EventEnvelope],
        goal: GoalId,
    ) {
        let session = view.id;
        let mut corpus = crate::supervisor::corpus::Corpus::new().unwrap();
        let interrupted = SessionView::replay_for_corpus(session, envelopes, None, &mut corpus)
            .unwrap()
            .unwrap();
        let output = corpus.finish(&interrupted).unwrap();
        let last: serde_json::Value = serde_json::from_str(output.lines().last().unwrap()).unwrap();
        assert_eq!(last["features"]["loop"]["repeat_identical"], 3);
        assert!(last["journaled_level"].is_null());
        let interrupted = SessionView::replay(session, envelopes).unwrap().unwrap();
        assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
        let mut resumed: Vec<_> = envelopes.iter().map(|e| e.event.clone()).collect();
        resumed.push(Event::GoalResumed { goal_id: goal });
        resumed.extend(start_run(
            goal,
            RunId::new(),
            AgentRunId::new(),
            view.runs[0].config.clone().unwrap(),
        ));
        assert!(
            SessionView::replay_open(session, &journal(session, resumed)).is_ok(),
            "a new Run follows an interrupted prefix with a pending verdict"
        );
    }

    fn supervisor_recorded_cases(view: &SessionView, event: &Event) {
        use crate::supervisor::{Features, Level, Response, Site};
        for case in 0..11 {
            let mut forged = event.clone();
            let Event::SupervisorVerdictRecorded {
                run_id,
                agent_run_id,
                verdict,
            } = &mut forged
            else {
                unreachable!()
            };
            match case {
                0 => verdict.subject_sequence += 1,
                1 => verdict.site = Site::Stall,
                2 => verdict.capped = true,
                3 => *agent_run_id = AgentRunId::new(),
                4 => *run_id = RunId::new(),
                5 => verdict.evaluator += 1,
                6 => verdict.signals[0].evidence = vec![0],
                7 => verdict.signals.clear(),
                8 => verdict.signals[0].evidence = vec![1],
                9 => {
                    verdict.signals[0].features = Features::Loop(Default::default());
                    verdict.signals[0].level = Level::WouldNarrow;
                    verdict.signals[0].would_response = Some(Response::Revoke);
                }
                _ => verdict.signals[0].evidence = vec![verdict.subject_sequence; 2],
            }
            assert_eq!(
                apply(&mut view.clone(), forged).is_ok(),
                (8..=9).contains(&case),
                "forgery {case}"
            );
        }
        let mut unpinned = view.clone();
        unpinned.runs[0].config.as_mut().unwrap().supervisor = None;
        assert!(apply(&mut unpinned, event.clone()).is_err());
    }

    fn supervisor_boundary(
        view: &mut SessionView,
        envelopes: &mut Vec<EventEnvelope>,
        goal: GoalId,
        i: usize,
        evaluator: &mut SupervisorState,
    ) {
        let (run, actor) = (view.runs[0].id, view.runs[0].primary().unwrap().id);
        let pending = match evaluator.decide_verdict(view, envelopes.last().unwrap()) {
            crate::supervisor::SupervisorDecision::Continue => None,
            crate::supervisor::SupervisorDecision::Record {
                run_id,
                agent_run_id,
                verdict,
            } => Some(Event::SupervisorVerdictRecorded {
                run_id,
                agent_run_id,
                verdict,
            }),
        };

        if i == 0 {
            let mut forged = view.clone();
            apply(&mut forged, recorded_supervisor_verdict(view, actor)).unwrap();
            assert_eq!(forged.runs[0].supervisor.verdicts().len(), 1);
        }
        if i == 2 {
            assert!(apply(&mut view.clone(), reserve_child_call(goal, run, actor)).is_ok());
            supervisor_interrupted_prefix(view, envelopes, goal);
            let verdict = pending.as_ref().unwrap();
            supervisor_recorded_cases(view, verdict);
            let mut misplaced = view.clone();
            apply(&mut misplaced, reserve_child_call(goal, run, actor)).unwrap();
            assert!(apply(&mut misplaced, verdict.clone()).is_err());
        }
        if let Some(verdict) = pending {
            append_record(view, envelopes, verdict.clone());
            assert_eq!(
                evaluator.decide_verdict(view, envelopes.last().unwrap()),
                crate::supervisor::SupervisorDecision::Continue
            );
            let (recorded_actor, recorded) = view.runs[0].supervisor.verdicts().last().unwrap();
            assert_eq!(*recorded_actor, actor);
            assert!(
                matches!(&verdict, Event::SupervisorVerdictRecorded { verdict, .. } if verdict == recorded)
            );
            assert!(
                apply(&mut view.clone(), verdict).is_err(),
                "duplicate verdict"
            );
        } else if view.runs[0].supervisor.verdicts().len() == usize::from(MAX_VERDICTS) {
            let overflow = recorded_supervisor_verdict(view, actor);
            assert!(apply(&mut view.clone(), overflow).is_err());
        }
    }

    fn supervisor_corpus_labels(
        mut view: SessionView,
        mut envelopes: Vec<EventEnvelope>,
        goal: GoalId,
    ) {
        let (session, run, actor) = (view.id, view.runs[0].id, view.runs[0].primary().unwrap().id);
        append_record(
            &mut view,
            &mut envelopes,
            reserve_child_call(goal, run, actor),
        );
        append_record(
            &mut view,
            &mut envelopes,
            Event::GoalSteeringNoteRecorded {
                goal_id: goal,
                run_id: run,
                text: "private-steering-canary".into(),
            },
        );
        let mut corpus = crate::supervisor::corpus::Corpus::new().unwrap();
        let closed = SessionView::replay_for_corpus(session, &envelopes, None, &mut corpus)
            .unwrap()
            .unwrap();
        let output = corpus.finish(&closed).unwrap();
        let lines: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!output.contains("private-steering-canary"));
        assert!(!output.contains("src/lib.rs"));
        assert!(!output.contains(&session.to_string()));
        assert!(!output.contains(&run.to_string()));
        assert!(!output.contains(&actor.to_string()));
        for (index, line) in lines[1..].iter().enumerate() {
            assert_eq!(line["labels"]["steering"], index >= 256);
            assert_eq!(line["labels"]["pause_or_cancel"], false);
        }
        let mut second = crate::supervisor::corpus::Corpus::new().unwrap();
        let reopened = SessionView::replay_for_corpus(session, &envelopes, None, &mut second)
            .unwrap()
            .unwrap();
        let second = second.finish(&reopened).unwrap();
        let second: Vec<serde_json::Value> = second
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0], second[0]);
        for (first, second) in lines[1..].iter().zip(&second[1..]) {
            assert_ne!(first["salted_call_key"], second["salted_call_key"]);
            assert_ne!(first["salted_outcome_key"], second["salted_outcome_key"]);
            assert_eq!(first["features"], second["features"]);
        }
        for failed in [false, true] {
            let mut terminal_view = view.clone();
            let mut terminal_events = envelopes.to_vec();
            for event in [
                Event::AgentFinished {
                    run_id: run,
                    agent_run_id: actor,
                    disposition: if failed {
                        AgentDisposition::Failed
                    } else {
                        AgentDisposition::Cancelled
                    },
                    summary: None,
                    result: None,
                    worker: None,
                },
                Event::RunFinished {
                    failure: failed.then_some(crate::session::RunFailure::ProviderCall),
                    run_id: run,
                    disposition: if failed {
                        RunDisposition::Failed
                    } else {
                        RunDisposition::Cancelled
                    },
                    active_ms: 0,
                },
            ] {
                append_record(&mut terminal_view, &mut terminal_events, event);
            }
            if failed {
                append_record(
                    &mut terminal_view,
                    &mut terminal_events,
                    Event::GoalFinished {
                        goal_id: goal,
                        status: GoalStatus::Failed,
                        active_ms: 0,
                        exhausted: false,
                        report: GoalReport {
                            summary: "private-failure-canary".into(),
                            changes: Vec::new(),
                            evidence: Vec::new(),
                            unresolved: Vec::new(),
                            cached_input_tokens: 0,
                        },
                        budget_refusal: None,
                    },
                );
            }
            let mut corpus = crate::supervisor::corpus::Corpus::new().unwrap();
            let closed =
                SessionView::replay_for_corpus(session, &terminal_events, None, &mut corpus)
                    .unwrap()
                    .unwrap();
            let output = corpus.finish(&closed).unwrap();
            assert!(!output.contains("private-failure-canary"));
            let records: Vec<serde_json::Value> = output
                .lines()
                .skip(1)
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            for (index, record) in records.iter().enumerate() {
                assert_eq!(record["labels"]["pause_or_cancel"], !failed && index >= 256);
                assert_eq!(record["labels"]["goal_failed"], failed && index >= 256);
            }
        }
        assert_eq!(lines.len(), 261);
        assert_eq!(lines[1]["features"]["loop"]["repeat_identical"], 1);
        assert!(lines[1]["journaled_level"].is_null());
        assert_eq!(lines[3]["journaled_level"], "notice");
        assert_eq!(lines[4]["journaled_level"], "would_narrow");
        assert_eq!(lines[260]["features"]["loop"]["repeat_identical"], 32);
        assert!(lines[260]["journaled_level"].is_null());
        for (index, line) in lines[1..].iter().enumerate() {
            assert_eq!(line["relative_step"], index + 1);
            assert_eq!(line["salted_call_key"], lines[1]["salted_call_key"]);
            assert_eq!(line["salted_outcome_key"], lines[1]["salted_outcome_key"]);
        }
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
                refusal.failure_reason =
                    Some(crate::provider::ProviderFailureReason::ProposalRefused);
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
                refusal.failure_reason =
                    Some(crate::provider::ProviderFailureReason::ProposalRefused);
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
    fn clearing_rejects_nonmonotone_partial_recent_open_and_forged_boundaries() {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        for size in [2, 1, 1, 1, 1] {
            apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
            apply(&mut view, requesting_step(run, agent, size)).unwrap();
            let intents: Vec<_> = (0..size)
                .map(|_| read_intent(run, agent, "src/lib.rs"))
                .collect();
            if size > 1 {
                for intent in &intents {
                    apply(&mut view, tool_proposed(run, agent, intent)).unwrap();
                }
            }
            for intent in &intents {
                apply(&mut view, tool_started(run, agent, intent)).unwrap();
            }
            for intent in &intents {
                apply(
                    &mut view,
                    tool_finished(run, agent, intent, &vec![b'x'; 2048]),
                )
                .unwrap();
            }
        }
        let cleared = |count: u32, bytes: u32| Event::ToolResultsCleared {
            run_id: run,
            covered_tool_count: count,
            cleared_bytes: bytes,
        };
        let savings = |view: &SessionView, start: usize, end: usize| {
            let mut before = 0_usize;
            let mut after = 0_usize;
            for tool in &view.runs[0].tools[start..end] {
                let observation = tool.observation.as_ref().unwrap();
                before += crate::tools::types::model_result(observation, &tool.hook_feedback())
                    .to_string()
                    .len();
                after += crate::tools::types::cleared_model_result(observation)
                    .to_string()
                    .len();
            }
            u32::try_from(before.saturating_sub(after)).unwrap()
        };
        for count in [0, 1, 4, 7] {
            assert!(
                rejected(&view, cleared(count, 0)),
                "invalid clearing boundary {count}"
            );
        }
        let bytes = savings(&view, 0, 2);
        assert!(
            rejected(&view, cleared(2, bytes + 1)),
            "forged byte savings"
        );
        let event = cleared(2, bytes);
        apply(&mut view, event.clone()).expect("whole step leaving at least three recent results");
        assert_eq!(view.runs[0].tool_results_cleared_count, 2);
        assert!(rejected(&view, event), "duplicate boundary");
        assert!(rejected(&view, cleared(1, 0)), "regression");
        let bytes = savings(&view, 2, 3);
        let event = cleared(3, bytes);
        let mut closed = view.clone();
        apply(&mut view, event.clone()).expect("valid active Run clearing boundary");
        closed.runs[0].status = RunStatus::Finished;
        assert!(rejected(&closed, event), "closed Run");
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        assert!(rejected(&view, cleared(4, 0)), "open Provider reservation");
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        let intent = read_intent(run, agent, "src/lib.rs");
        apply(&mut view, tool_started(run, agent, &intent)).unwrap();
        assert!(rejected(&view, cleared(4, 0)), "open Tool");
    }

    #[test]
    fn compaction_before_outcome_correction_requires_the_first_settled_failure() {
        let (mut view, goal, run, agent) = primary_tool_session(false);
        let read = read_intent(run, agent, "src/lib.rs");
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(&mut view, requesting_step(run, agent, 1)).unwrap();
        apply(&mut view, tool_started(run, agent, &read)).unwrap();
        apply(&mut view, tool_finished(run, agent, &read, b"one")).unwrap();
        let reservation = Event::GoalCompactionCallReserved {
            goal_id: goal,
            run_id: run,
            active_ceiling_ms: 0,
        };
        for attempt in 1..=2 {
            apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
            let mut failure = record(
                AgentPhase::RootPlan,
                ProviderCallDisposition::InvalidResponse,
                Some(0),
            );
            failure.failure_reason = Some(crate::provider::ProviderFailureReason::OutcomeContract);
            apply(
                &mut view,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: agent,
                    record: failure,
                },
            )
            .unwrap();
            if attempt == 1 {
                let mut compacting = view.clone();
                apply(&mut compacting, reservation.clone()).expect("compaction before correction");
                assert_eq!(compacting.goals[0].compaction_call_reserved, Some(run));
                assert_eq!(
                    compacting.goals[0].provider_calls,
                    view.goals[0].provider_calls + 1
                );
                assert!(
                    compacting.runs[0]
                        .primary()
                        .unwrap()
                        .outcome_correction_pending()
                );
                let mut unsettled = view.clone();
                unsettled.runs[0].tools[0].observation = None;
                assert!(rejected(&unsettled, reservation.clone()));
            } else {
                assert!(
                    rejected(&view, reservation.clone()),
                    "correction is exhausted"
                );
            }
        }
    }

    fn compacting_session() -> (SessionView, GoalId, RunId) {
        compacting_session_with_last_tool(ToolCall::Read {
            path: "src/lib.rs".into(),
            offset: None,
            limit: Some(1024),
        })
    }

    fn compacting_session_with_last_tool(call: ToolCall) -> (SessionView, GoalId, RunId) {
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

    fn covering_compaction(view: &SessionView, goal: GoalId, run: RunId, count: u32) -> Event {
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

    #[test]
    fn recent_compaction_files_use_newest_committed_workspace_digest() {
        let (mut view, _, _) = compacting_session();
        for (index, digest, call) in [
            (
                0,
                "a".repeat(64),
                crate::tools::ToolCall::Write {
                    path: "src/lib.rs".into(),
                    expected_digest: None,
                    content: "first".into(),
                },
            ),
            (
                1,
                "b".repeat(64),
                crate::tools::ToolCall::Edit {
                    path: "src/lib.rs".into(),
                    expected_digest: "a".repeat(64),
                    old: "first".into(),
                    new: "second".into(),
                },
            ),
        ] {
            let tool = &mut view.runs[0].tools[index];
            let observation = tool.observation.as_mut().unwrap();
            observation.intent.call = call.clone();
            tool.intent.call = call;
            observation.output = serde_json::json!({ "sha256": digest }).to_string();
        }
        let run = &view.runs[0];
        let covered_sequence = run.tools[1].finished_sequence.unwrap();
        let recent_files = crate::session::recent_files(run, covered_sequence);
        assert_eq!(
            recent_files,
            [crate::session::RecentFile {
                path: "src/lib.rs".into(),
                digest: "b".repeat(64),
            }]
        );
        let expected = format!(
            "Files changed before compaction; Read them for current content\n[{{\"path\":\"src/lib.rs\",\"digest\":\"{}\"}}]",
            "b".repeat(64)
        );
        assert_eq!(
            crate::session::render_recent_files(&recent_files).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            crate::session::recent_files(
                run,
                run.tools[0].finished_sequence.unwrap().saturating_sub(1)
            ),
            []
        );
        assert_eq!(
            crate::session::recent_files(run, run.tools[0].finished_sequence.unwrap()),
            [crate::session::RecentFile {
                path: "src/lib.rs".into(),
                digest: "a".repeat(64),
            }]
        );
        let mut earlier_run = run.clone();
        earlier_run.tools = vec![run.tools[0].clone()];
        let mut covered_run = run.clone();
        covered_run.tools = vec![run.tools[1].clone()];
        assert_eq!(
            crate::session::recent_files_across(&[earlier_run, covered_run], covered_sequence),
            [crate::session::RecentFile {
                path: "src/lib.rs".into(),
                digest: "b".repeat(64),
            }]
        );
    }

    fn background_primary_tool_session() -> (SessionView, GoalId, RunId, AgentRunId, AgentRunId) {
        let fixture = super::super::worker_tests::background_fixture();
        let mut view = SessionView::replay_open(fixture.session, &fixture.envelopes())
            .unwrap()
            .unwrap();
        let (goal, run, primary, child) =
            (fixture.goal, fixture.run, fixture.primary, fixture.child);
        let read = read_intent(run, primary, "src/lib.rs");
        apply(&mut view, reserve_child_call(goal, run, primary)).unwrap();
        apply(
            &mut view,
            Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: primary,
                record: ProviderCallRecord {
                    requested_tools: 1,
                    ..record(
                        AgentPhase::RootSynthesis,
                        ProviderCallDisposition::ToolRequested,
                        Some(0),
                    )
                },
            },
        )
        .unwrap();
        apply(&mut view, tool_started(run, primary, &read)).unwrap();
        apply(&mut view, tool_finished(run, primary, &read, b"one")).unwrap();
        (view, goal, run, primary, child)
    }

    #[test]
    fn primary_compaction_keeps_child_calls_independent_and_budgeted() {
        let (mut view, goal, run, primary, child) = background_primary_tool_session();
        apply(
            &mut view,
            Event::GoalCompactionCallReserved {
                goal_id: goal,
                run_id: run,
                active_ceiling_ms: 0,
            },
        )
        .unwrap();
        let primary_call = reserve_child_call(goal, run, primary);
        let child_call = reserve_child_call(goal, run, child);
        assert!(rejected(&view, primary_call.clone()));
        let calls = view.goals[0].provider_calls;
        let mut exhausted = view.clone();
        exhausted.goals[0]
            .plan_contract
            .as_mut()
            .unwrap()
            .budget
            .max_provider_calls = Some(calls);
        assert_eq!(
            exhausted.goals[0].effective_budget().max_provider_calls,
            Some(calls)
        );
        assert!(rejected(&exhausted, child_call.clone()));
        apply(&mut view, child_call.clone()).expect("child continues during primary compaction");
        assert_eq!(view.goals[0].provider_calls, calls + 1);
        assert_eq!(view.goals[0].compaction_call_reserved, Some(run));
        assert_eq!(view.goals[0].unobserved_provider_calls, [child]);
        assert!(rejected(&view, child_call));
        let count = u32::try_from(view.runs[0].tools.len()).unwrap();
        let compacted = covering_compaction(&view, goal, run, count);
        apply(&mut view, compacted).expect("primary compaction settles independently");
        assert_eq!(view.goals[0].compaction_call_reserved, None);
        assert_eq!(view.goals[0].unobserved_provider_calls, [child]);
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::Finished),
        )
        .expect("child call settles independently");
        assert!(view.goals[0].unobserved_provider_calls.is_empty());
        apply(&mut view, primary_call).expect("primary resumes after its compaction");
        assert_eq!(view.goals[0].unobserved_provider_calls, [primary]);
    }

    #[test]
    fn primary_compaction_accepts_only_continuable_worker_deferrals() {
        use crate::provider::ProviderFailureReason;

        for reason in [
            Some(ProviderFailureReason::WorkerAnswerDeferred),
            Some(ProviderFailureReason::WorkerDelegateRefused),
            Some(ProviderFailureReason::WorkerToolRefused),
            None,
        ] {
            let (mut view, goal, run, primary, child) = background_primary_tool_session();
            apply(&mut view, reserve_child_call(goal, run, child)).unwrap();
            apply(&mut view, reserve_child_call(goal, run, primary)).unwrap();
            let mut deferred = record(
                AgentPhase::RootSynthesis,
                ProviderCallDisposition::InvalidResponse,
                Some(0),
            );
            deferred.failure_reason = reason;
            apply(
                &mut view,
                Event::ProviderCallRecorded {
                    run_id: run,
                    agent_run_id: primary,
                    record: deferred,
                },
            )
            .unwrap();
            let reservation = Event::GoalCompactionCallReserved {
                goal_id: goal,
                run_id: run,
                active_ceiling_ms: 0,
            };
            assert_eq!(
                apply(&mut view, reservation).is_ok(),
                reason.is_some(),
                "{reason:?}"
            );
            if reason.is_some() {
                let count = u32::try_from(view.runs[0].tools.len()).unwrap();
                let compacted = covering_compaction(&view, goal, run, count);
                apply(&mut view, compacted).expect("compaction settles after a worker deferral");
                assert_eq!(view.goals[0].compaction_call_reserved, None);
                assert_eq!(view.goals[0].unobserved_provider_calls, [child]);
            }
        }
    }

    #[test]
    fn compaction_prefix_never_splits_a_step() {
        let (view, goal, run) = compacting_session();
        assert!(
            rejected(&view, covering_compaction(&view, goal, run, 1)),
            "a covered prefix inside the two-request step"
        );
        let whole = covering_compaction(&view, goal, run, 2);
        let mut replayed = view.clone();
        apply(&mut replayed, whole).expect("the whole step is covered");
        assert_eq!(replayed.goals[0].compaction_call_reserved, None);
        assert_eq!(
            replayed
                .usage
                .groups
                .iter()
                .map(|group| group.input.unreported)
                .sum::<u64>(),
            view.usage
                .groups
                .iter()
                .map(|group| group.input.unreported)
                .sum::<u64>()
                + 1
        );
        assert!(
            !replayed
                .usage
                .pending
                .iter()
                .any(|(_, id, actor)| *id == run && actor.is_none())
        );
    }

    #[test]
    fn reducer_accepts_maximum_summary_from_recorded_compiler_floor() {
        let (mut view, goal, run) = compacting_session_with_last_tool(ToolCall::Write {
            path: "src/lib.rs".into(),
            expected_digest: None,
            content: "two".into(),
        });
        let Event::GoalContextCompacted { mut record, .. } =
            covering_compaction(&view, goal, run, 2)
        else {
            unreachable!()
        };
        let summary = "S".repeat(crate::session::MAX_COMPACTION_SUMMARY_BYTES);
        record.compiler_version = crate::session::MIN_RECORDED_COMPACTION_COMPILER_VERSION;
        record.recent_files = vec![crate::session::RecentFile {
            path: "src/lib.rs".into(),
            digest: crate::validate::hex_digest(b"two"),
        }];
        record.status = crate::session::events::CompactionStatus::Succeeded {
            content_digest: crate::validate::digest(summary.as_bytes()),
            summary_bytes: summary.len() as u32,
            summary_tokens_estimate: summary.len().div_ceil(4) as u32,
            summary: summary.clone(),
        };
        let mut invalid_record = record.clone();
        invalid_record.recent_files[0].digest = "x".repeat(64);
        assert!(rejected(
            &view,
            Event::GoalContextCompacted {
                goal_id: goal,
                run_id: run,
                covered_tool_count: 2,
                record: invalid_record,
            }
        ));

        apply(
            &mut view,
            Event::GoalContextCompacted {
                goal_id: goal,
                run_id: run,
                covered_tool_count: 2,
                record,
            },
        )
        .expect("recorded compiler floor accepts the current summary byte bound");
        assert_eq!(
            view.runs[0].tool_compaction_summary.as_deref(),
            Some(summary.as_str())
        );
        assert_eq!(
            view.runs[0].tool_compaction_recent_files,
            [crate::session::RecentFile {
                path: "src/lib.rs".into(),
                digest: crate::validate::hex_digest(b"two"),
            }]
        );
        assert_eq!(view.runs[0].tools.len(), 2);
    }

    #[test]
    fn goal_compaction_recent_files_must_match_covered_observations() {
        let (view, goal, run) = compacting_session_with_last_tool(ToolCall::Write {
            path: "src/lib.rs".into(),
            expected_digest: None,
            content: "two".into(),
        });
        let Event::GoalContextCompacted { mut record, .. } =
            covering_compaction(&view, goal, run, 2)
        else {
            unreachable!()
        };
        record.recent_files = crate::session::recent_files(&view.runs[0], record.covered_sequence);
        assert_eq!(record.recent_files.len(), 1);
        for (path, digest) in [
            ("src/other.rs", crate::validate::hex_digest(b"two")),
            ("src/lib.rs", "a".repeat(64)),
        ] {
            let mut forged = record.clone();
            forged.recent_files[0] = crate::session::RecentFile {
                path: path.into(),
                digest,
            };
            assert!(rejected(
                &view,
                Event::GoalContextCompacted {
                    goal_id: goal,
                    run_id: run,
                    covered_tool_count: 2,
                    record: forged,
                }
            ));
        }
        for files in [Vec::new(), record.recent_files.clone()] {
            let mut view = view.clone();
            let mut record = record.clone();
            record.recent_files = files.clone();
            apply(
                &mut view,
                Event::GoalContextCompacted {
                    goal_id: goal,
                    run_id: run,
                    covered_tool_count: 2,
                    record,
                },
            )
            .expect("derived hints and historical omission are accepted");
            assert_eq!(view.runs[0].tool_compaction_recent_files, files);
        }
    }

    #[test]
    fn goal_compaction_snapshot_binds_its_primary_tool_across_child_tool_finishes() {
        let (view, goal, run) = compacting_session();
        let (primary, child) = (view.runs[0].agents[0].id, AgentRunId::new());
        let finished =
            |agent, path| tool_finished(run, agent, &read_intent(run, agent, path), b"x");
        let mut events = journal(
            view.id,
            vec![
                finished(primary, "src/lib.rs"),
                finished(child, "notes/a"),
                finished(primary, "src/main.rs"),
            ],
        );
        let compacted = |digest_through, events: &[EventEnvelope]| {
            let Event::GoalContextCompacted { mut record, .. } =
                covering_compaction(&view, goal, run, 2)
            else {
                unreachable!()
            };
            // The second primary Tool is the Run's third ToolFinished.
            record.covered_sequence = 3;
            record.source_digest =
                crate::session::lineage::prefix_digest(events, digest_through).unwrap();
            EventEnvelope {
                sequence: 4,
                session_id: view.id,
                run_id: Some(run),
                agent_run_id: None,
                event: Event::GoalContextCompacted {
                    goal_id: goal,
                    run_id: run,
                    covered_tool_count: 2,
                    record,
                },
                created_at_ms: 0,
            }
        };
        let valid = compacted(3, &events);
        let stale = compacted(2, &events);
        events.push(valid);
        crate::session::compaction::validate_goal_context_compactions(&events)
            .expect("a child Tool finish does not shift the covered primary Tool");
        events.pop();
        events.push(stale);
        assert!(
            matches!(
                crate::session::compaction::validate_goal_context_compactions(&events),
                Err(ReplayError::InvalidSnapshot)
            ),
            "the digest must cover the journal through the covered Tool"
        );
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

    #[test]
    fn invoked_skill_origin_requires_acceptance_and_observation_before_egress() {
        let (session, goal, run, actor) = (
            SessionId::new(),
            GoalId::new(),
            RunId::new(),
            AgentRunId::new(),
        );
        let mut events = vec![goal_started(goal, GoalBudget::default())];
        let mut opening = start_run(goal, run, actor, config(CollaborationPolicy::Single, true));
        for event in &mut opening {
            if let Event::MessageAccepted { skill, .. } = event {
                *skill = Some("ask-matt".into());
            }
        }
        events.extend(opening);
        let mut view = SessionView::replay_open(session, &session_with(session, events))
            .unwrap()
            .unwrap();
        let intent = child_intent(
            run,
            actor,
            ToolCall::Skill {
                name: "ask-matt".into(),
                resource: None,
            },
        );
        let start = |intent: EffectIntent| Event::ToolStarted {
            verdict: None,
            run_id: run,
            agent_run_id: actor,
            intent: intent.into(),
            active_ceiling_ms: 0,
        };
        assert!(rejected(&view, reserve_child_call(goal, run, actor)));
        for call in [
            ToolCall::Skill {
                name: "other".into(),
                resource: None,
            },
            ToolCall::Skill {
                name: "ask-matt".into(),
                resource: Some("references/a.md".into()),
            },
        ] {
            let mut wrong = intent.clone();
            wrong.call = call;
            assert!(rejected(&view, start(wrong)));
        }
        let mut uninvoked = view.clone();
        uninvoked.runs[0].skill = None;
        assert!(rejected(&uninvoked, start(intent.clone())));
        apply(&mut view, start(intent.clone())).unwrap();
        assert!(rejected(&view, reserve_child_call(goal, run, actor)));
        let mut observation = ToolObservation {
            spill: None,
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            output: "pinned guidance".into(),
            attestation: Some(crate::tools::Attestation::for_site(&intent)),
            approval_cause: None,
            cache_update: CacheUpdate::NotApplicable,
            enforcement_cause: None,
            limit_cause: None,
        };
        let finish = |observation| Event::ToolFinished {
            checkpoint: None,
            run_id: run,
            agent_run_id: actor,
            observation,
        };
        let mut guarded = observation.clone();
        guarded.attestation = Some(crate::tools::Attestation::Guard(
            crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            },
        ));
        assert!(rejected(&view, finish(guarded.clone())));
        guarded.attestation = None;
        guarded.disposition = ToolDisposition::Failed;
        guarded.enforcement_cause = Some(crate::tools::EnforcementCause::GuardBootstrap);
        assert!(rejected(&view, finish(guarded)));
        let mut failed = view.clone();
        observation.disposition = ToolDisposition::Failed;
        apply(&mut failed, finish(observation.clone())).unwrap();
        assert!(rejected(&failed, reserve_child_call(goal, run, actor)));
        observation.disposition = ToolDisposition::Succeeded;
        apply(&mut view, finish(observation)).unwrap();
        assert!(rejected(&view, start(intent.clone())));
        apply(&mut view, reserve_child_call(goal, run, actor)).unwrap();
        assert!(rejected(&view, start(intent)));
        apply(
            &mut view,
            Event::ProviderCallRecorded {
                run_id: run,
                agent_run_id: actor,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::ToolRequested,
                    Some(1),
                ),
            },
        )
        .unwrap();
        let read = child_intent(
            run,
            actor,
            ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: Some(32),
            },
        );
        apply(&mut view, start(read.clone())).unwrap();
        apply(&mut view, finish(observation_for(read, b"Fact"))).unwrap();
        apply(
            &mut view,
            Event::GoalCompactionCallReserved {
                goal_id: goal,
                run_id: run,
                active_ceiling_ms: 0,
            },
        )
        .unwrap();
        let compacted = covering_compaction(&view, goal, run, 2);
        apply(&mut view, compacted).expect("the Skill prelude does not consume a model Tool step");
    }
}

#[test]
fn setup_edit_replay_requires_matching_finish_and_blocks_uncertainty() {
    let session_id = SessionId::new();
    let operation_id = uuid::Uuid::now_v7();
    let envelope = |sequence, event| EventEnvelope {
        sequence,
        session_id,
        run_id: None,
        agent_run_id: None,
        event,
        created_at_ms: 1,
    };
    let started = envelope(
        1,
        Event::SessionStarted {
            title: "Setup".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
    );
    let edit_started = envelope(
        2,
        Event::ProviderSetupEditStarted {
            operation_id,
            target: ProviderSetupTarget {
                kind: ProviderSetupTargetKind::CustomProfile,
                identity: "example-route".into(),
            },
            expected_digest: None,
        },
    );
    let mut view = SessionView::replay_open(session_id, &[started.clone(), edit_started.clone()])
        .expect("valid open setup edit")
        .expect("Session view");
    assert_eq!(view.pending_provider_setup_edit, Some(operation_id));
    assert!(
        view.apply_committed(&envelope(
            3,
            Event::ProviderSetupEditFinished {
                operation_id: uuid::Uuid::now_v7(),
                resulting_digest: Some([1; 32]),
                disposition: ProviderSetupDisposition::Applied,
            },
        ))
        .is_err()
    );
    assert!(
        view.apply_committed(&envelope(
            3,
            Event::ProviderSetupEditFinished {
                operation_id,
                resulting_digest: None,
                disposition: ProviderSetupDisposition::Uncertain,
            },
        ))
        .is_ok()
    );
    assert!(view.provider_setup_uncertain);
    assert!(
        view.apply_committed(&envelope(
            4,
            Event::SessionDefaultChanged {
                defaults: SessionDefaults::default(),
            },
        ))
        .is_err()
    );

    let recovered = SessionView::replay(session_id, &[started, edit_started])
        .expect("incomplete edit remains replayable")
        .expect("recovered Session");
    assert_eq!(recovered.pending_provider_setup_edit, Some(operation_id));
}
