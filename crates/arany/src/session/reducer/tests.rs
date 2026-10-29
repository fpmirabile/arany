use super::test_support::*;
use super::*;
use crate::provider::{AgentPhase, OutputTokenBound};
use crate::session::events::CollaborationPolicy;
use crate::session::{ProviderSetupDisposition, ProviderSetupTarget, ProviderSetupTargetKind};
use crate::supervisor::{MAX_VERDICTS, SupervisorState};
use crate::tools::{CacheUpdate, EffectIntent, ToolCall, ToolDisposition, ToolObservation};

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

async fn seed_setup_crash_prefix(
    root: &crate::StateRoot,
    workspace: &std::path::Path,
    prefix: usize,
) -> SessionId {
    let identity = crate::session::input::WorkspaceInputs::admit_identity(workspace, root).unwrap();
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_selection_failure_survives_the_store_and_replay() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let root = crate::StateRoot::admit(&temp.path().join("state")).unwrap();
    let session = selection_failure_session(&root, &workspace).await;
    let store = crate::Store::open_read_only(root).unwrap();
    let committed = store.load_session(session).await.unwrap();
    let view = SessionView::replay_open(session, &committed)
        .unwrap()
        .unwrap();
    store.close().await.unwrap();
    let observation = view.runs[0].tools[0].observation.as_ref().unwrap();
    assert_eq!(
        observation.guard_stage_diagnostic().as_deref(),
        Some("Guard stage: project selection timed out")
    );
    assert!(!observation.output.contains("runtime mount topology"));
}

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

fn supervisor_interrupted_prefix(view: &SessionView, envelopes: &[EventEnvelope], goal: GoalId) {
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
        let closed = SessionView::replay_for_corpus(session, &terminal_events, None, &mut corpus)
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
        guard_stage: None,
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
        // A single-route custom profile shares its own account, so Tools stay admitted.
        assert_eq!(invalid.validate().is_err(), !custom);
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
                prompt: crate::provider::EffectivePrompts::default()
                    .pin(crate::provider::PromptFile::Compaction),
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
