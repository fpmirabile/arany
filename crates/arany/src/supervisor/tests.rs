use super::*;

fn step(n: u64, call: u8, success: bool) -> Step {
    Step {
        sequence: n,
        call: [call; 32],
        outcome: [call; 32],
        succeeded: success,
        denied: false,
        alias: false,
        stall: false,
        observation: true,
        edits: Vec::new(),
    }
}
fn looping(signals: &[Signal]) -> Option<(&LoopFeatures, Level)> {
    signals.iter().find_map(|s| match &s.features {
        Features::Loop(f) => Some((f, s.level)),
        _ => None,
    })
}
#[test]
fn supervisor_loop_signal_corpus() {
    let policy = SupervisorPolicy::default();
    for (n, success, expected) in [
        (2, true, None),
        (3, true, Some(Level::Notice)),
        (4, true, Some(Level::WouldNarrow)),
        (1, false, None),
        (2, false, Some(Level::Notice)),
        (3, false, Some(Level::WouldNarrow)),
    ] {
        let window = (1..=n).map(|i| step(i, 1, success)).collect();
        let signals = evaluate(&window, None, &policy);
        assert_eq!(
            looping(&signals).map(|(_, l)| l),
            expected,
            "repeat {n} success {success}"
        );
    }
    for (k, reps, level) in [
        (2, 3, Level::WouldNarrow),
        (3, 3, Level::Notice),
        (3, 5, Level::WouldNarrow),
        (4, 3, Level::Notice),
        (4, 5, Level::WouldNarrow),
    ] {
        let window = (0..k * reps)
            .map(|i| step(i + 1, (i % k + 1) as u8, true))
            .collect();
        let signals = evaluate(&window, None, &policy);
        let (f, l) = looping(&signals).expect("cycle");
        assert_eq!(
            (f.cycle_len, f.cycle_reps, l),
            (k as u16, reps as u16, level)
        );
    }
    for (n, level) in [(2, Level::Notice), (3, Level::WouldNarrow)] {
        let mut window: VecDeque<_> = (1..=n)
            .map(|i| {
                let mut s = step(i, 0, false);
                s.observation = false;
                s.stall = true;
                s
            })
            .collect();
        assert_eq!(looping(&evaluate(&window, None, &policy)).unwrap().1, level);
        window.push_back(step(n + 1, 8, true));
        assert!(looping(&evaluate(&window, None, &policy)).is_none());
    }
    for (n, level, returns) in [(3, Level::Notice, 1), (4, Level::WouldNarrow, 2)] {
        let window = (0..n)
            .map(|i| {
                let mut s = step(i + 1, (i + 1) as u8, true);
                s.edits.push(([1; 32], [(i % 2) as u8; 32]));
                s
            })
            .collect();
        let signals = evaluate(&window, None, &policy);
        let (f, l) = looping(&signals).unwrap();
        assert_eq!((f.edit_oscillation, l), (returns, level));
    }
}
#[test]
fn supervisor_probing_signal_corpus() {
    let policy = SupervisorPolicy::default();
    for (n, alias, expected) in [
        (1, false, None),
        (2, false, Some(Level::Notice)),
        (3, false, Some(Level::WouldNarrow)),
        (1, true, Some(Level::Notice)),
        (2, true, Some(Level::WouldNarrow)),
    ] {
        let window = (1..=n)
            .map(|i| {
                let mut s = step(i, i as u8, false);
                s.denied = true;
                s.alias = alias;
                s
            })
            .collect();
        let signals = evaluate(&window, None, &policy);
        assert_eq!(
            signals
                .iter()
                .find(|s| matches!(s.features, Features::Probing(_)))
                .map(|s| s.level),
            expected
        );
    }
    let window = (1..=7)
        .map(|i| {
            let mut s = step(i, i as u8, false);
            s.denied = i % 2 == 1;
            s
        })
        .collect();
    let signals = evaluate(&window, None, &policy);
    assert!(signals.iter().any(|s| matches!(&s.features,Features::Probing(f) if f.distinct_denied==4 && f.denials_window==4) && s.level==Level::WouldNarrow));
    let window = (1..=4).map(|i| step(i, i as u8, false)).collect();
    assert!(
        !evaluate(&window, None, &policy)
            .iter()
            .any(|s| matches!(s.features, Features::Probing(_)))
    );
}
#[test]
fn supervisor_disproportion_boundaries_saturation_and_privacy() {
    let policy = SupervisorPolicy::default();
    let window = VecDeque::from([step(1, 1, true)]);
    for (files, deleted, changed, base, expected) in [
        (15, 0, 0, None, None),
        (16, 0, 0, None, Some(Level::Notice)),
        (31, 0, 0, None, Some(Level::Notice)),
        (32, 0, 0, None, Some(Level::WouldNarrow)),
        (3, 1, 0, None, None),
        (4, 1, 0, None, Some(Level::Notice)),
        (16, 8, 0, None, Some(Level::WouldNarrow)),
        (1, 0, 63, Some(1), None),
        (1, 0, 64, Some(16), Some(Level::WouldNarrow)),
    ] {
        let footprint = DisproportionFeatures {
            files,
            deleted_files: deleted,
            changed_kib: changed,
            declared_roots: 1,
            base_kib: base,
            files_per_root: files,
            delete_permille: ratio(u64::from(deleted) * 1000, u64::from(files)),
            growth_permille: base.map(|b| ratio(u64::from(changed) * 1000, u64::from(b))),
        };
        let signals = evaluate(&window, Some(footprint), &policy);
        assert_eq!(
            signals
                .iter()
                .find(|s| matches!(s.features, Features::Disproportion(_)))
                .map(|s| s.level),
            expected,
            "files {files} deleted {deleted} changed {changed}"
        );
    }
    let mut pinned = SupervisorPolicy::all_off();
    pinned.floors.files_per_root = 4;
    pinned.floors.delete_permille = 250;
    pinned.floors.deleted_files = 2;
    pinned.digest = pinned.computed_digest();
    assert!(pinned.valid());
    for (deleted_files, expected) in [(1, false), (2, true)] {
        let signals = evaluate(
            &window,
            Some(DisproportionFeatures {
                files: 4,
                files_per_root: 4,
                deleted_files,
                delete_permille: deleted_files * 250,
                ..Default::default()
            }),
            &pinned,
        );
        assert_eq!(
            signals
                .iter()
                .any(|s| matches!(s.features, Features::Disproportion(_))),
            expected
        );
    }
    assert_eq!(count(usize::MAX), u16::MAX);
    assert_eq!(ratio(u64::MAX, 0), u16::MAX);
    let window = (1..=32)
        .map(|i| {
            let mut s = step(i, 1, false);
            s.denied = true;
            s.alias = true;
            s
        })
        .collect();
    let signals = evaluate(
        &window,
        Some(DisproportionFeatures {
            files: 32,
            files_per_root: 32,
            ..Default::default()
        }),
        &policy,
    );
    let verdict = SupervisorVerdict {
        subject_sequence: 32,
        site: Site::CommandCapture,
        evaluator: 1,
        mode: Mode::Shadow,
        signals,
        capped: false,
    };
    assert!(verdict.valid());
    assert!(verdict.signals.iter().all(|s| s.evidence.len() <= 8));
    assert!(SupervisorPolicy::all_off().valid());
    assert!(evaluate(&window, None, &SupervisorPolicy::all_off()).is_empty());
}
#[test]
fn supervisor_call_equality_excludes_intent_identity_and_hashes_content() {
    let a = ToolCall::Write {
        path: "src/a".into(),
        content: "payload".into(),
        expected_digest: None,
    };
    let mut b = a.clone();
    assert_eq!(call_key(&a), call_key(&b));
    if let ToolCall::Write { content, .. } = &mut b {
        *content = "different".into();
    }
    assert_ne!(call_key(&a), call_key(&b));

    let fixture = crate::session::worker_protocol_fixture();
    let view = SessionView::replay(fixture.session, &fixture.envelopes())
        .unwrap()
        .unwrap();
    let mut envelope = fixture
        .envelopes()
        .into_iter()
        .find(|e| matches!(&e.event, Event::ToolFinished { observation, .. } if matches!(observation.intent.call, ToolCall::Write { .. })))
        .unwrap();
    let mut window = VecDeque::new();
    for n in 1..=3 {
        envelope.sequence = n;
        let Event::ToolFinished { observation, .. } = &mut envelope.event else {
            unreachable!()
        };
        observation.intent.call = a.clone();
        observation.intent.id = uuid::Uuid::now_v7();
        observation.intent.target = crate::worker::EffectTarget::Workspace;
        observation.disposition = ToolDisposition::Failed;
        observation.attestation = None;
        observation.output = "payload".into();
        let (site, step, footprint) = subject(&view.runs[0], fixture.child, &envelope, 1).unwrap();
        assert_eq!(site, Site::ToolOutcome);
        assert!(footprint.is_none());
        assert_eq!(step.call, call_key(&a));
        window.push_back(step);
    }
    let verdict = SupervisorVerdict {
        subject_sequence: 3,
        site: Site::ToolOutcome,
        evaluator: 1,
        mode: Mode::Shadow,
        signals: evaluate(&window, None, &SupervisorPolicy::default()),
        capped: false,
    };
    assert!(verdict.valid());
    assert_eq!(looping(&verdict.signals).unwrap().0.repeat_identical, 3);
    assert!(!serde_json::to_string(&verdict).unwrap().contains("payload"));
}

