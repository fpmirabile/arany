use super::*;
use crate::session::rewind::*;
use crate::session::{Checkpoint, CheckpointStatus};
use crate::tools::{CacheUpdate, ToolCall, ToolDisposition};

fn compiled_bytes(view: &SessionView, items: &[crate::provider::CompactionItem]) -> Vec<u8> {
    serde_json::to_vec(&crate::provider::compaction_input(
        &crate::provider::CompactionRequest {
            session_id: view.id,
            covered_run_id: view.runs[0].id,
            model: "test".into(),
            previous_summary: None,
            items: items.to_vec(),
            max_output_tokens: 1024,
        },
    ))
    .unwrap()
}
fn fixture() -> (worker_tests::Fixture, SessionView) {
    let f = worker_tests::completed_fixture();
    let view = SessionView::replay_open(f.session, &f.envelopes())
        .unwrap()
        .unwrap();
    (f, view)
}
fn target(view: &SessionView) -> CheckpointTarget {
    view.rewinds.checkpoints[0].clone()
}
pub(crate) fn started(view: &SessionView, target: CheckpointTarget) -> Event {
    let p = plan(view, &target).unwrap();
    let w = view.workspace_identity.unwrap();
    Event::WorkspaceRewindStarted {
        record: Box::new(RewindStarted {
            rewind_id: uuid::Uuid::now_v7(),
            target,
            head_sequence: view.last_sequence,
            prefix_digest: view.rewinds.prefix.digest(),
            plan_digest: p.digest(),
            file_count: p.files.len() as u32,
            restored_bytes: p.restored_bytes(),
            kept: Vec::new(),
            overwrite: Vec::new(),
            workspace_device: w.device,
            workspace_inode: w.inode,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            expires_at_ms: 10_000,
        }),
    }
}
pub(crate) fn finished(view: &SessionView, outcome: RewindOutcome) -> Event {
    let r = view.rewinds.operations.last().unwrap();
    let attested = matches!(outcome, RewindOutcome::Restored | RewindOutcome::Uncertain);
    Event::WorkspaceRewindFinished {
        record: Box::new(RewindFinished {
            rewind_id: r.started.rewind_id,
            outcome,
            attestation: attested.then_some(RewindAttestation {
                plan_digest: r.started.plan_digest,
                workspace_device: r.started.workspace_device,
                workspace_inode: r.started.workspace_inode,
                enforcement_digest: r.started.enforcement_digest,
                receipt_digest: [3; 32],
            }),
            conflicts: Vec::new(),
            conflict_count: 0,
            checkpoint: attested.then_some(Checkpoint {
                status: if r.files.iter().any(|f| f.expected_current.is_some()) {
                    CheckpointStatus::Captured
                } else {
                    CheckpointStatus::NotNeeded
                },
                reason: None,
                pruned_before_run: None,
            }),
        }),
    }
}
fn envelope(view: &SessionView, event: Event) -> EventEnvelope {
    let (run_id, agent_run_id) = event.scope();
    EventEnvelope {
        sequence: view.last_sequence + 1,
        session_id: view.id,
        run_id,
        agent_run_id,
        event,
        created_at_ms: 0,
    }
}
fn apply(view: &mut SessionView, event: Event) {
    assert_eq!(
        Event::decode(
            event.kind(),
            event.version(),
            &event.payload().unwrap(),
            event.scope().0,
            event.scope().1
        )
        .unwrap(),
        event
    );
    view.apply_committed(&envelope(view, event)).unwrap();
}
fn rejects(view: &SessionView, event: Event) {
    assert!(
        view.clone()
            .apply_committed(&envelope(view, event))
            .is_err()
    );
}

