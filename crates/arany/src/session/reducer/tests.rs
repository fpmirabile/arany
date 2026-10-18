use super::*;
use crate::session::WorkspaceIdentity;

mod goal_recovery_tests {
    use super::*;
    use crate::tools::{EffectIntent, ToolCall, ToolDisposition, ToolObservation};

    fn pending_plan_view(disposition: Option<ToolDisposition>) -> SessionView {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let run_id = RunId::new();
        let intent = EffectIntent {
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
                offset: 0,
                limit: 32,
            },
            limits: crate::tools::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let tool = ToolView {
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            intent: intent.clone(),
            observation: disposition.map(|disposition| ToolObservation {
                enforcement_cause: None,
                intent,
                disposition,
                output: if disposition == ToolDisposition::Succeeded {
                    "{}".into()
                } else {
                    String::new()
                },
                attestation: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
            finished_sequence: disposition.map(|_| 2),
        };
        SessionView {
            id: session_id,
            title: "Interrupted Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some(WorkspaceIdentity {
                device: 1,
                inode: 2,
            }),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 1,
            lineage: None,
            runs: vec![RunView {
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
                tool_compaction_covered_count: 0,
                definitions: Default::default(),
                display: Vec::new(),
                steering_notes: Vec::new(),
                lifecycle_hooks: Vec::new(),
                lifecycle_hook_sequence: None,
                provider_sequences: Default::default(),
                provider_fallbacks: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
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
            }],
        }
    }

    #[test]
    fn pending_plan_recovery_accepts_closed_inspection_and_refuses_open_or_uncertain_tools() {
        let closed = pending_plan_view(Some(ToolDisposition::Succeeded));
        assert!(closed.goals[0].can_resume(&closed));

        let mut started_only = closed.clone();
        started_only.runs.clear();
        started_only.goals[0].run_ids.clear();
        started_only.goals[0].awaiting_plan_contract = false;
        assert!(started_only.goals[0].can_resume(&started_only));

        let open = pending_plan_view(None);
        assert!(!open.goals[0].can_resume(&open));

        let uncertain = pending_plan_view(Some(ToolDisposition::Uncertain));
        assert!(!uncertain.goals[0].can_resume(&uncertain));

        // Resume keeps refusing a Tool that stopped the Run, such as a cancelled one.
        let cancelled = pending_plan_view(Some(ToolDisposition::Cancelled));
        assert!(!cancelled.goals[0].can_resume(&cancelled));
    }
}

mod goal_replay_tests {
    use super::*;
    use crate::provider::{AgentPhase, OutputTokenBound};
    use crate::session::events::{ChildGrant, CollaborationPolicy, GoalSuccessCriterion};
    use crate::tools::{
        CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolLimits, ToolObservation,
        ToolPolicyReceipt,
    };

    const GOAL_OBJECTIVE: &str = "Goal";

    fn config(policy: CollaborationPolicy, tools: bool) -> RunConfig {
        RunConfig {
            worker_policy: None,
            agent_definitions: None,
            roster: None,
            providers: None,
            provider: "openai".into(),
            model: "gpt-5.4".into(),
            effort: None,
            custom_profile_provenance: None,
            saved_api_account_id: None,
            chatgpt_provenance: None,
            output_token_bound: OutputTokenBound::ProviderEnforced,
            policy,
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
            tool_policy: tools.then_some(ToolPolicyReceipt {
                permissions_digest: None,
                contract_version: 1,
                config_digest: [1; 32],
                max_context_bytes: crate::tools::types::MAX_TOOL_CONTEXT_BYTES as u32,
                unguarded_rules_digest: None,
            }),
        }
    }

    fn record(
        phase: AgentPhase,
        disposition: ProviderCallDisposition,
        tokens: Option<u32>,
    ) -> ProviderCallRecord {
        ProviderCallRecord {
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
            failure_reason: None,
            requested_tools: u8::from(disposition == ProviderCallDisposition::ToolRequested),
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
        }
    }