#[test]
fn supervisor_editor_command_outcomes_ignore_sequences_and_keep_the_exit_class() {
    let fixture = crate::session::worker_protocol_fixture();
    let view = SessionView::replay(fixture.session, &fixture.envelopes())
        .unwrap()
        .unwrap();
    let mut envelope = fixture
        .envelopes()
        .into_iter()
        .find(|e| matches!(&e.event, Event::ToolFinished { observation, .. } if matches!(observation.intent.call, ToolCall::Write { .. })))
        .unwrap();
    let mut outcome = |n: u64, exit_code: i32| {
        envelope.sequence = n;
        let Event::ToolFinished { observation, .. } = &mut envelope.event else {
            unreachable!()
        };
        observation.intent.call = ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            cwd: ".".into(),
            resource_class: None,
            timeout_ms: None,
        };
        observation.attestation = None;
        observation.disposition = ToolDisposition::Succeeded;
        observation.output = serde_json::to_string(&CommandResult {
            exit_code: Some(exit_code),
            stdout: String::new(),
            stderr: String::new(),
            stdout_total: Default::default(),
            stderr_total: Default::default(),
            changeset: Default::default(),
            selection: None,
            network_refusals: Vec::new(),
        })
        .unwrap();
        subject(&view.runs[0], fixture.child, &envelope, 1)
            .unwrap()
            .1
            .outcome
    };
    assert_eq!(outcome(10, 0), outcome(20, 0));
    assert_ne!(outcome(10, 0), outcome(10, 1));
}