#[test]
fn rewind_replay_checks_binding_outcomes_undo_and_worker_evidence() {
    let (f, mut view) = fixture();
    assert!(view.runs[0].worker_history()[0].verified);
    let p = plan(&view, &target(&view)).unwrap();
    assert_eq!(p.files.len(), 1);
    assert_eq!(p.files[0].path, "src/a");
    assert_eq!(p.files[0].restore_to, None);
    let begin = started(&view, target(&view));
    for field in [
        "head",
        "prefix",
        "digest",
        "count",
        "bytes",
        "workspace",
        "target",
    ] {
        let Event::WorkspaceRewindStarted { mut record } = begin.clone() else {
            unreachable!()
        };
        match field {
            "head" => record.head_sequence -= 1,
            "prefix" => record.prefix_digest = [9; 32],
            "digest" => record.plan_digest = [9; 32],
            "count" => record.file_count += 1,
            "bytes" => record.restored_bytes += 1,
            "workspace" => record.workspace_inode += 1,
            _ => {
                record.target = CheckpointTarget::Run {
                    run_id: RunId::new(),
                    sequence: 1,
                }
            }
        }
        rejects(&view, Event::WorkspaceRewindStarted { record });
    }
    apply(&mut view, begin.clone());
    rejects(&view, begin);
    let end = finished(&view, RewindOutcome::Restored);
    for field in [
        "id",
        "plan",
        "workspace",
        "enforcement",
        "missing",
        "checkpoint",
    ] {
        let Event::WorkspaceRewindFinished { mut record } = end.clone() else {
            unreachable!()
        };
        match field {
            "id" => record.rewind_id = uuid::Uuid::now_v7(),
            "plan" => record.attestation.as_mut().unwrap().plan_digest = [9; 32],
            "workspace" => record.attestation.as_mut().unwrap().workspace_inode += 1,
            "enforcement" => record.attestation.as_mut().unwrap().enforcement_digest = [9; 32],
            "missing" => record.attestation = None,
            _ => record.checkpoint = None,
        }
        rejects(&view, Event::WorkspaceRewindFinished { record });
    }
    apply(&mut view, end.clone());
    rejects(&view, end);
    assert!(!view.runs[0].worker_history()[0].verified);
    assert!(!view.goals[0].success_criterion_satisfied);
    assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
    assert!(plan(&view, &target(&view)).unwrap().files.is_empty());
    let before_rewind = view.rewinds.checkpoints.last().unwrap().clone();
    let redo = plan(&view, &before_rewind).unwrap();
    assert_eq!(redo.files[0].restore_to, p.files[0].expected_current);
    assert_eq!(redo.files[0].expected_current, None);
    let begin_redo = started(&view, before_rewind);
    apply(&mut view, begin_redo);
    let end_redo = finished(&view, RewindOutcome::Restored);
    apply(&mut view, end_redo);
    assert_eq!(plan(&view, &target(&view)).unwrap().files, p.files);
    assert_eq!(
        view.goals[0].provider_calls,
        SessionView::replay(f.session, &f.envelopes())
            .unwrap()
            .unwrap()
            .goals[0]
            .provider_calls
    );
}

#[test]
fn rewind_prefix_sealing_and_uncertainty_block_resume_without_fabricating_a_finish() {
    for outcome in [
        None,
        Some(RewindOutcome::Uncertain),
        Some(RewindOutcome::Conflict),
        Some(RewindOutcome::Denied),
        Some(RewindOutcome::Failed),
        Some(RewindOutcome::Cancelled),
        Some(RewindOutcome::Restored),
    ] {
        let (f, mut view) = fixture();
        let begin = started(&view, target(&view));
        apply(&mut view, begin);
        if let Some(outcome) = outcome {
            let end = finished(&view, outcome);
            apply(&mut view, end);
        }
        let unknown = outcome.is_none() || outcome == Some(RewindOutcome::Uncertain);
        assert_eq!(
            !plan(&view, &target(&view)).unwrap().blockers.is_empty(),
            unknown
        );
        assert_eq!(view.goals[0].can_resume(&view), !unknown);
        if unknown {
            rejects(&view, Event::GoalResumed { goal_id: f.goal });
        }
        let mut exhausted = view.clone();
        exhausted.goals[0].status = GoalStatus::Failed;
        exhausted.goals[0].budget_closed = true;
        assert_eq!(exhausted.continuable_goal().is_none(), unknown);
        if outcome.is_none() {
            let end = finished(&view, RewindOutcome::Restored);
            apply(
                &mut view,
                Event::SessionRenamed {
                    title: "later".into(),
                },
            );
            assert!(view.rewinds.operations[0].sealed);
            assert!(view.rewinds.operations[0].finished.is_none());
            rejects(&view, end);
            let mut later = view.runs[0].clone();
            later.id = RunId::new();
            later.accepted_sequence = view.last_sequence + 1;
            let checkpoint = CheckpointTarget::Run {
                run_id: later.id,
                sequence: view.last_sequence + 2,
            };
            for tool in &mut later.tools {
                if let Some(seq) = &mut tool.finished_sequence {
                    *seq += view.last_sequence;
                }
            }
            view.last_sequence += view.runs[0].finished_sequence.unwrap() + 2;
            view.rewinds.checkpoints.push(checkpoint.clone());
            view.runs.push(later);
            assert!(plan(&view, &checkpoint).unwrap().blockers.is_empty());
            let next = started(&view, checkpoint);
            apply(&mut view, next);
        }
    }
}