    fn journal(session_id: SessionId, mut events: Vec<Event>) -> Vec<EventEnvelope> {
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

    fn goal_started(goal_id: GoalId, budget: GoalBudget) -> Event {
        started_with(goal_id, budget, false, None)
    }

    fn started_with(
        goal_id: GoalId,
        budget: GoalBudget,
        autonomous: bool,
        continues: Option<GoalId>,
    ) -> Event {
        Event::GoalStarted {
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

    fn start_run(
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
                run_id,
                text: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
            },
            Event::RunStarted {
                run_id,
                config,
                active_deadline_ms: Some(crate::session::MAX_GOAL_ACTIVE_MS),
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

    fn complete_run_and_goal(
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
            },
        ]
    }

    fn cancel_run(run_id: RunId, agent_run_id: AgentRunId) -> Vec<Event> {
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
                active_ms: 0,
            },
        ]
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
                    covered_run_id: run,
                    covered_sequence: paused.runs[0].finished_sequence.unwrap(),
                    source_digest: crate::session::lineage::prefix_digest(
                        &session_with(session, prefix.clone()),
                        paused.runs[0].finished_sequence.unwrap(),
                    )
                    .unwrap(),
                    provider: "openai".into(),
                    model: if drifted { "different" } else { "gpt-5.4" }.into(),
                    prompt_version: 1,
                    schema_version: 1,
                    compiler_version: 3,
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
                offset: 0,
                limit: 1024,
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
            intent: child_intent(
                run,
                primary_agent,
                ToolCall::Read {
                    path: "src/main.rs".into(),
                    offset: 0,
                    limit: 1024,
                },
            ),
            observation: None,
            review_call_index: None,
            permission_verdict: None,
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        });
        apply(&mut elsewhere, reserve_child_call(goal, run, child)).expect("review reservation");
        assert!(rejected(&elsewhere, fallback.clone()));
        apply(
            &mut view,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: read.clone(),
                active_ceiling_ms: 0,
            },
        )
        .expect("child Tool starts");
        apply(&mut view, reserve_child_call(goal, run, child)).expect("review reservation");
        apply(&mut view, fallback).expect("child review fallback");
        assert_eq!(view.runs[0].provider_fallbacks.len(), 1);
        apply(
            &mut view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: observation_for(read, b"ok"),
            },
        )
        .expect("child Tool finishes after the review switch");
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
                covered_run_id: source_run,
                covered_sequence: 1,
                source_digest: [0; 32],
                provider: "openai".into(),
                model: "gpt-5.4".into(),
                prompt_version: 1,
                schema_version: 1,
                compiler_version: 3,
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

    fn session_with(session_id: SessionId, events: Vec<Event>) -> Vec<EventEnvelope> {
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
                offset: 0,
                limit: 32,
            },
            limits: crate::tools::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        review.runs[0].tools.push(ToolView {
            permission_verdict: None,
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
    }

    #[test]
    fn crash_after_steering_note_replays_interrupted_and_resume_starts_without_notes() {
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
        events.push(Event::GoalSteeringNoteRecorded {
            goal_id,
            run_id,
            text: "steer".into(),
        });
        // The process dies before the reserved call's outcome commits.
        let crashed = SessionView::replay(session, &session_with(session, events.clone()))
            .expect("crashed journal replays")
            .expect("Session exists");
        assert_eq!(crashed.runs[0].status, RunStatus::Interrupted);
        assert_eq!(crashed.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(crashed.runs[0].steering_notes, vec!["steer"]);

        let resumed_run = RunId::new();
        let resumed_agent = AgentRunId::new();
        events.push(Event::GoalResumed { goal_id });
        events.extend(start_run(
            goal_id,
            resumed_run,
            resumed_agent,
            config(CollaborationPolicy::Single, false),
        ));
        let resumed = SessionView::replay_open(session, &session_with(session, events))
            .expect("resume replays")
            .expect("Session exists");
        assert_eq!(resumed.runs.len(), 2);
        assert_eq!(resumed.runs[0].steering_notes, vec!["steer"]);
        assert!(resumed.runs[1].steering_notes.is_empty());
    }

    #[test]
    fn a_new_goal_is_accepted_after_a_paused_goal_and_owns_its_run() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(first, GoalBudget::default())];
        events.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(first_run, first_agent));
        events.push(goal_started(second, GoalBudget::default()));
        events.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(complete_run_and_goal(second, second_run, second_agent));
        let session = SessionId::new();
        let journal = session_with(session, events);

        let open = SessionView::replay_open(session, &journal)
            .expect("a new Goal pauses its paused predecessor")
            .expect("Session exists");
        assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(open.goals[1].status, GoalStatus::Completed);
        assert_eq!(open.goals[1].run_ids, vec![second_run]);
        assert_eq!(open.goals[0].run_ids, vec![first_run]);
        let closed = SessionView::replay(session, &journal)
            .expect("closed replay agrees")
            .expect("Session exists");
        assert_eq!(closed.interrupted_goal_id(), Some(first));
    }

    #[test]
    fn a_crashed_predecessor_is_sealed_by_a_new_goal() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(first, GoalBudget::default())];
        events.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        // No terminal Run or Goal event: the process died mid-Run.
        events.push(goal_started(second, GoalBudget::default()));
        events.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        let session = SessionId::new();
        let journal = session_with(session, events);

        let open = SessionView::replay_open(session, &journal)
            .expect("a crashed predecessor does not block a new Goal")
            .expect("Session exists");
        assert_eq!(open.goals[0].status, GoalStatus::Interrupted);
        assert_eq!(open.runs[0].status, RunStatus::Interrupted);
        assert_eq!(open.goals[1].status, GoalStatus::Running);
    }

    #[test]
    fn resume_continues_the_most_recent_paused_goal_after_a_newer_goal_finished() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let (resumed_run, resumed_agent) = (RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(first, GoalBudget::default())];
        events.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(first_run, first_agent));
        events.push(goal_started(second, GoalBudget::default()));
        events.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(complete_run_and_goal(second, second_run, second_agent));
        events.push(Event::GoalResumed { goal_id: first });
        events.extend(start_run(
            first,
            resumed_run,
            resumed_agent,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(complete_run_and_goal(first, resumed_run, resumed_agent));
        let session = SessionId::new();
        let journal = session_with(session, events);

        let view = SessionView::replay(session, &journal)
            .expect("resumed Goal replays")
            .expect("Session exists");
        assert_eq!(view.goals[0].status, GoalStatus::Completed);
        assert_eq!(view.goals[0].run_ids, vec![first_run, resumed_run]);
        assert_eq!(view.goals[1].status, GoalStatus::Completed);
        assert_eq!(view.interrupted_goal_id(), None);
    }

    #[test]
    fn an_older_paused_goal_is_not_resumable_once_a_newer_goal_is_paused() {
        let (first, second) = (GoalId::new(), GoalId::new());
        let (first_run, first_agent) = (RunId::new(), AgentRunId::new());
        let (second_run, second_agent) = (RunId::new(), AgentRunId::new());
        let mut paused = vec![goal_started(first, GoalBudget::default())];
        paused.extend(start_run(
            first,
            first_run,
            first_agent,
            config(CollaborationPolicy::Single, false),
        ));
        paused.extend(cancel_run(first_run, first_agent));
        paused.push(goal_started(second, GoalBudget::default()));
        paused.extend(start_run(
            second,
            second_run,
            second_agent,
            config(CollaborationPolicy::Single, false),
        ));
        paused.extend(cancel_run(second_run, second_agent));
        let session = SessionId::new();
        let closed_session = session_with(session, paused.clone());
        let view = SessionView::replay(session, &closed_session)
            .expect("both paused Goals replay")
            .expect("Session exists");
        assert_eq!(view.interrupted_goal_id(), Some(second));

        let mut older = paused.clone();
        older.push(Event::GoalResumed { goal_id: first });
        assert!(
            SessionView::replay_open(session, &session_with(session, older)).is_err(),
            "the older paused Goal is history, not resumable"
        );
        let mut newer = paused;
        newer.push(Event::GoalResumed { goal_id: second });
        assert!(SessionView::replay_open(session, &session_with(session, newer)).is_ok());
    }

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
    fn resumed_crash_left_compaction_call_settles_as_uncertain() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let mut events = vec![goal_started(goal_id, GoalBudget::default())];
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, false),
        ));
        events.extend(cancel_run(run_id, agent_run_id));
        // The process died after the compaction call was reserved, before its outcome was journaled.
        events.push(Event::GoalCompactionCallReserved {
            goal_id,
            run_id,
            active_ceiling_ms: 0,
        });
        events.push(Event::GoalResumed { goal_id });

        let view = SessionView::replay_open(session_id, &session_with(session_id, events))
            .expect("resumed Goal replays")
            .expect("Session exists");
        let goal = view.goal(goal_id).expect("Goal");
        assert_eq!(goal.status, GoalStatus::Running);
        assert_eq!(goal.compaction_call_reserved, None);
        assert_eq!(goal.uncertain_compaction_calls, 1);
        assert!(goal.token_usage_unknown);
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
                intent: intent.clone(),
                observation: Some(ToolObservation {
                    intent,
                    disposition,
                    enforcement_cause: None,
                    output: String::new(),
                    attestation: None,
                    cache_update,
                }),
                review_call_index: None,
                permission_verdict: None,
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
            intent: intent.clone(),
            observation: Some(ToolObservation {
                intent,
                disposition: ToolDisposition::Uncertain,
                enforcement_cause: None,
                output: String::new(),
                attestation: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
            permission_verdict: None,
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
            intent: intent.clone(),
            observation: Some(ToolObservation {
                intent,
                disposition: ToolDisposition::Cancelled,
                enforcement_cause: None,
                output: String::new(),
                attestation: None,
                cache_update: crate::tools::CacheUpdate::NotApplicable,
            }),
            review_call_index: None,
            permission_verdict: None,
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
    fn resumed_orphaned_provider_reservation_settles_as_uncertain() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
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
        events.push(Event::GoalResumed { goal_id });

        let view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("resumed Goal replays")
            .expect("Session exists");
        let goal = view.goals.last().expect("Goal");
        assert!(goal.unobserved_provider_calls.is_empty());
        assert!(goal.token_usage_unknown);
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

        let view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("unknown usage does not block a Finish")
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
                        sha256: crate::tools::types::hex_digest(b"done"),
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
            let receipt =
                view.finish_goal(&goal_id, &GoalStatus::Failed, &settled, &report, true, 0);
            assert!(
                unflagged
                    .finish_goal(&goal_id, &GoalStatus::Failed, &settled, &report, false, 0)
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
    fn plan_wait_does_not_block_a_terminal_failure_before_tool_start() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
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
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, true),
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
                ProviderCallDisposition::ToolRequested,
                Some(0),
            ),
        });
        // Engine fails the primary when the Tool deadline or context capacity refuses before ToolStarted.
        events.push(Event::AgentFinished {
            worker: None,
            run_id,
            agent_run_id,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        });
        events.push(Event::RunFinished {
            run_id,
            disposition: RunDisposition::Failed,
            active_ms: 0,
        });
        events.push(Event::GoalFinished {
            goal_id,
            status: GoalStatus::Failed,
            active_ms: 0,
            report: GoalReport {
                summary: "failed".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: false,
        });
        let mut forged = events.clone();
        if let Some(Event::GoalFinished { exhausted, .. }) = forged.last_mut() {
            *exhausted = true;
        }
        assert!(
            SessionView::replay_open(session_id, &journal(session_id, forged)).is_err(),
            "exhausted is refused while the Goal still admits a Run"
        );

        let view = SessionView::replay_open(session_id, &journal(session_id, events))
            .expect("a terminal failure closes the plan wait")
            .expect("Session exists");
        let goal = view.goals.last().expect("Goal");
        assert_eq!(goal.status, GoalStatus::Failed);
        assert!(!goal.awaiting_plan_contract);
        assert_eq!(view.runs[0].status, RunStatus::Failed);
    }

    #[test]
    fn crash_between_tool_start_and_finish_replays_interrupted_with_the_tool_ceiling_charged() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
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
        events.extend(start_run(
            goal_id,
            run_id,
            agent_run_id,
            config(CollaborationPolicy::Single, true),
        ));
        let intent = read_intent(run_id, agent_run_id, "src/lib.rs");
        events.extend([
            Event::GoalProviderCallReserved {
                goal_id,
                run_id,
                agent_run_id,
                active_ceiling_ms: 605_000,
            },
            Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record: record(
                    AgentPhase::RootPlan,
                    ProviderCallDisposition::ToolRequested,
                    Some(0),
                ),
            },
            Event::ToolStarted {
                run_id,
                agent_run_id,
                intent,
                active_ceiling_ms: 46_000,
            },
        ]);
        let view = SessionView::replay(session_id, &journal(session_id, events.clone()))
            .expect("crash-left prefix")
            .expect("Session");
        let goal = view.goal(goal_id).expect("Goal");
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert!(view.runs[0].tools[0].observation.is_none());
        assert_eq!(goal.status, GoalStatus::Interrupted);
        assert_eq!(
            goal.resume_usage(&view).active_ms,
            46_000,
            "only the open Tool's ceiling is charged, not the settled call's reservation"
        );
        assert!(
            !goal.can_resume(&view),
            "an unobserved effect stops continuation"
        );

        let mut resumed = events;
        resumed.push(Event::GoalResumed { goal_id });
        assert!(SessionView::replay_open(session_id, &journal(session_id, resumed)).is_err());
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

    #[test]
    fn crash_mid_run_charges_only_the_open_action_and_resume_continues() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
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
            active_ceiling_ms: 7_000,
        });
        let mut resumed = events.clone();
        resumed.push(Event::GoalResumed { goal_id });
        resumed.extend(start_run(
            goal_id,
            RunId::new(),
            AgentRunId::new(),
            config(CollaborationPolicy::Single, false),
        ));
        if let Some(Event::RunStarted {
            active_deadline_ms, ..
        }) = resumed.iter_mut().rev().nth(1)
        {
            *active_deadline_ms = Some(crate::session::MAX_GOAL_ACTIVE_MS - 7_000);
        }
        let view = SessionView::replay_open(session_id, &journal(session_id, resumed))
            .expect("resume continues with time left")
            .expect("Session");
        let goal = view.goal(goal_id).expect("Goal");
        assert_eq!((goal.active_ms, goal.uncertain_active_ms), (7_000, 7_000));
        assert_eq!(goal.active_ceiling_ms, 0, "charged once");

        let view = SessionView::replay(session_id, &journal(session_id, events))
            .expect("crash-left Run")
            .expect("Session");
        let mut capped = view.goals[0].clone();
        capped.active_deadline = Some((run_id, Some(3_000)));
        assert_eq!(
            capped.unsettled_active_charge_ms(),
            3_000,
            "the Run deadline caps it"
        );
        capped.active_deadline = Some((run_id, Some(30_000)));
        capped.plan_contract = Some(GoalPlanContract {
            mission: GOAL_OBJECTIVE.into(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: Vec::new(),
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/lib.rs".into(),
                content: "ok".into(),
            },
            budget: GoalBudget {
                max_active_ms: Some(5_000),
                ..GoalBudget::default()
            },
        });
        assert_eq!(
            capped.unsettled_active_charge_ms(),
            5_000,
            "plan narrowing caps it"
        );
    }

    fn delegated_session(known_tokens: u64) -> SessionView {
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
            id: session_id,
            title: "Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some(WorkspaceIdentity {
                device: 1,
                inode: 2,
            }),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 1,
            lineage: None,
            runs: vec![RunView {
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
                tool_compaction_covered_count: 0,
                definitions: Default::default(),
                display: Vec::new(),
                steering_notes: Vec::new(),
                lifecycle_hooks: Vec::new(),
                lifecycle_hook_sequence: None,
                provider_sequences: Default::default(),
                provider_fallbacks: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
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
            }],
        }
    }

    fn apply_child_spawn(view: &mut SessionView, grant: ChildGrant) -> Result<(), ReplayError> {
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
        let mut no_tools = session_with_tools(false);
        assert!(apply_child_spawn(&mut no_tools, ChildGrant::NoTools).is_ok());
        assert_eq!(no_tools.runs[0].agents[1].grant, Some(ChildGrant::NoTools));
        assert!(apply_child_spawn(&mut session_with_tools(false), ChildGrant::ReadOnly).is_err());

        let mut with_tools = session_with_tools(true);
        assert!(apply_child_spawn(&mut with_tools, ChildGrant::ReadOnly).is_ok());
        assert_eq!(
            with_tools.runs[0].agents[1].grant,
            Some(ChildGrant::ReadOnly)
        );
        assert!(apply_child_spawn(&mut session_with_tools(true), ChildGrant::NoTools).is_err());
    }

    fn apply(view: &mut SessionView, event: Event) -> Result<(), ReplayError> {
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

    fn child_intent(run_id: RunId, agent_run_id: AgentRunId, call: ToolCall) -> EffectIntent {
        EffectIntent {
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
            expires_at_ms: 1,
            use_count: 1,
        }
    }

    fn research_session() -> (SessionView, GoalId, RunId, AgentRunId) {
        let mut view = delegated_session(0);
        view.runs[0].config = Some(config(
            CollaborationPolicy::Auto {
                max_active_children: 1,
            },
            true,
        ));
        apply_child_spawn(&mut view, ChildGrant::ReadOnly).expect("research child spawn");
        let (goal, run, child) = (view.goals[0].id, view.runs[0].id, view.runs[0].agents[1].id);
        (view, goal, run, child)
    }

    fn reserve_child_call(goal_id: GoalId, run_id: RunId, agent_run_id: AgentRunId) -> Event {
        Event::GoalProviderCallReserved {
            goal_id,
            run_id,
            agent_run_id,
            active_ceiling_ms: 0,
        }
    }

    fn child_record(
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

    fn rejected(view: &SessionView, event: Event) -> bool {
        apply(&mut view.clone(), event).is_err()
    }

    #[test]
    fn child_research_tools_replay_in_order_and_reject_widening() {
        let (mut view, goal, run, child) = research_session();
        let read = child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/lib.rs".into(),
                offset: 0,
                limit: 1024,
            },
        );
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        assert!(rejected(
            &view,
            Event::ToolStarted {
                run_id: run,
                agent_run_id: child,
                intent: read.clone(),
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
            run_id: run,
            agent_run_id: child,
            intent: read.clone(),
            active_ceiling_ms: 0,
        };
        let finished = Event::ToolFinished {
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
                run_id: run,
                agent_run_id: child,
                intent: write,
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
                run_id: run,
                agent_run_id: child,
                intent: mcp,
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
                    run_id: run,
                    agent_run_id: child,
                    intent,
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
            enforcement_cause: None,
            intent: child_intent(
                run,
                child,
                ToolCall::Read {
                    path: "src/lib.rs".into(),
                    offset: 0,
                    limit: 2,
                },
            ),
            ..observation_for(read.clone(), b"ok")
        };
        assert!(rejected(
            &view,
            Event::ToolFinished {
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
    fn child_tool_outside_the_plan_replays_only_as_denied() {
        let (mut view, goal, run, child) = research_session();
        let outside = child_intent(
            run,
            child,
            ToolCall::Read {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 1024,
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
                run_id: run,
                agent_run_id: child,
                intent: outside.clone(),
                active_ceiling_ms: 0,
            },
        )
        .expect("child Tool starts");
        assert!(rejected(
            &view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: observation_for(outside.clone(), b"x"),
            }
        ));
        apply(
            &mut view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: child,
                observation: ToolObservation {
                    enforcement_cause: None,
                    intent: outside,
                    disposition: ToolDisposition::Denied,
                    output:
                        "The effect is outside the active Goal plan; no operation was dispatched"
                            .into(),
                    attestation: None,
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
                    offset: 0,
                    limit: 1024,
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
                    run_id: run,
                    agent_run_id: child,
                    intent: read.clone(),
                    active_ceiling_ms: 0,
                },
            )
            .expect("child Tool starts");
            apply(
                &mut view,
                Event::ToolFinished {
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
                offset: 0,
                limit: 1024,
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
                run_id: run,
                agent_run_id: child,
                intent: read.clone(),
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
    fn child_admission_uses_the_effective_plan_token_ceiling() {
        let mut below_cap = delegated_session(999);
        assert!(apply_child_spawn(&mut below_cap, ChildGrant::NoTools).is_ok());

        let mut at_cap = delegated_session(1000);
        assert!(apply_child_spawn(&mut at_cap, ChildGrant::NoTools).is_err());
    }

    fn read_observation(path: &str, content: &[u8]) -> ToolObservation {
        file_observation(
            ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1024,
            },
            content,
        )
    }

    fn file_observation(call: ToolCall, content: &[u8]) -> ToolObservation {
        let intent = EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: RunId::new(),
            agent_run_id: AgentRunId::new(),
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 2,
            call,
            limits: ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        observation_for(intent, content)
    }

    fn observation_for(intent: EffectIntent, content: &[u8]) -> ToolObservation {
        ToolObservation {
            enforcement_cause: None,
            attestation: Some(crate::tools::Attestation::for_site(&intent)),
            intent,
            disposition: ToolDisposition::Succeeded,
            output: serde_json::json!({
                "sha256": crate::tools::types::hex_digest(content)
            })
            .to_string(),
            cache_update: CacheUpdate::NotApplicable,
        }
    }

    #[test]
    fn plan_contract_counts_a_pre_plan_read_matching_its_digest() {
        let session_id = SessionId::new();
        let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
        let read = read_observation("src/main.rs", b"before");
        let read_intent = read.intent.clone();
        let contract = GoalPlanContract {
            mission: "Verify the file".into(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            paths: vec!["src/main.rs".into()],
            commands: Vec::new(),
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileDigest {
                path: "src/main.rs".into(),
                sha256: crate::tools::types::hex_digest(b"before"),
            },
            budget: GoalBudget::default(),
        };
        let mut view = SessionView {
            id: session_id,
            title: "Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some(WorkspaceIdentity {
                device: 1,
                inode: 2,
            }),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 1,
            lineage: None,
            runs: vec![RunView {
                workers: Default::default(),
                id: run_id,
                objective: GOAL_OBJECTIVE.into(),
                images: Vec::new(),
                config: Some(config(CollaborationPolicy::Single, true)),
                agents: vec![AgentView {
                    profile: None,
                    assignment: None,
                    definition: None,
                    id: agent_run_id,
                    role: AgentRole::Primary,
                    ordinal: 0,
                    objective: None,
                    summary: None,
                    result: None,
                    provider_calls: vec![
                        record(
                            AgentPhase::RootPlan,
                            ProviderCallDisposition::ToolRequested,
                            Some(0),
                        ),
                        record(
                            AgentPhase::RootPlan,
                            ProviderCallDisposition::ToolRequested,
                            Some(0),
                        ),
                    ],
                    grant: None,
                    tools: Vec::new(),
                    status: AgentStatus::Active,
                }],
                assistant_message: None,
                status: RunStatus::Active,
                accepted_sequence: 1,
                finished_sequence: None,
                tools: vec![ToolView {
                    permission_verdict: None,
                    hooks: Vec::new(),
                    hook_sequence: None,
                    intent: read_intent,
                    observation: Some(read),
                    review_call_index: None,
                    finished_sequence: Some(2),
                }],
                tool_compaction_summary: None,
                tool_compaction_covered_count: 0,
                definitions: Default::default(),
                display: Vec::new(),
                steering_notes: Vec::new(),
                lifecycle_hooks: Vec::new(),
                lifecycle_hook_sequence: None,
                provider_sequences: Default::default(),
                provider_fallbacks: Vec::new(),
            }],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: GOAL_OBJECTIVE.into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
                budget: GoalBudget::default(),
                resource_settings: crate::tools::ToolResourceSettings::default(),
                phase: GoalPhase::Research,
                autonomous: false,
                status: GoalStatus::Running,
                run_ids: vec![run_id],
                active_run: Some(run_id),
                plan_contract: None,
                success_criterion_satisfied: false,
                awaiting_plan_contract: true,
                active_ms: 0,
                uncertain_active_ms: 0,
                active_deadline: None,
                compaction_call_reserved: None,
                uncertain_compaction_calls: 0,
                success_state: crate::session::GoalSuccessState::NoContract,
                provider_calls: 2,
                unobserved_provider_calls: Vec::new(),
                known_tokens: 0,
                known_cached_tokens: 0,
                token_usage_unknown: false,
                report: None,
                started_sequence: 1,
                finished_sequence: None,
                active_ceiling_ms: 0,
                continues: None,
                budget_closed: false,
            }],
        };
        view.apply_committed(&EventEnvelope {
            sequence: 2,
            session_id,
            run_id: Some(run_id),
            agent_run_id: None,
            event: Event::GoalPlanContractRecorded {
                goal_id,
                run_id,
                contract,
            },
            created_at_ms: 0,
        })
        .expect("contract records");
        assert!(view.goals[0].success_criterion_satisfied);
    }

    #[test]
    fn plan_contract_counts_a_write_from_an_earlier_run() {
        let session_id = SessionId::new();
        let (goal_id, earlier_run_id, run_id, agent_run_id) =
            (GoalId::new(), RunId::new(), RunId::new(), AgentRunId::new());
        let write = file_observation(
            ToolCall::Write {
                path: "src/main.rs".into(),
                expected_digest: None,
                content: "after".into(),
            },
            b"after",
        );
        let contract = GoalPlanContract {
            mission: "Write the file".into(),
            paths: vec!["src/main.rs".into()],
            commands: Vec::new(),
            resource_class: crate::tools::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/main.rs".into(),
                content: "after".into(),
            },
            budget: GoalBudget::default(),
        };
        let mut view = SessionView {
            id: session_id,
            title: "Goal".into(),
            title_is_explicit: false,
            inherited_title: None,
            workspace_identity: Some(WorkspaceIdentity {
                device: 1,
                inode: 2,
            }),
            defaults: SessionDefaults::default(),
            created_sequence: 0,
            last_sequence: 2,
            lineage: None,
            runs: vec![
                RunView {
                    workers: Default::default(),
                    id: earlier_run_id,
                    objective: GOAL_OBJECTIVE.into(),
                    images: Vec::new(),
                    config: Some(config(CollaborationPolicy::Single, true)),
                    agents: Vec::new(),
                    assistant_message: None,
                    status: RunStatus::Finished,
                    accepted_sequence: 1,
                    finished_sequence: Some(1),
                    tools: vec![ToolView {
                        permission_verdict: None,
                        hooks: Vec::new(),
                        hook_sequence: None,
                        intent: write.intent.clone(),
                        observation: Some(write),
                        review_call_index: None,
                        finished_sequence: Some(1),
                    }],
                    tool_compaction_summary: None,
                    tool_compaction_covered_count: 0,
                    definitions: Default::default(),
                    display: Vec::new(),
                    steering_notes: Vec::new(),
                    lifecycle_hooks: Vec::new(),
                    lifecycle_hook_sequence: None,
                    provider_sequences: Default::default(),
                    provider_fallbacks: Vec::new(),
                },
                RunView {
                    workers: Default::default(),
                    id: run_id,
                    objective: GOAL_OBJECTIVE.into(),
                    images: Vec::new(),
                    config: Some(config(CollaborationPolicy::Single, true)),
                    agents: vec![AgentView {
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
                            ProviderCallDisposition::ToolRequested,
                            Some(0),
                        )],
                        grant: None,
                        tools: Vec::new(),
                        status: AgentStatus::Active,
                    }],
                    assistant_message: None,
                    status: RunStatus::Active,
                    accepted_sequence: 2,
                    finished_sequence: None,
                    tools: Vec::new(),
                    tool_compaction_summary: None,
                    tool_compaction_covered_count: 0,
                    definitions: Default::default(),
                    display: Vec::new(),
                    steering_notes: Vec::new(),
                    lifecycle_hooks: Vec::new(),
                    lifecycle_hook_sequence: None,
                    provider_sequences: Default::default(),
                    provider_fallbacks: Vec::new(),
                },
            ],
            compactions: Vec::new(),
            goals: vec![GoalView {
                id: goal_id,
                objective: GOAL_OBJECTIVE.into(),
                workspace_identity: WorkspaceIdentity {
                    device: 1,
                    inode: 2,
                },
                budget: GoalBudget::default(),
                resource_settings: crate::tools::ToolResourceSettings::default(),
                phase: GoalPhase::Research,
                autonomous: false,
                status: GoalStatus::Running,
                run_ids: vec![earlier_run_id, run_id],
                active_run: Some(run_id),
                plan_contract: None,
                success_state: crate::session::GoalSuccessState::NoContract,
                success_criterion_satisfied: false,
                awaiting_plan_contract: true,
                active_ms: 0,
                uncertain_active_ms: 0,
                active_deadline: None,
                provider_calls: 2,
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
            }],
        };
        view.apply_committed(&EventEnvelope {
            sequence: 3,
            session_id,
            run_id: Some(run_id),
            agent_run_id: None,
            event: Event::GoalPlanContractRecorded {
                goal_id,
                run_id,
                contract,
            },
            created_at_ms: 0,
        })
        .expect("contract records");
        assert!(view.goals[0].success_criterion_satisfied);
        assert_eq!(
            view.goals[0].success_state,
            crate::session::GoalSuccessState::WorkspaceFile {
                sha256: Some(crate::tools::types::hex_digest(b"after")),
            }
        );
    }

    fn primary_tool_session(planned: bool) -> (SessionView, GoalId, RunId, AgentRunId) {
        let mut view = delegated_session(0);
        view.runs[0].config = Some(config(CollaborationPolicy::Single, true));
        view.runs[0].agents[0].provider_calls.clear();
        if !planned {
            view.goals[0].plan_contract = None;
        }
        let (goal, run, agent) = (view.goals[0].id, view.runs[0].id, view.runs[0].agents[0].id);
        (view, goal, run, agent)
    }

    fn read_intent(run: RunId, agent: AgentRunId, path: &str) -> EffectIntent {
        child_intent(
            run,
            agent,
            ToolCall::Read {
                path: path.into(),
                offset: 0,
                limit: 1024,
            },
        )
    }

    #[test]
    fn permission_verdict_replay_requires_pending_intent_and_blocks_denied_effects() {
        use crate::tools::permissions::{
            Decision, PermissionMode, PermissionSource, PermissionVerdict,
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
            let verdict = Event::ToolPermissionDecided {
                run_id: run,
                agent_run_id: agent,
                verdict: PermissionVerdict {
                    intent_digest: intent.digest(),
                    decision: Decision::Deny,
                    mode: Some(PermissionMode::Default),
                    basis: PermissionSource::Rule,
                    source: PermissionSource::Rule,
                    approved: false,
                    hook_restriction: None,
                    hooks: 0,
                    narrowed: false,
                },
            };
            assert_eq!(
                Event::decode(
                    "ToolPermissionDecided",
                    4,
                    &verdict.payload().unwrap(),
                    Some(run),
                    Some(agent)
                )
                .unwrap(),
                verdict
            );
            assert!(rejected(&view, verdict.clone()));
            apply(&mut view, tool_started(run, agent, &intent)).unwrap();
            let succeeded = tool_finished(run, agent, &intent, b"ok");
            assert!(rejected(&view, succeeded.clone()));
            let mut wrong_intent = verdict.clone();
            if let Event::ToolPermissionDecided { verdict, .. } = &mut wrong_intent {
                verdict.intent_digest = [4; 32];
            }
            assert!(rejected(&view, wrong_intent));
            for mode in [PermissionMode::Auto, PermissionMode::Default] {
                let mut unreviewed = verdict.clone();
                if let Event::ToolPermissionDecided { verdict, .. } = &mut unreviewed {
                    verdict.decision = Decision::Ask;
                    verdict.mode = Some(mode);
                    verdict.basis = PermissionSource::Mode;
                    verdict.source = PermissionSource::AiReview;
                    verdict.approved = true;
                }
                assert!(unreviewed.payload().is_ok());
                assert!(rejected(&view, unreviewed.clone()));
                if let Event::ToolPermissionDecided { verdict, .. } = &mut unreviewed {
                    verdict.basis = PermissionSource::Rule;
                }
                assert!(unreviewed.payload().is_err());
            }
            apply(&mut view, verdict.clone()).unwrap();
            assert!(rejected(&view, verdict));
            assert!(rejected(&view, succeeded));
            let mut denied = observation_for(intent, b"unused");
            denied.disposition = ToolDisposition::Denied;
            denied.attestation = None;
            denied.output = "Permission deny rule blocked this Tool".into();
            apply(
                &mut view,
                Event::ToolFinished {
                    run_id: run,
                    agent_run_id: agent,
                    observation: denied,
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn hook_events_replay_in_order_and_bind_the_verdict_to_their_outcomes() {
        use crate::tools::permissions::{
            Decision, HookEvent, HookIgnoreReason, HookInvocation, HookOutcome, HookResult,
            PermissionMode, PermissionSource, PermissionVerdict,
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
        let intent = read_intent(run, agent, "src/lib.rs");
        let hook_intent = child_intent(
            run,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
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
        let verdict = |decision, restriction, hooks, basis| Event::ToolPermissionDecided {
            run_id: run,
            agent_run_id: agent,
            verdict: PermissionVerdict {
                intent_digest: intent.digest(),
                decision,
                mode: Some(PermissionMode::Default),
                basis,
                source: basis,
                approved: false,
                hook_restriction: restriction,
                hooks,
                narrowed: false,
            },
        };
        assert!(rejected(&view, started(intent.digest(), &hook_intent)));
        apply(&mut view, tool_started(run, agent, &intent)).unwrap();
        let mut hook_observation = observation_for(hook_intent.clone(), b"");
        hook_observation.disposition = ToolDisposition::Failed;
        hook_observation.output = serde_json::json!({"exit_code": 2, "stdout": "", "stderr": "blocked", "changeset": {"files": []}}).to_string();
        assert!(rejected(
            &view,
            finished(hook_observation.clone(), deny.clone())
        ));
        assert!(rejected(&view, started([4; 32], &hook_intent)));
        let mut foreign = hook_intent.clone();
        foreign.policy_digest = [8; 32];
        assert!(rejected(&view, started(intent.digest(), &foreign)));
        apply(&mut view, started(intent.digest(), &hook_intent)).unwrap();
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
        let ask_verdict = |source| Event::ToolPermissionDecided {
            run_id: run,
            agent_run_id: agent,
            verdict: PermissionVerdict {
                intent_digest: intent.digest(),
                decision: Decision::Ask,
                mode: Some(PermissionMode::Auto),
                basis: PermissionSource::Mode,
                source,
                approved: true,
                hook_restriction: Some(Decision::Ask),
                hooks: 1,
                narrowed: false,
            },
        };
        assert!(ask_verdict(PermissionSource::Human).payload().is_ok());
        assert!(ask_verdict(PermissionSource::AiReview).payload().is_err());
        let forged_pass = finished(hook_observation.clone(), HookResult::Passed);
        assert!(forged_pass.payload().is_err());
        let mid = view.clone();
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
        forged.runs[0].tools[0].hooks[0].outcome = Some(HookOutcome {
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
            Event::decode("ToolPermissionDecided", 2, &payload, Some(run), Some(agent)),
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
        assert!(!after_uncertain.runs[0].tools[0].settled_without_stop());
        let mut stopped = view.clone();
        stopped.runs[0].tools[0].hooks[0].outcome = Some(HookOutcome {
            boundary_active_ms: None,
            observation: uncertain,
            result: not_completed,
        });
        assert!(!stopped.runs[0].tools[0].settled_without_stop());
    }

    #[test]
    fn post_tool_use_hooks_need_a_committed_observation_and_feed_back_only_their_result() {
        use crate::tools::permissions::{
            Decision, HookEvent, HookInvocation, HookOutcome, HookResult, PermissionMode,
            PermissionSource, PermissionVerdict,
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
        let intent = read_intent(run, agent, "src/lib.rs");
        let hook_intent = child_intent(
            run,
            agent,
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 2".into()],
                cwd: String::new(),
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
        apply(&mut view, tool_started(run, agent, &intent)).unwrap();
        assert!(rejected(
            &view,
            started(HookEvent::PostToolUse, intent.digest())
        ));
        apply(
            &mut view,
            Event::ToolPermissionDecided {
                run_id: run,
                agent_run_id: agent,
                verdict: PermissionVerdict {
                    intent_digest: intent.digest(),
                    decision: Decision::Allow,
                    mode: Some(PermissionMode::Default),
                    basis: PermissionSource::Mode,
                    source: PermissionSource::Mode,
                    approved: true,
                    hook_restriction: None,
                    hooks: 0,
                    narrowed: false,
                },
            },
        )
        .unwrap();
        apply(&mut view, tool_finished(run, agent, &intent, b"x")).unwrap();
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
                AgentPhase::RootPlan,
                ProviderCallDisposition::Finished,
                Some(0),
            ),
        };
        let mut open = view.clone();
        apply(&mut open, reserve_child_call(goal, run, agent)).unwrap();
        assert!(
            rejected(&open, continuation.clone()),
            "no continuation is recorded while a PostToolUse hook is open"
        );
        let mut hook_observation = observation_for(hook_intent.clone(), b"");
        hook_observation.disposition = ToolDisposition::Failed;
        hook_observation.output = serde_json::json!({"exit_code": 2, "stdout": "", "stderr": "blocked", "changeset": {"files": []}}).to_string();
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
        let tool = &accepted.runs[0].tools[0];
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
        assert!(!view.runs[0].tools[0].settled_without_stop());
        assert!(view.runs[0].tools[0].hook_feedback().is_empty());
    }

    #[test]
    fn no_tool_starts_while_a_post_tool_use_hook_is_open() {
        use crate::tools::permissions::{
            Decision, HookEvent, HookInvocation, PermissionMode, PermissionSource,
            PermissionVerdict,
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
        apply(&mut view, requesting_step(run, agent, 2)).unwrap();
        let (first, second) = (
            read_intent(run, agent, "src/lib.rs"),
            read_intent(run, agent, "src/lib.rs"),
        );
        apply(&mut view, tool_started(run, agent, &first)).unwrap();
        apply(
            &mut view,
            Event::ToolPermissionDecided {
                run_id: run,
                agent_run_id: agent,
                verdict: PermissionVerdict {
                    intent_digest: first.digest(),
                    decision: Decision::Allow,
                    mode: Some(PermissionMode::Default),
                    basis: PermissionSource::Mode,
                    source: PermissionSource::Mode,
                    approved: true,
                    hook_restriction: None,
                    hooks: 0,
                    narrowed: false,
                },
            },
        )
        .unwrap();
        apply(&mut view, tool_finished(run, agent, &first, b"x")).unwrap();
        let mut settled = view.clone();
        apply(&mut settled, tool_started(run, agent, &second)).unwrap();
        apply(
            &mut view,
            Event::HookStarted {
                run_id: run,
                agent_run_id: agent,
                hook: HookInvocation {
                    active_ceiling_ms: None,
                    event: HookEvent::PostToolUse,
                    group: 0,
                    handler: 0,
                    subject_digest: first.digest(),
                    intent: child_intent(
                        run,
                        agent,
                        ToolCall::Command {
                            program: "sh".into(),
                            args: vec!["-c".into(), "exit 0".into()],
                            cwd: String::new(),
                        },
                    ),
                    input_digest: [9; 32],
                },
            },
        )
        .unwrap();
        assert!(rejected(&view, tool_started(run, agent, &second)));
    }

    struct Unguarded {
        view: SessionView,
        host: EffectIntent,
        run: RunId,
        agent: AgentRunId,
        started: bool,
    }

    // A primary that ran `sh -c <prior_script>` in the Guard with this outcome, then requests the host re-run of `exit 7`.
    fn unguarded_scenario(
        prior: Option<(
            &str,
            ToolDisposition,
            Option<crate::tools::EnforcementCause>,
        )>,
        configured: bool,
        rules_pinned: bool,
    ) -> Unguarded {
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
        policy.permissions_digest = configured.then_some([3; 32]);
        policy.unguarded_rules_digest = rules_pinned.then_some([5; 32]);
        apply(&mut view, reserve_child_call(goal, run, agent)).unwrap();
        apply(
            &mut view,
            requesting_step(run, agent, 1 + u8::from(prior.is_some())),
        )
        .unwrap();
        if let Some((script, disposition, cause)) = prior {
            let guarded = child_intent(
                run,
                agent,
                ToolCall::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), script.into()],
                    cwd: String::new(),
                },
            );
            apply(&mut view, tool_started(run, agent, &guarded)).unwrap();
            if configured {
                apply(
                    &mut view,
                    verdict_event(
                        &guarded,
                        configured.then_some(crate::tools::permissions::PermissionMode::Default),
                        crate::tools::permissions::Decision::Allow,
                        crate::tools::permissions::PermissionSource::Mode,
                        crate::tools::permissions::PermissionSource::Mode,
                        true,
                    ),
                )
                .unwrap();
            }
            let mut observation = observation_for(guarded, b"ok");
            observation.disposition = disposition;
            observation.enforcement_cause = cause;
            observation.output =
                r#"{"exit_code":7,"stdout":"","stderr":"","changeset":{"files":[]}}"#.into();
            apply(
                &mut view,
                Event::ToolFinished {
                    run_id: run,
                    agent_run_id: agent,
                    observation,
                },
            )
            .unwrap();
        }
        let host = child_intent(
            run,
            agent,
            ToolCall::CommandUnguarded {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 7".into()],
                cwd: String::new(),
                justification: "Run the failed command once".into(),
            },
        );
        let start = tool_started(run, agent, &host);
        if matches!(
            prior,
            Some((
                _,
                ToolDisposition::Uncertain | ToolDisposition::Cancelled,
                _
            ))
        ) {
            // The Run stops at these outcomes, so the reducer refuses the unguarded start itself.
            assert!(rejected(&view, start), "{prior:?}");
            return Unguarded {
                view,
                host,
                run,
                agent,
                started: false,
            };
        }
        apply(&mut view, start).expect("the start is always admitted");
        Unguarded {
            view,
            host,
            run,
            agent,
            started: true,
        }
    }

    #[test]
    fn an_approved_host_run_without_an_outcome_counts_as_unguarded() {
        use crate::tools::permissions::{Decision, PermissionSource};
        let mut scenario = unguarded_scenario(
            Some(("exit 7", ToolDisposition::Failed, None)),
            false,
            false,
        );
        let count = |view: &SessionView| view.runs[0].unguarded_tools();
        assert_eq!(count(&scenario.view), 0);
        let (host, run, agent) = (scenario.host.clone(), scenario.run, scenario.agent);
        let mut refused = scenario.view.clone();
        apply(
            &mut refused,
            verdict_event(
                &host,
                None,
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::UnavailableApproval,
                false,
            ),
        )
        .unwrap();
        assert_eq!(count(&refused), 0);
        let _ = (run, agent);
        apply(
            &mut scenario.view,
            verdict_event(
                &host,
                None,
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                true,
            ),
        )
        .unwrap();
        assert_eq!(count(&scenario.view), 1);
    }

    fn verdict_event(
        intent: &EffectIntent,
        mode: Option<crate::tools::permissions::PermissionMode>,
        decision: crate::tools::permissions::Decision,
        basis: crate::tools::permissions::PermissionSource,
        source: crate::tools::permissions::PermissionSource,
        approved: bool,
    ) -> Event {
        Event::ToolPermissionDecided {
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            verdict: crate::tools::permissions::PermissionVerdict {
                intent_digest: intent.digest(),
                decision,
                mode,
                basis,
                source,
                approved,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            },
        }
    }

    #[test]
    fn unguarded_replay_requires_prior_guard_failure_human_or_exact_rule_verdict_and_unguarded_attestation()
     {
        use crate::tools::EnforcementCause;
        use crate::tools::permissions::{Decision, PermissionMode, PermissionSource};
        let failed = Some(("exit 7", ToolDisposition::Failed, None));
        for configured in [false, true] {
            let mode = configured.then_some(PermissionMode::Default);
            let human = |scenario: &Unguarded| {
                verdict_event(
                    &scenario.host,
                    mode,
                    Decision::Ask,
                    PermissionSource::Mode,
                    PermissionSource::Human,
                    true,
                )
            };
            let good = unguarded_scenario(failed, configured, true);
            assert!(!rejected(&good.view, human(&good)));

            for prior in [
                None,
                Some(("exit 8", ToolDisposition::Failed, None)),
                Some(("exit 7", ToolDisposition::Limit, None)),
                Some((
                    "exit 7",
                    ToolDisposition::Denied,
                    Some(EnforcementCause::FilesystemDenied),
                )),
                Some(("exit 7", ToolDisposition::Succeeded, None)),
                Some(("exit 7", ToolDisposition::Uncertain, None)),
                Some(("exit 7", ToolDisposition::Cancelled, None)),
            ] {
                let scenario = unguarded_scenario(prior, configured, true);
                if !scenario.started {
                    continue;
                }
                assert!(rejected(&scenario.view, human(&scenario)), "{prior:?}");
                let mut denied = scenario.view.clone();
                apply(
                    &mut denied,
                    verdict_event(
                        &scenario.host,
                        mode,
                        Decision::Ask,
                        PermissionSource::Mode,
                        PermissionSource::UnavailableApproval,
                        false,
                    ),
                )
                .expect("a denial is journaled without a qualifying failure");
                let mut refusal = observation_for(scenario.host.clone(), b"");
                refusal.disposition = ToolDisposition::Denied;
                refusal.attestation = None;
                refusal.output = "No identical command failed under the Guard".into();
                apply(
                    &mut denied,
                    Event::ToolFinished {
                        run_id: scenario.run,
                        agent_run_id: scenario.agent,
                        observation: refusal,
                    },
                )
                .expect("the refusal finishes");
            }

            for (basis, source, approved) in [
                (PermissionSource::Mode, PermissionSource::AiReview, true),
                (PermissionSource::Rule, PermissionSource::Rule, true),
                (PermissionSource::Mode, PermissionSource::Mode, true),
                (
                    PermissionSource::UnavailableApproval,
                    PermissionSource::UnavailableApproval,
                    true,
                ),
                (
                    PermissionSource::PinnedGrant,
                    PermissionSource::PinnedGrant,
                    true,
                ),
            ] {
                assert!(
                    rejected(
                        &good.view,
                        verdict_event(&good.host, mode, Decision::Ask, basis, source, approved)
                    ),
                    "{source:?}"
                );
            }
            let exact = |scenario: &Unguarded| {
                verdict_event(
                    &scenario.host,
                    mode,
                    Decision::Allow,
                    PermissionSource::ExactCommandRule,
                    PermissionSource::ExactCommandRule,
                    true,
                )
            };
            assert!(!rejected(&good.view, exact(&good)));
            let unpinned = unguarded_scenario(failed, configured, false);
            assert!(rejected(&unpinned.view, exact(&unpinned)));
            let forged_decision = verdict_event(
                &good.host,
                mode,
                Decision::Deny,
                PermissionSource::Mode,
                PermissionSource::Human,
                true,
            );
            assert!(rejected(&good.view, forged_decision));
        }

        let good = unguarded_scenario(failed, true, true);
        let mode = Some(PermissionMode::Default);
        let mut approved = good.view.clone();
        apply(
            &mut approved,
            verdict_event(
                &good.host,
                mode,
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                true,
            ),
        )
        .unwrap();
        let finish = |observation: ToolObservation| Event::ToolFinished {
            run_id: good.run,
            agent_run_id: good.agent,
            observation,
        };
        let mut host_run = observation_for(good.host.clone(), b"ok");
        assert!(matches!(
            host_run.attestation,
            Some(crate::tools::Attestation::Unguarded(_))
        ));
        let mut guard_receipt = host_run.clone();
        guard_receipt.attestation = Some(crate::tools::Attestation::Guard(
            crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: good.host.digest(),
                enforcement_digest: good.host.enforcement_digest,
                limits: good.host.limits.clone(),
            },
        ));
        assert!(rejected(&approved, finish(guard_receipt)));
        let mut cached = host_run.clone();
        cached.cache_update = CacheUpdate::Published;
        assert!(rejected(&approved, finish(cached)));
        host_run.output =
            r#"{"exit_code":0,"stdout":"","stderr":"","changeset":{"files":[]}}"#.into();
        let no_verdict = good.view.clone();
        assert!(rejected(&no_verdict, finish(host_run.clone())));
        let mut refused = good.view.clone();
        apply(
            &mut refused,
            verdict_event(
                &good.host,
                mode,
                Decision::Ask,
                PermissionSource::Mode,
                PermissionSource::Human,
                false,
            ),
        )
        .unwrap();
        assert!(rejected(&refused, finish(host_run.clone())));
        apply(&mut approved, finish(host_run.clone())).expect("the approved host run finishes");

        let integration = Event::ToolStarted {
            run_id: good.run,
            agent_run_id: good.agent,
            intent: child_intent(
                good.run,
                good.agent,
                ToolCall::ApplyChangeset {
                    source: crate::session::worker::ChangesetSource::CommandReceipt {
                        source_intent_digest: good.host.digest(),
                        changeset: crate::tools::CommandChangeset { files: Vec::new() },
                    },
                },
            ),
            active_ceiling_ms: 0,
        };
        assert!(rejected(&approved, integration));

        let (mut child, goal, run, researcher) = research_session();
        child.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .permissions_digest = Some([3; 32]);
        apply(&mut child, reserve_child_call(goal, run, researcher)).unwrap();
        apply(
            &mut child,
            child_record(run, researcher, ProviderCallDisposition::ToolRequested),
        )
        .unwrap();
        let forged = child_intent(
            run,
            researcher,
            ToolCall::CommandUnguarded {
                program: "sh".into(),
                args: vec!["-c".into(), "exit 7".into()],
                cwd: String::new(),
                justification: "Run the failed command once".into(),
            },
        );
        assert!(rejected(&child, tool_started(run, researcher, &forged)));
    }

    #[test]
    fn verdict_mode_and_exact_rule_are_pinned_by_the_policy_receipt() {
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
        let intent = read_intent(run, agent, "src/lib.rs");
        apply(&mut view, tool_started(run, agent, &intent)).unwrap();
        let verdict = |mode, decision, basis, source, approved| Event::ToolPermissionDecided {
            run_id: run,
            agent_run_id: agent,
            verdict: PermissionVerdict {
                intent_digest: intent.digest(),
                decision,
                mode,
                basis,
                source,
                approved,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            },
        };
        let exact = |approved| {
            verdict(
                Some(PermissionMode::Default),
                Decision::Allow,
                PermissionSource::ExactCommandRule,
                PermissionSource::ExactCommandRule,
                approved,
            )
        };
        let ordinary = verdict(
            Some(PermissionMode::Default),
            Decision::Deny,
            PermissionSource::Rule,
            PermissionSource::Rule,
            false,
        );
        assert!(rejected(
            &view,
            verdict(
                None,
                Decision::Deny,
                PermissionSource::Rule,
                PermissionSource::Rule,
                false
            )
        ));
        assert!(rejected(&view, exact(true)));
        let mut pinned = view.clone();
        pinned.runs[0]
            .config
            .as_mut()
            .unwrap()
            .tool_policy
            .as_mut()
            .unwrap()
            .unguarded_rules_digest = Some([5; 32]);
        assert!(rejected(&pinned, exact(true)));
        assert!(!rejected(&view, ordinary.clone()));
        apply(&mut view, ordinary).expect("the ordinary verdict is admitted");
    }

    fn tool_started(run: RunId, agent: AgentRunId, intent: &EffectIntent) -> Event {
        Event::ToolStarted {
            run_id: run,
            agent_run_id: agent,
            intent: intent.clone(),
            active_ceiling_ms: 0,
        }
    }

    fn tool_finished(
        run: RunId,
        agent: AgentRunId,
        intent: &EffectIntent,
        content: &[u8],
    ) -> Event {
        Event::ToolFinished {
            run_id: run,
            agent_run_id: agent,
            observation: observation_for(intent.clone(), content),
        }
    }

    fn requesting_step(run: RunId, agent: AgentRunId, requested_tools: u8) -> Event {
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
        apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
        apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
        apply(&mut view, tool_started(run, agent, &second)).expect("second request starts");
        apply(&mut view, tool_finished(run, agent, &second, b"two")).expect("second observed");
        assert!(
            rejected(&view, tool_started(run, agent, &third)),
            "every declared request is already started"
        );
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
            policy_digest: write.policy_digest,
            workspace_device: write.workspace_device,
            workspace_inode: write.workspace_inode,
        });
        assert!(rejected(
            &view,
            Event::ToolFinished {
                run_id: run,
                agent_run_id: agent,
                observation: read_attested,
            }
        ));
        apply(&mut view, tool_finished(run, agent, &write, b"after")).expect("Guard receipt");
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
        apply(&mut view, tool_started(run, agent, &first)).expect("first request starts");
        apply(&mut view, tool_finished(run, agent, &first, b"one")).expect("first observed");
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("continuation call");
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

    fn compacting_session() -> (SessionView, GoalId, RunId) {
        let (mut view, goal, run, agent) = primary_tool_session(true);
        let (first, second) = (
            read_intent(run, agent, "src/lib.rs"),
            read_intent(run, agent, "src/lib.rs"),
        );
        apply(&mut view, reserve_child_call(goal, run, agent)).expect("primary call");
        apply(&mut view, requesting_step(run, agent, 2)).expect("two-request step");
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
                covered_run_id: run,
                covered_sequence: covered.finished_sequence.expect("observed Tool"),
                source_digest: [0; 32],
                provider: "openai".into(),
                model: "gpt-5.4".into(),
                prompt_version: 1,
                schema_version: 1,
                compiler_version: 3,
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

    #[test]
    fn child_tool_request_count_is_one() {
        let (mut view, goal, run, child) = research_session();
        apply(&mut view, reserve_child_call(goal, run, child)).expect("child call");
        let widened = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: child,
            record: ProviderCallRecord {
                requested_tools: 2,
                ..record(
                    AgentPhase::ChildWork,
                    ProviderCallDisposition::ToolRequested,
                    Some(0),
                )
            },
        };
        assert!(rejected(&view, widened), "a child step declares one Tool");
        apply(
            &mut view,
            child_record(run, child, ProviderCallDisposition::ToolRequested),
        )
        .expect("single child request");
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
            digest: crate::tools::types::digest(&serde_json::to_vec(&files).unwrap()),
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
            cache_update: CacheUpdate::NotApplicable,
            enforcement_cause: None,
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
            run_id: intent.run_id,
            agent_run_id: intent.agent_run_id,
            intent,
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
            policy_digest: intent.policy_digest,
            workspace_device: 1,
            workspace_inode: 2,
        });
        assert!(rejected(
            &view,
            Event::ToolFinished {
                run_id: intent.run_id,
                agent_run_id: intent.agent_run_id,
                observation: in_process
            }
        ));
        apply(
            &mut view,
            Event::ToolFinished {
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
                    run_id: intent.run_id,
                    agent_run_id: intent.agent_run_id,
                    intent: intent.clone(),
                    active_ceiling_ms: 0,
                },
            )
            .unwrap();
            apply(
                &mut view,
                Event::ToolFinished {
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
            observation.output = serde_json::json!({"exit_code":2,"stdout":"","stderr":"verify","changeset":{"files":[]}}).to_string();
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
            if event != HookEvent::PreCompact {
                let mut stopped_view = view.clone();
                let mut stopped = finished.clone();
                if let Event::HookFinished { outcome, .. } = &mut stopped {
                    outcome.observation.disposition = ToolDisposition::Succeeded;
                    outcome.observation.output = serde_json::json!({"exit_code":0,"stdout":r#"{"continue":false,"stopReason":"halt"}"#,"stderr":"","changeset":{"files":[]}}).to_string();
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
                    stopped_view.goals[0].active_deadline =
                        Some((run, Some(crate::session::MAX_GOAL_ACTIVE_MS)));
                    let terminal = Event::RunFinished {
                        run_id: run,
                        disposition: RunDisposition::StoppedByHook,
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
                    active_ceiling_ms: Some(1000),
                },
            },
        )
        .unwrap();
        assert_eq!(view.goals[0].resume_usage(&view).active_ms, 1000);
        assert!(!view.runs[0].hooks_settled());
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
            outcome.boundary_active_ms = Some(1001);
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
                    active_ceiling_ms: Some(1000),
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
        assert_eq!(view.goals[0].resume_usage(&view).active_ms, 1000);
    }

    #[test]
    fn crash_with_an_open_idle_precompact_leaves_the_session_usable() {
        use crate::tools::permissions::{HookEvent, HookInvocation, compact_subject};
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
                    active_ceiling_ms: Some(1000),
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
        assert_eq!(view.goals[0].resume_usage(&view).active_ms, 1000);
    }
}