#[test]
fn supervisor_footprint_uses_only_leased_base_and_evicts_whole_actor_steps() {
    let fixture = crate::session::worker_protocol_fixture();
    let mut view = SessionView::replay(fixture.session, &fixture.envelopes())
        .unwrap()
        .unwrap();
    let actor = fixture.child;
    let run = &mut view.runs[0];
    let worker = run
        .agents
        .iter_mut()
        .find(|a| a.id == actor)
        .unwrap()
        .assignment
        .as_mut()
        .unwrap();
    worker.pins.request.lease = vec!["src/owned".into()];
    let facts = vec![crate::session::worker::WorkerFileFact {
        path: "src/owned".into(),
        before: WorkerBaseVersion::Absent,
        after: WorkerBaseVersion::File {
            sha256: "c".repeat(64),
            bytes: 64 * 1024,
            executable: false,
        },
    }];
    let f = capture_features(&view.runs[0], actor, &facts, 1);
    assert_eq!(
        (f.base_kib, f.changed_kib, f.growth_permille),
        (None, 64, None)
    );
    assert_eq!((f.files, f.declared_roots, f.files_per_root), (1, 1, 1));
    let mut window: VecDeque<_> = (1..=32).map(|i| step(i, 1, true)).collect();
    window.pop_front();
    window.push_back(step(33, 2, true));
    assert!(evaluate(&window, None, &SupervisorPolicy::default()).is_empty());
}