#[test]
fn rewind_planner_folds_runs_gaps_exec_bits_deletes_and_inherited_prefix_only() {
    let (_, mut view) = fixture();
    let source = view.runs[0]
        .tools
        .iter()
        .find(|t| matches!(t.intent.call, ToolCall::ApplyChangeset { .. }))
        .unwrap()
        .clone();
    let version = |text: &[u8], exec| crate::tools::FileVersion::from_bytes(text, exec);
    let mut run = view.runs[0].clone();
    run.id = RunId::new();
    run.accepted_sequence = view.last_sequence + 1;
    let new_start = view.last_sequence + 2;
    view.rewinds.checkpoints.push(CheckpointTarget::Run {
        run_id: run.id,
        sequence: new_start,
    });
    run.tools.clear();
    for (i, (before, after)) in [
        (Some(version(b"new", false)), Some(version(b"middle", true))),
        (Some(version(b"external", false)), None),
    ]
    .into_iter()
    .enumerate()
    {
        let mut t = source.clone();
        let changeset = crate::tools::CommandChangeset {
            files: vec![crate::tools::FileChange {
                path: "src/a".into(),
                before,
                after,
            }],
        };
        t.intent.call = ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest: [4; 32],
                changeset,
            },
        };
        t.observation.as_mut().unwrap().intent = t.intent.clone();
        t.finished_sequence = Some(new_start + 1 + i as u64);
        t.checkpoint = Some(Checkpoint {
            status: CheckpointStatus::Captured,
            reason: None,
            pruned_before_run: None,
        });
        run.tools.push(t);
    }
    view.last_sequence = new_start + 3;
    view.runs.push(run);
    let p = plan(&view, &view.rewinds.checkpoints[1]).unwrap();
    assert_eq!(
        p.files[0].restore_to.as_ref().unwrap().sha256,
        crate::tools::types::hex_digest(b"new")
    );
    assert_eq!(p.files[0].expected_current, None);
    assert!(p.files[0].history_drift);
    // The original create followed by a deletion is a net no-op.
    assert!(plan(&view, &target(&view)).unwrap().files.is_empty());
    let mut prefix = view.clone();
    prefix.runs.pop();
    prefix.rewinds.checkpoints.pop();
    prefix.last_sequence = new_start - 2;
    let inherited = ForkInheritance {
        usage: prefix.usage.clone(),
        workspace_identity: prefix.workspace_identity,
        runs: prefix.runs.clone(),
        compactions: Vec::new(),
        rewinds: prefix.rewinds.clone(),
    };
    let id = SessionId::new();
    let fork = Event::SessionForked {
        title: "Forked Session".into(),
        source_session_id: prefix.id,
        source_run_id: prefix.runs[0].id,
        source_sequence: prefix.last_sequence,
        prefix_digest: [1; 32],
    };
    let env = EventEnvelope {
        sequence: view.last_sequence + 1,
        session_id: id,
        run_id: None,
        agent_run_id: None,
        event: fork,
        created_at_ms: 0,
    };
    let child = SessionView::replay_forked(id, &[env], true, &inherited)
        .unwrap()
        .unwrap();
    assert_eq!(
        plan(&child, &target(&child)).unwrap(),
        plan(&prefix, &target(&prefix)).unwrap()
    );
    let t = view.runs[1].tools.last_mut().unwrap();
    t.observation = None;
    assert!(
        !plan(&view, &view.rewinds.checkpoints[1])
            .unwrap()
            .blockers
            .is_empty()
    );
    let t = view.runs[1].tools.last_mut().unwrap();
    t.observation = source.observation.clone();
    t.observation.as_mut().unwrap().disposition = ToolDisposition::Uncertain;
    assert!(
        !plan(&view, &view.rewinds.checkpoints[1])
            .unwrap()
            .blockers
            .is_empty()
    );
}

