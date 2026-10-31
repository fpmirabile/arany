use super::super::test_support::*;
use super::super::*;
use crate::provider::AgentPhase;
use crate::tools::ToolCall;

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
                expected_digest: Some("a".repeat(64)),
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

fn foreground_primary_tool_session() -> (SessionView, GoalId, RunId, AgentRunId, AgentRunId) {
    let mut fixture = super::super::worker_tests::completed_fixture();
    let finished = fixture.events.iter().position(|event| matches!(event,
        Event::ProviderCallRecorded { agent_run_id, record, .. }
            if *agent_run_id == fixture.primary && record.disposition == ProviderCallDisposition::Finished
    )).unwrap();
    fixture.events.truncate(finished - 1);
    let view = SessionView::replay_open(fixture.session, &fixture.envelopes())
        .unwrap()
        .unwrap();
    (
        view,
        fixture.goal,
        fixture.run,
        fixture.primary,
        fixture.child,
    )
}

#[test]
fn primary_compaction_charges_itself_without_restarting_a_finished_editor() {
    let (mut view, goal, run, primary, child) = foreground_primary_tool_session();
    let calls = view.goals[0].provider_calls;
    apply(
        &mut view,
        Event::GoalCompactionCallReserved {
            goal_id: goal,
            run_id: run,
            active_ceiling_ms: 0,
        },
    )
    .unwrap();
    assert_eq!(view.goals[0].provider_calls, calls + 1);
    let primary_call = reserve_child_call(goal, run, primary);
    let child_call = reserve_child_call(goal, run, child);
    assert!(rejected(&view, primary_call.clone()));
    assert!(rejected(&view, child_call.clone()));
    assert_eq!(view.goals[0].provider_calls, calls + 1);
    assert_eq!(view.goals[0].compaction_call_reserved, Some(run));
    assert!(view.goals[0].unobserved_provider_calls.is_empty());
    let count = u32::try_from(view.runs[0].tools.len()).unwrap();
    let compacted = covering_compaction(&view, goal, run, count);
    apply(&mut view, compacted).expect("primary compaction settles independently");
    assert_eq!(view.goals[0].compaction_call_reserved, None);
    assert!(view.goals[0].unobserved_provider_calls.is_empty());
    let mut exhausted = view.clone();
    exhausted.goals[0]
        .plan_contract
        .as_mut()
        .unwrap()
        .budget
        .max_provider_calls = Some(calls + 1);
    assert!(rejected(&exhausted, primary_call.clone()));
    assert!(rejected(&exhausted, child_call.clone()));
    assert!(rejected(&view, child_call));
    apply(&mut view, primary_call).expect("primary resumes after its compaction");
    assert_eq!(view.goals[0].provider_calls, calls + 2);
    assert_eq!(view.goals[0].unobserved_provider_calls, [primary]);
}

#[test]
fn primary_compaction_rejects_background_deferrals_and_accepts_proposal_refusals() {
    use crate::provider::ProviderFailureReason;

    for (reason, admitted, continuable) in [
        (
            Some(ProviderFailureReason::WorkerAnswerDeferred),
            false,
            false,
        ),
        (
            Some(ProviderFailureReason::WorkerDelegateRefused),
            false,
            false,
        ),
        (Some(ProviderFailureReason::WorkerToolRefused), false, false),
        (Some(ProviderFailureReason::ProposalRefused), true, true),
        (None, true, false),
    ] {
        let (mut view, goal, run, primary, child) = foreground_primary_tool_session();
        assert!(rejected(&view, reserve_child_call(goal, run, child)));
        apply(&mut view, reserve_child_call(goal, run, primary)).unwrap();
        let mut deferred = record(
            AgentPhase::RootSynthesis,
            ProviderCallDisposition::InvalidResponse,
            Some(0),
        );
        deferred.failure_reason = reason;
        if reason == Some(ProviderFailureReason::ProposalRefused) {
            deferred.disposition = ProviderCallDisposition::Rejected;
            deferred.refusal = Some(crate::session::ProposalRefusal {
                operation: "arany_read".into(),
                cause: crate::session::ProposalRefusalCause::InvalidArguments,
            });
        }
        let event = Event::ProviderCallRecorded {
            run_id: run,
            agent_run_id: primary,
            record: deferred,
        };
        let reservation = Event::GoalCompactionCallReserved {
            goal_id: goal,
            run_id: run,
            active_ceiling_ms: 0,
        };
        if !admitted {
            assert!(rejected(&view, event), "{reason:?}");
            assert!(rejected(&view, reservation));
            assert_eq!(view.goals[0].unobserved_provider_calls, [primary]);
            continue;
        }
        apply(&mut view, event).expect("foreground Provider outcome");
        assert_eq!(
            apply(&mut view, reservation).is_ok(),
            continuable,
            "{reason:?}"
        );
        if continuable {
            let count = u32::try_from(view.runs[0].tools.len()).unwrap();
            let compacted = covering_compaction(&view, goal, run, count);
            apply(&mut view, compacted).expect("compaction settles after a proposal refusal");
            assert_eq!(view.goals[0].compaction_call_reserved, None);
            assert!(view.goals[0].unobserved_provider_calls.is_empty());
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
    let Event::GoalContextCompacted { mut record, .. } = covering_compaction(&view, goal, run, 2)
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
        summary_bytes: u32::try_from(summary.len()).unwrap(),
        summary_tokens_estimate: u32::try_from(summary.len().div_ceil(4)).unwrap(),
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
    let Event::GoalContextCompacted { mut record, .. } = covering_compaction(&view, goal, run, 2)
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
    let finished = |agent, path| tool_finished(run, agent, &read_intent(run, agent, path), b"x");
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