#[test]
fn supervisor_worst_case_verdict_fits_its_byte_bound() {
    let max = u16::MAX;
    let evidence: Vec<u64> = (0..8).map(|n| u64::MAX - 7 + n).collect();
    let signal = |signal, features| Signal {
        signal,
        level: Level::WouldNarrow,
        would_response: Some(Response::RequireApproval),
        features,
        evidence: evidence.clone(),
    };
    let verdict = SupervisorVerdict {
        subject_sequence: u64::MAX,
        site: Site::CommandCapture,
        evaluator: 1,
        mode: Mode::Shadow,
        signals: vec![
            signal(
                SignalKind::Loop,
                Features::Loop(LoopFeatures {
                    repeat_identical: max,
                    repeat_failing: max,
                    cycle_reps: max,
                    cycle_len: max,
                    edit_oscillation: max,
                    stalled_calls: max,
                }),
            ),
            signal(
                SignalKind::Probing,
                Features::Probing(ProbingFeatures {
                    denial_streak: max,
                    denials_window: max,
                    distinct_denied: max,
                    alias_rejections: max,
                }),
            ),
            signal(
                SignalKind::Disproportion,
                Features::Disproportion(DisproportionFeatures {
                    files: max,
                    deleted_files: max,
                    changed_kib: max,
                    declared_roots: max,
                    base_kib: Some(max),
                    files_per_root: max,
                    delete_permille: max,
                    growth_permille: Some(max),
                }),
            ),
        ],
        capped: true,
    };
    assert!(verdict.valid());
}

#[test]
fn supervisor_roots_initialize_once_and_refresh_only_at_plan_and_run_boundaries() {
    let fixture = crate::session::worker_protocol_fixture();
    let envelopes = fixture.envelopes();
    let mut view = SessionView::replay_open(fixture.session, &envelopes)
        .unwrap()
        .unwrap();
    view.goals[0].plan_contract.as_mut().unwrap().paths = vec!["src".into(), "tests".into()];
    let mut live = SupervisorState::from_committed(&view);
    let mut corpus = SupervisorState::default();
    let started = envelopes
        .iter()
        .find(|e| matches!(e.event, Event::RunStarted { .. }))
        .unwrap();
    corpus.decide_verdict(&view, started);
    assert_eq!(live, corpus);
    assert_eq!(live.declared_roots, 2);
    view.goals[0]
        .plan_contract
        .as_mut()
        .unwrap()
        .paths
        .push("docs".into());
    for envelope in envelopes
        .iter()
        .filter(|e| matches!(e.event, Event::ToolFinished { .. }))
    {
        live.decide_verdict(&view, envelope);
        corpus.decide_verdict(&view, envelope);
        assert_eq!(live, corpus);
        assert_eq!(live.declared_roots, 2);
    }
    let plan = envelopes
        .iter()
        .find(|e| matches!(e.event, Event::GoalPlanContractRecorded { .. }))
        .unwrap();
    live.decide_verdict(&view, plan);
    corpus.decide_verdict(&view, plan);
    assert_eq!(live, corpus);
    assert_eq!(live.declared_roots, 3);
    live.decide_verdict(&view, started);
    assert_eq!(live, SupervisorState::from_committed(&view));
}