#[test]
fn rewind_pruned_first_preimages_reject_start_but_can_be_kept() {
    let (_, mut view) = fixture();
    let source = view.runs[0]
        .tools
        .iter()
        .find(|t| matches!(t.intent.call, ToolCall::ApplyChangeset { .. }))
        .unwrap()
        .clone();
    for (before, after) in [
        (b"old".as_slice(), b"middle".as_slice()),
        (b"middle", b"new"),
    ] {
        let mut run = view.runs[0].clone();
        run.id = RunId::new();
        run.accepted_sequence = view.last_sequence + 1;
        let sequence = view.last_sequence + 2;
        view.rewinds.checkpoints.push(CheckpointTarget::Run {
            run_id: run.id,
            sequence,
        });
        let mut tool = source.clone();
        tool.intent.run_id = run.id;
        tool.intent.call = ToolCall::ApplyChangeset {
            source: crate::session::worker::ChangesetSource::CommandReceipt {
                source_intent_digest: [4; 32],
                changeset: crate::tools::CommandChangeset {
                    files: vec![crate::tools::FileChange {
                        path: "src/a".into(),
                        before: Some(crate::tools::FileVersion::from_bytes(before, false)),
                        after: Some(crate::tools::FileVersion::from_bytes(after, false)),
                    }],
                },
            },
        };
        tool.observation.as_mut().unwrap().intent = tool.intent.clone();
        tool.finished_sequence = Some(sequence + 1);
        tool.checkpoint = Some(Checkpoint {
            status: CheckpointStatus::Captured,
            reason: None,
            pruned_before_run: None,
        });
        run.tools = vec![tool];
        view.last_sequence = sequence + 2;
        view.runs.push(run);
    }
    let target = view.rewinds.checkpoints[1].clone();
    assert!(!plan(&view, &target).unwrap().files[0].unavailable);
    let pruned = view.runs[1].id;
    view.runs[2].tools[0]
        .checkpoint
        .as_mut()
        .unwrap()
        .pruned_before_run = Some(pruned);
    let p = plan(&view, &target).unwrap();
    assert!(p.files[0].unavailable);
    rejects(&view, started(&view, target.clone()));
    let overwrite = Overwrite {
        path: "src/a".into(),
        observed: p.files[0].expected_current.clone(),
    };
    assert!(p.select(&[], &[overwrite]).unwrap().files[0].unavailable);
    assert!(p.select(&["src/a".into()], &[]).unwrap().files.is_empty());
    assert!(!plan(&view, &view.rewinds.checkpoints[2]).unwrap().files[0].unavailable);
    // A failed publication can still carry an eviction notice from capture.
    let mut failed = view.runs[2].tools[0].clone();
    failed.observation.as_mut().unwrap().disposition = ToolDisposition::Failed;
    failed.checkpoint.as_mut().unwrap().status = CheckpointStatus::NotNeeded;
    view.runs[2].tools[0]
        .checkpoint
        .as_mut()
        .unwrap()
        .pruned_before_run = None;
    view.runs[2].tools.push(failed);
    assert!(plan(&view, &target).unwrap().files[0].unavailable);
}