#[test]
fn supervisor_decision_table_keeps_negatives_cap_and_run_actor_scope() {
    let run = RunId::new();
    let actor = AgentRunId::new();
    for (prior, policy, records, capped) in [
        (0, SupervisorPolicy::default(), true, false),
        (MAX_VERDICTS - 1, SupervisorPolicy::default(), true, true),
        (MAX_VERDICTS, SupervisorPolicy::default(), false, false),
        (0, SupervisorPolicy::all_off(), false, false),
    ] {
        let mut state = SupervisorState {
            verdicts: prior,
            ..Default::default()
        };
        for sequence in 1..=3 {
            let decision = state.observe(
                run,
                actor,
                sequence,
                (Site::ToolOutcome, step(sequence, 1, true), None),
                &policy,
            );
            match decision {
                SupervisorDecision::Continue => assert!(sequence < 3 || !records),
                SupervisorDecision::Record {
                    run_id,
                    agent_run_id,
                    verdict,
                } => {
                    assert!(records);
                    assert_eq!((run_id, agent_run_id), (run, actor));
                    assert_eq!(verdict.subject_sequence, 3);
                    assert_eq!(verdict.capped, capped);
                    assert!(verdict.valid());
                }
            }
        }
    }
}

#[test]
fn supervisor_one_crossing_journals_one_verdict_not_a_stream() {
    let policy = SupervisorPolicy::default();
    let mut window = VecDeque::new();
    let mut journaled = Vec::new();
    for n in 1..=8 {
        let mut s = step(n, n as u8, true);
        if n == 2 {
            s.succeeded = false;
            s.denied = true;
            s.alias = true;
        }
        window.push_back(s);
        let signals = new_facts(evaluate(&window, None, &policy), n);
        if !signals.is_empty() {
            journaled.push(n);
        }
    }
    assert_eq!(journaled, [2]);
}

#[test]
fn primary_proposal_refusals_are_stalls_without_marking_account_rejections() {
    use crate::session::{ProposalRefusal, ProposalRefusalCause, ProviderCallDisposition};
    let fixture = crate::session::worker_protocol_fixture();
    let view = SessionView::replay(fixture.session, &fixture.envelopes())
        .unwrap()
        .unwrap();
    let run = &view.runs[0];
    let actor = run.primary().unwrap().id;
    let original = fixture.envelopes().into_iter().find(|envelope| matches!(
        &envelope.event, Event::ProviderCallRecorded { agent_run_id, .. } if *agent_run_id == actor
    )).unwrap();
    for (disposition, refused, stalled) in [
        (ProviderCallDisposition::InvalidResponse, false, true),
        (ProviderCallDisposition::Rejected, true, true),
        (ProviderCallDisposition::Rejected, false, false),
        (ProviderCallDisposition::Finished, false, false),
    ] {
        let mut envelope = original.clone();
        let Event::ProviderCallRecorded { record, .. } = &mut envelope.event else {
            unreachable!()
        };
        record.disposition = disposition;
        record.delegation = None;
        record.requested_tools = 0;
        record.failure_reason =
            refused.then_some(crate::provider::ProviderFailureReason::ProposalRefused);
        record.refusal = refused.then(|| ProposalRefusal {
            operation: "arany_read".into(),
            cause: ProposalRefusalCause::InvalidPath,
        });
        record.response_id = None;
        record.input_tokens = None;
        record.output_tokens = None;
        record.cache_read_input_tokens = None;
        record.cache_creation_input_tokens = None;
        record.wire_provenance = None;
        envelope.event.payload().expect("current call record");
        let boundary = subject(run, actor, &envelope, 1);
        assert_eq!(
            boundary.is_some(),
            stalled,
            "{disposition:?}, refused {refused}"
        );
        if let Some((site, mut step, footprint)) = boundary {
            assert_eq!(site, Site::Stall);
            assert!(step.stall && !step.observation);
            assert!(footprint.is_none());
            step.sequence = 1;
            let mut window = VecDeque::from([step.clone()]);
            step.sequence = 2;
            window.push_back(step);
            assert_eq!(
                looping(&evaluate(&window, None, &SupervisorPolicy::default()))
                    .unwrap()
                    .1,
                Level::Notice
            );
        }
    }
}