#[test]
fn rewind_plan_selection_and_payload_bounds_are_exact_and_closed() {
    let (_, view) = fixture();
    let p = plan(&view, &target(&view)).unwrap();
    let file = p.files[0].clone();
    for n in [64, 65] {
        let mut p = p.clone();
        p.files = (0..n)
            .map(|i| {
                let mut f = file.clone();
                f.path = format!("src/{i:03}");
                f
            })
            .collect();
        let kept: Vec<_> = p.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(p.select(&kept, &[]).is_ok(), n == 64);
    }
    assert!(p.select(&["missing".into()], &[]).is_err());
    assert!(
        p.select(
            std::slice::from_ref(&file.path),
            &[Overwrite {
                path: file.path.clone(),
                observed: None
            }]
        )
        .is_err()
    );
    let mut drift = p.clone();
    drift.files[0].history_drift = true;
    let selected = drift
        .select(
            &[],
            &[Overwrite {
                path: file.path,
                observed: None,
            }],
        )
        .unwrap();
    assert!(!selected.files[0].history_drift);
    assert_eq!(selected.files[0].expected_current, None);
    let event = started(&view, target(&view));
    let payload = event.payload().unwrap();
    let exact = format!(
        "{payload}{}",
        " ".repeat(MAX_REWIND_EVENT_BYTES - payload.len())
    );
    assert!(Event::decode(event.kind(), 1, &exact, None, None).is_ok());
    assert!(matches!(
        Event::decode(event.kind(), 1, &(exact + " "), None, None),
        Err(ReplayError::InvalidPayload)
    ));
    assert!(matches!(
        Event::decode(event.kind(), 1, &payload, Some(view.runs[0].id), None),
        Err(ReplayError::InvalidPayload)
    ));
    assert!(matches!(
        Event::decode(event.kind(), 1, &payload, None, Some(AgentRunId::new())),
        Err(ReplayError::InvalidPayload)
    ));
    let Event::WorkspaceRewindStarted { mut record } = event.clone() else {
        unreachable!()
    };
    record.kept = (0..64)
        .map(|i| format!("{i:02}{}", "\"".repeat(510)))
        .collect();
    assert!(serde_json::to_vec(&record).unwrap().len() > MAX_REWIND_EVENT_BYTES);
    assert!(!record.valid());
    for version in [0, 2] {
        assert!(matches!(
            Event::decode(event.kind(), version, &payload, None, None),
            Err(ReplayError::UnknownEvent)
        ));
    }
    for invalid in [
        payload.replacen("\"kept\":[]", "\"kept\":[],\"kept\":[]", 1),
        payload.replacen("\"kept\":[]", "\"unknown\":[]", 1),
        format!("{} ", " ".repeat(MAX_REWIND_EVENT_BYTES)),
    ] {
        assert!(Event::decode(event.kind(), 1, &invalid, None, None).is_err());
    }
    let mut open = view.clone();
    apply(&mut open, event);
    let event = finished(&open, RewindOutcome::Restored);
    let payload = event.payload().unwrap();
    for (run, agent, bytes) in [
        (Some(view.runs[0].id), None, payload.clone()),
        (None, Some(AgentRunId::new()), payload.clone()),
        (None, None, " ".repeat(MAX_REWIND_EVENT_BYTES + 1)),
    ] {
        assert!(matches!(
            Event::decode(event.kind(), 1, &bytes, run, agent),
            Err(ReplayError::InvalidPayload)
        ));
    }
    for field in ["checkpoint", "attestation"] {
        let mut v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        v.as_object_mut().unwrap().remove(field);
        assert!(Event::decode(event.kind(), 1, &v.to_string(), None, None).is_err());
    }
    assert!(Event::decode(event.kind(), 0, &payload, None, None).is_err());
    let mut r = match event {
        Event::WorkspaceRewindFinished { record } => record,
        _ => unreachable!(),
    };
    r.outcome = RewindOutcome::Conflict;
    r.attestation = None;
    r.checkpoint = None;
    for n in [32, 33] {
        r.conflicts = (0..n).map(|i| format!("src/{i:02}")).collect();
        r.conflict_count = n;
        assert_eq!(r.valid(), n == 32);
    }
}

#[test]
fn rewind_compiler_preserves_histories_without_rewinds_byte_for_byte() {
    let (_, mut view) = fixture();
    let mut plain = view.clone();
    plain.runs[0].objective = "Task".into();
    plain.runs[0].tools.clear();
    for a in &mut plain.runs[0].agents {
        a.assignment = None;
    }
    let compiled = crate::session::PreparedCompaction::prepare(&plain)
        .unwrap_or_else(|_| panic!("plain history"));
    assert_eq!(compiled_bytes(&plain, &compiled.items), br#"{"items":[{"assistant":"done","status":"completed","user":"Task"}],"previous_derived_summary":null}"#);
    let baseline = crate::session::PreparedCompaction::prepare(&view)
        .unwrap_or_else(|_| panic!("complete history"));
    let bytes = compiled_bytes(&view, &baseline.items);
    assert_eq!(baseline.compiler_version, 3);
    assert!(
        !String::from_utf8(bytes.clone())
            .unwrap()
            .contains("Workspace rewind")
    );
    let again = crate::session::PreparedCompaction::prepare(&view)
        .unwrap_or_else(|_| panic!("complete history"));
    assert_eq!(compiled_bytes(&view, &again.items), bytes);
    let event = started(&view, target(&view));
    apply(&mut view, event);
    let event = finished(&view, RewindOutcome::Restored);
    apply(&mut view, event);
    let with = crate::session::PreparedCompaction::prepare(&view)
        .unwrap_or_else(|_| panic!("rewound history"));
    assert_eq!(with.compiler_version, 3);
    assert!(
        String::from_utf8(compiled_bytes(&view, &with.items))
            .unwrap()
            .contains("Workspace rewind")
    );
    assert!(crate::session::rewind::history_facts(&view, 0, view.last_sequence).is_some());
    assert_eq!(
        view.runs[0]
            .tools
            .last()
            .unwrap()
            .observation
            .as_ref()
            .unwrap()
            .cache_update,
        CacheUpdate::NotApplicable
    );
}

#[test]
fn rewind_planner_refuses_first_file_and_byte_overflow_and_preserves_exec_only_changes() {
    use crate::session::worker::{WorkerBaseVersion, WorkerDataFacts, WorkerFileFact};
    for (files, bytes, admitted) in [
        (2048, 1, true),
        (2049, 1, false),
        (32, 1024 * 1024, true),
        (33, 1024 * 1024, false),
        (1, 1024 * 1024 + 1, false),
    ] {
        let (_, mut view) = fixture();
        for changes in view.runs[0]
            .workers
            .data
            .iter_mut()
            .filter(|d| matches!(d.facts, WorkerDataFacts::Changes(_)))
        {
            changes.facts = WorkerDataFacts::Changes(
                (0..files)
                    .map(|i| WorkerFileFact {
                        path: format!("src/{i:04}"),
                        before: WorkerBaseVersion::File {
                            sha256: "a".repeat(64),
                            bytes,
                            executable: true,
                        },
                        after: WorkerBaseVersion::Absent,
                    })
                    .collect(),
            );
        }
        let result = plan(&view, &target(&view));
        assert_eq!(result.is_ok(), admitted, "{files} files, {bytes} bytes");
        if admitted {
            let p = result.unwrap();
            assert_eq!(p.files.len(), files);
            assert_eq!(p.restored_bytes(), files as u64 * u64::from(bytes));
        }
    }
    let (_, mut view) = fixture();
    for changes in &mut view.runs[0].workers.data {
        if let WorkerDataFacts::Changes(files) = &mut changes.facts {
            for f in files {
                f.before = WorkerBaseVersion::File {
                    sha256: "a".repeat(64),
                    bytes: 3,
                    executable: true,
                };
                f.after = WorkerBaseVersion::File {
                    sha256: "a".repeat(64),
                    bytes: 3,
                    executable: false,
                };
            }
        }
    }
    let p = plan(&view, &target(&view)).unwrap();
    assert!(p.files[0].restore_to.as_ref().unwrap().executable);
    assert!(!p.files[0].expected_current.as_ref().unwrap().executable);
}

#[test]
fn rewind_compaction_binds_the_new_boundary_and_rejects_stale_prefixes() {
    let (f, mut view) = fixture();
    let mut events = f.envelopes();
    let event = started(&view, target(&view));
    events.push(envelope(&view, event.clone()));
    apply(&mut view, event);
    let event = finished(&view, RewindOutcome::Restored);
    events.push(envelope(&view, event.clone()));
    apply(&mut view, event);
    let record = crate::session::CompactionRecord {
        covered_run_id: f.run,
        covered_sequence: view.last_sequence,
        source_digest: crate::session::prefix_digest(&events, view.last_sequence).unwrap(),
        provider: "scripted".into(),
        model: "test".into(),
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
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        status: crate::session::CompactionStatus::Failed {
            reason: crate::session::CompactionFailure::ProviderUnavailable,
        },
    };
    let mut stale = record.clone();
    stale.covered_sequence = view.runs[0].finished_sequence.unwrap();
    rejects(
        &view,
        Event::ContextCompacted {
            record: stale,
            active_ms: None,
        },
    );
    let event = Event::ContextCompacted {
        record: record.clone(),
        active_ms: None,
    };
    events.push(envelope(&view, event.clone()));
    apply(&mut view, event);
    assert_eq!(
        SessionView::replay(f.session, &events).unwrap().unwrap(),
        view
    );
    let Event::ContextCompacted { record, .. } = &mut events.last_mut().unwrap().event else {
        unreachable!()
    };
    record.source_digest = [9; 32];
    assert!(SessionView::replay(f.session, &events).is_err());
}

#[test]
fn rewind_reverse_changeset_transfer_has_an_exact_48_mebibyte_boundary() {
    let actual = crate::session::worker::WorkerChangeset {
        files: vec![crate::tools::FileChange {
            path: "src/\"quoted".into(),
            before: Some(crate::tools::FileVersion::from_bytes(b"old", true)),
            after: Some(crate::tools::FileVersion::from_bytes(b"new!", false)),
        }],
    };
    let encoded = RewindPlan {
        files: vec![FilePlan {
            path: actual.files[0].path.clone(),
            expected_current: Some(FileState {
                sha256: crate::tools::types::hex_digest(b"old"),
                bytes: 3,
                executable: true,
            }),
            restore_to: Some(FileState {
                sha256: crate::tools::types::hex_digest(b"new!"),
                bytes: 4,
                executable: false,
            }),
            history_drift: false,
            unavailable: false,
        }],
        blockers: Vec::new(),
        non_rewindable: Vec::new(),
    };
    assert_eq!(
        encoded.transfer_bytes(),
        serde_json::to_vec(&actual).unwrap().len() as u64
    );
    let state = FileState {
        sha256: "a".repeat(64),
        bytes: 1024 * 1024,
        executable: false,
    };
    let mut p = RewindPlan {
        files: (0..18)
            .map(|i| FilePlan {
                path: format!("src/{i:02}"),
                restore_to: Some(state.clone()),
                expected_current: Some(state.clone()),
                history_drift: false,
                unavailable: false,
            })
            .collect(),
        blockers: Vec::new(),
        non_rewindable: Vec::new(),
    };
    let limit = crate::session::worker::WORKER_DATA_BYTES as u64;
    while p.transfer_bytes() % 4 != limit % 4 {
        p.files[0].path.push('x');
    }
    let excess = (p.transfer_bytes() - limit).div_ceil(4);
    p.files[0].expected_current.as_mut().unwrap().bytes -= (excess * 3) as u32;
    assert_eq!(p.transfer_bytes(), limit);
    assert!(p.select(&[], &[]).is_ok());
    p.files[0].expected_current.as_mut().unwrap().bytes += 3;
    assert_eq!(p.transfer_bytes(), limit + 4);
    assert!(p.select(&[], &[]).is_err());
}

pub(crate) fn restored_operation() -> RewindView {
    let (_, mut view) = fixture();
    let begin = started(&view, target(&view));
    apply(&mut view, begin);
    let end = finished(&view, RewindOutcome::Restored);
    apply(&mut view, end);
    view.rewinds.operations.pop().unwrap()
}
