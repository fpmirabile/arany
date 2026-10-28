use super::*;

fn multiedit() -> arany::ToolCall {
    arany::ToolCall::MultiEdit {
        file_path: "notes/facts.txt".into(),
        expected_digest: hash(b"Fact"),
        edits: vec![
            arany::LiteralEdit {
                old_string: "Fact".into(),
                new_string: "intermediate".into(),
                replace_all: false,
            },
            arany::LiteralEdit {
                old_string: "intermediate".into(),
                new_string: "done".into(),
                replace_all: false,
            },
        ],
    }
}

fn removal(move_file: bool) -> arany::ToolCall {
    if move_file {
        arany::ToolCall::Move {
            source: "notes/facts.txt".into(),
            destination: "notes/nested/moved.txt".into(),
            expected_digest: hash(b"Fact"),
        }
    } else {
        arany::ToolCall::Delete {
            path: "notes/facts.txt".into(),
            expected_digest: hash(b"Fact"),
        }
    }
}

#[tokio::test]
async fn delete_and_move_guard_publication_permissions_checkpoints_and_replay() {
    for move_file in [false, true] {
        for case in [
            "success",
            "stale",
            "read_deny",
            "destination_deny",
            "hook_deny",
            "grant",
        ] {
            if case == "destination_deny" && !move_file {
                continue;
            }
            removal_journey(move_file, case).await;
        }
    }
}

async fn removal_journey(move_file: bool, case: &str) {
    let mut settings = json!({"permissions":{"defaultMode":"acceptEdits"}});
    if case == "read_deny" {
        settings["permissions"]["deny"] = json!(["Read(./notes/facts.txt)"]);
    }
    if case == "destination_deny" {
        settings["permissions"]["deny"] = json!(["Edit(./notes/nested/**)"]);
    }
    if case == "hook_deny" {
        settings["hooks"] =
            json!({"PreToolUse":[{"matcher":"Edit","hooks":[shell_hook("exit 2")]}]});
    }
    let fixture = Fixture::new(Some(&settings), case != "grant");
    let mut call = removal(move_file);
    if case == "stale" {
        match &mut call {
            arany::ToolCall::Delete {
                expected_digest, ..
            }
            | arany::ToolCall::Move {
                expected_digest, ..
            } => *expected_digest = hash(b"stale"),
            _ => unreachable!(),
        }
    }
    let success = case == "success";
    let (result, requests, humans) = fixture
        .run(
            if success {
                vec![
                    plan_outcome(plan(&call)),
                    tool_outcome(call.clone()),
                    finish("answer"),
                ]
            } else {
                vec![tool_outcome(call.clone()), finish("answer")]
            },
            None,
            false,
        )
        .await;
    assert_eq!(humans, 0);
    assert_eq!(requests.len(), if success { 3 } else { 2 });
    assert_eq!(fixture.workspace.join("notes/facts.txt").exists(), !success);
    assert_eq!(
        fixture.workspace.join("notes/nested/moved.txt").exists(),
        success && move_file
    );
    if success && move_file {
        assert_eq!(
            std::fs::read(fixture.workspace.join("notes/nested/moved.txt")).unwrap(),
            b"Fact"
        );
    }
    let tool = &result.run.tools[0];
    assert_eq!(tool.intent.call, call);
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        match case {
            "success" => arany::ToolDisposition::Succeeded,
            "stale" => arany::ToolDisposition::Conflict,
            _ => arany::ToolDisposition::Denied,
        }
    );
    if success {
        rewind_removal(&fixture, &result, move_file).await;
    }
    let (events, view) = fixture.reopen(result.session_id).await;
    if success {
        assert!(!view.goals[0].success_criterion_satisfied);
    }
    assert_removal_export(&view, &events);
}

async fn rewind_removal(fixture: &Fixture, result: &arany::RunOutcome, move_file: bool) {
    assert_eq!(
        result.run.tools[0].checkpoint.as_ref().unwrap().status,
        arany::CheckpointStatus::Captured
    );
    let store = Store::open_read_only(StateRoot::open_existing(&fixture.state).unwrap()).unwrap();
    let events = store.load_session(result.session_id).await.unwrap();
    store.close().await.unwrap();
    let view = SessionView::replay(result.session_id, &events)
        .unwrap()
        .unwrap();
    assert!(view.goals[0].success_criterion_satisfied);
    let sequence = events
        .iter()
        .find(|e| matches!(e.event, Event::RunStarted { .. }))
        .unwrap()
        .sequence;
    let preview = fixture
        .engine
        .rewind_preview(arany::RewindRequest {
            session_id: result.session_id,
            workspace: fixture.workspace.clone(),
            target: arany::rewind::CheckpointTarget::Run {
                run_id: result.run.id,
                sequence,
            },
            tools: ToolGrant::Configured,
            kept: vec![],
            overwrite: vec![],
        })
        .await
        .unwrap();
    assert!(preview.can_restore());
    assert_eq!(preview.files().len(), if move_file { 2 } else { 1 });
    let restored = fixture
        .engine
        .rewind(preview, RunCancellation::new())
        .await
        .unwrap();
    assert_eq!(restored.outcome, arany::rewind::RewindOutcome::Restored);
    assert_eq!(
        std::fs::read(fixture.workspace.join("notes/facts.txt")).unwrap(),
        b"Fact"
    );
    assert!(!fixture.workspace.join("notes/nested/moved.txt").exists());
}

fn assert_removal_export(view: &SessionView, events: &[arany::EventEnvelope]) {
    let jsonl = render_session(view, events, Output::Jsonl);
    let exported: Vec<arany_cli::protocol::EventLine> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        exported,
        events
            .iter()
            .map(arany_cli::protocol::EventLine::from)
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn multiedit_guard_publication_denials_late_failure_and_closed_replay() {
    for case in [
        "success",
        "late_failure",
        "stale",
        "read_deny",
        "hook_deny",
        "grant",
    ] {
        let mut settings = json!({"permissions":{"defaultMode":"acceptEdits"}});
        if case == "read_deny" {
            settings["permissions"]["deny"] = json!(["Read(./notes/**)"]);
        }
        if case == "hook_deny" {
            settings["hooks"] =
                json!({"PreToolUse":[{"matcher":"MultiEdit","hooks":[shell_hook("exit 2")]}]});
        }
        let fixture = Fixture::new(Some(&settings), case != "grant");
        let mut call = multiedit();
        if let arany::ToolCall::MultiEdit {
            expected_digest,
            edits,
            ..
        } = &mut call
        {
            if case == "late_failure" {
                edits[1].old_string = "missing".into();
            }
            if case == "stale" {
                *expected_digest = hash(b"stale");
            }
        }
        let outcomes = if case == "success" {
            vec![
                plan_outcome(plan(&call)),
                tool_outcome(call.clone()),
                finish("answer"),
            ]
        } else {
            vec![tool_outcome(call.clone()), finish("answer")]
        };
        let (result, requests, humans) = fixture.run(outcomes, None, false).await;
        assert_eq!(humans, 0, "{case}");
        assert_eq!(
            std::fs::read(fixture.workspace.join("notes/facts.txt")).unwrap(),
            if case == "success" {
                b"done".as_slice()
            } else {
                b"Fact".as_slice()
            },
            "{case}"
        );
        let (events, view) = fixture.reopen(result.session_id).await;
        assert_eq!(
            requests.len(),
            if case == "success" { 3 } else { 2 },
            "{case}"
        );
        if case == "grant" {
            let observed = view.runs[0].tools[0].observation.as_ref().unwrap();
            assert_eq!(observed.disposition, arany::ToolDisposition::Denied);
            assert_eq!(
                observed.output,
                "Tool operation is outside the pinned grant"
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e.event, Event::ToolStarted { .. }))
                    .count(),
                1
            );
            continue;
        }
        let tool = &view.runs[0].tools[0];
        assert_eq!(tool.intent.call, call);
        let observed = tool.observation.as_ref().unwrap();
        assert_eq!(
            observed.disposition,
            match case {
                "success" => arany::ToolDisposition::Succeeded,
                "late_failure" | "stale" => arany::ToolDisposition::Conflict,
                _ => arany::ToolDisposition::Denied,
            },
            "{case}"
        );
        if case == "success" {
            assert_eq!(
                tool.checkpoint.as_ref().unwrap().status,
                arany::CheckpointStatus::Captured
            );
            assert!(view.goals[0].success_criterion_satisfied);
            assert!(render_session(&view, &events, Output::Text).contains("multiedit"));
            let jsonl = render_session(&view, &events, Output::Jsonl);
            let exported: Vec<arany_cli::protocol::EventLine> = jsonl
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                exported,
                events
                    .iter()
                    .map(arany_cli::protocol::EventLine::from)
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(events.iter().filter(|e| matches!(&e.event, Event::ToolFinished { observation, .. } if matches!(observation.intent.call, arany::ToolCall::MultiEdit { .. }))).count(), 1);
    }
}

#[tokio::test]
async fn typed_mutations_cancelled_approval_and_lost_receipt_never_retry() {
    for call in [multiedit(), removal(false), removal(true)] {
        for fault in [
            MutationFailure::Cancelled,
            MutationFailure::ReceiptLost,
            MutationFailure::ParentSync,
        ] {
            cancelled_or_lost_mutation(call.clone(), fault).await;
        }
    }
    cancelled_or_lost_mutation(
        removal(true),
        MutationFailure::ParentSyncWithDestinationParent,
    )
    .await;
}

#[derive(Clone, Copy, Debug)]
enum MutationFailure {
    Cancelled,
    ReceiptLost,
    ParentSync,
    ParentSyncWithDestinationParent,
}

async fn cancelled_or_lost_mutation(call: arany::ToolCall, fault: MutationFailure) {
    let fixture = Fixture::new(None, true);
    if matches!(fault, MutationFailure::ParentSyncWithDestinationParent) {
        std::fs::create_dir(fixture.workspace.join("notes/nested")).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new(vec![tool_outcome(call.clone())]));
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let cancellation = RunCancellation::new();
    let mut input = request(&fixture.workspace, "Make one file mutation");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Single;
    let mut asked = 0;
    let outcome = {
        let running = run_with(
            &fixture.engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals),
                cancellation: cancellation.clone(),
                ..Default::default()
            },
        );
        tokio::pin!(running);
        loop {
            tokio::select! {
                biased;
                result = &mut running => break result,
                Some(approval) = inbox.next() => {
                    asked += 1;
                    match fault {
                        MutationFailure::ReceiptLost => arany::FaultPlan::lose_guard_receipt(approval.intent.id),
                        MutationFailure::ParentSync | MutationFailure::ParentSyncWithDestinationParent => arany::FaultPlan::fail_file_parent_sync(approval.intent.id),
                        MutationFailure::Cancelled => { cancellation.cancel(); }
                    }
                    approval.decide(true);
                }
            }
        }
    };
    let published = !matches!(fault, MutationFailure::Cancelled);
    if published {
        assert!(matches!(
            outcome,
            Err(EngineError::Tool(arany::ToolError::Uncertain))
        ));
    } else {
        assert_eq!(outcome.unwrap().run.status, RunStatus::Cancelled);
    }
    assert_eq!(asked, 1);
    let session = {
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "no continuation or automatic retry");
        requests[0].session_id
    };
    assert_stopped_mutation(fixture, session, &call, fault).await;
}

async fn assert_stopped_mutation(
    fixture: Fixture,
    session: SessionId,
    call: &arany::ToolCall,
    fault: MutationFailure,
) {
    let published = !matches!(fault, MutationFailure::Cancelled);
    let move_sync_failed = matches!(
        fault,
        MutationFailure::ParentSync | MutationFailure::ParentSyncWithDestinationParent
    ) && matches!(call, arany::ToolCall::Move { .. });
    let removed = published
        && !move_sync_failed
        && matches!(
            call,
            arany::ToolCall::Delete { .. } | arany::ToolCall::Move { .. }
        );
    assert_eq!(
        fixture.workspace.join("notes/facts.txt").exists(),
        !removed,
        "{fault:?}"
    );
    if !removed {
        assert_eq!(
            std::fs::read(fixture.workspace.join("notes/facts.txt")).unwrap(),
            if published && !move_sync_failed {
                b"done".as_slice()
            } else {
                b"Fact".as_slice()
            }
        );
    }
    let destination_published = matches!(call, arany::ToolCall::Move { .. })
        && matches!(
            fault,
            MutationFailure::ReceiptLost | MutationFailure::ParentSyncWithDestinationParent
        );
    assert_eq!(
        fixture.workspace.join("notes/nested/moved.txt").exists(),
        destination_published,
        "{fault:?}"
    );
    if matches!(call, arany::ToolCall::Move { .. }) {
        assert_eq!(fixture.workspace.join("notes/nested").is_dir(), published);
    }
    if destination_published {
        assert_eq!(
            std::fs::read(fixture.workspace.join("notes/nested/moved.txt")).unwrap(),
            b"Fact"
        );
    }
    let (_, view) = fixture.reopen(session).await;
    assert_eq!(view.runs[0].tools.len(), 1);
    let observation = view.runs[0].tools[0].observation.as_ref().unwrap();
    if published {
        assert_eq!(observation.disposition, arany::ToolDisposition::Uncertain);
        assert_eq!(
            observation.attestation.is_some(),
            matches!(
                fault,
                MutationFailure::ParentSync | MutationFailure::ParentSyncWithDestinationParent
            )
        );
    } else {
        assert!(observation.attestation.is_none());
    }
}

#[tokio::test]
async fn multiedit_crash_prefixes_keep_pre_go_and_committed_publication_facts() {
    for stage in ["started", "published", "finished"] {
        let finished = stage == "finished";
        let fixture = Fixture::new(None, true);
        let session = fixture
            .engine
            .create_session(fixture.workspace.clone(), None)
            .await
            .unwrap();
        let gate = if stage == "published" {
            arany::FaultPlan::fail_goal_append(session, |event| {
                matches!(event, Event::ToolFinished { .. })
            });
            None
        } else {
            Some(arany::FaultPlan::gate_goal_append_acknowledgment(
                session,
                if finished {
                    |e| matches!(e, Event::ToolFinished { .. })
                } else {
                    |e| matches!(e, Event::ToolStarted { .. })
                },
            ))
        };
        let provider = Arc::new(ScriptedProvider::new(vec![tool_outcome(multiedit())]));
        let mut input = request(&fixture.workspace, "Make one atomic edit");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        input.policy = CollaborationPolicy::Single;
        {
            let running = run_with(
                &fixture.engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            );
            tokio::pin!(running);
            if let Some((entered, release)) = gate {
                tokio::select! {
                    result = &mut running => panic!("acknowledgment gate was not reached: {result:?}"),
                    entered = entered => entered.unwrap(),
                }
                drop(release);
            }
            assert!(running.await.is_err());
        }
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        let bytes = if stage != "started" {
            b"done".as_slice()
        } else {
            b"Fact".as_slice()
        };
        assert_eq!(
            std::fs::read(fixture.workspace.join("notes/facts.txt")).unwrap(),
            bytes
        );
        let (_, view) = fixture.reopen(session).await;
        assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        assert_eq!(view.runs[0].tools.len(), 1);
        let observed = &view.runs[0].tools[0].observation;
        assert_eq!(observed.is_some(), finished);
        if let Some(observed) = observed {
            assert_eq!(observed.disposition, arany::ToolDisposition::Succeeded);
        }
    }
}

#[tokio::test]
async fn multiedit_content_ceiling_and_full_hook_input_refuse_overflow_without_publication() {
    for case in ["exact", "overflow", "hook"] {
        let mut settings = json!({"permissions":{"defaultMode":"acceptEdits"}});
        if case == "hook" {
            settings["hooks"] =
                json!({"PreToolUse":[{"matcher":"MultiEdit","hooks":[shell_hook("exit 0")]}]});
        }
        let fixture = Fixture::new(Some(&settings), true);
        let old = "a".repeat(32768);
        let new = "b".repeat(32768 + usize::from(case == "overflow"));
        std::fs::write(fixture.workspace.join("notes/facts.txt"), &old).unwrap();
        let call = arany::ToolCall::MultiEdit {
            file_path: "notes/facts.txt".into(),
            expected_digest: hash(old.as_bytes()),
            edits: vec![arany::LiteralEdit {
                old_string: old.clone(),
                new_string: new.clone(),
                replace_all: false,
            }],
        };
        let (result, _, humans) = fixture
            .run(
                vec![tool_outcome(call.clone()), finish("answer")],
                None,
                false,
            )
            .await;
        assert_eq!(humans, 0);
        let expected = if case == "exact" { &new } else { &old };
        assert_eq!(
            std::fs::read(fixture.workspace.join("notes/facts.txt")).unwrap(),
            expected.as_bytes()
        );
        let (events, view) = fixture.reopen(result.session_id).await;
        if case == "overflow" {
            assert!(view.runs[0].tools.is_empty());
        } else {
            let observation = view.runs[0].tools[0].observation.as_ref().unwrap();
            assert_eq!(observation.intent.call, call);
            assert_eq!(
                observation.disposition,
                if case == "exact" {
                    arany::ToolDisposition::Succeeded
                } else {
                    arany::ToolDisposition::Denied
                }
            );
            if case == "hook" {
                assert_eq!(
                    observation.output,
                    "hook_denied: no operation was dispatched"
                );
                assert!(
                    !events
                        .iter()
                        .any(|e| matches!(e.event, Event::HookStarted { .. }))
                );
            }
        }
    }
}

#[tokio::test]
async fn permission_dispatch_over_maximum_command_timeout_limits_before_deny() {
    for settings in [
        json!({"permissions":{"defaultMode":"plan"}}),
        json!({"permissions":{"deny":["Bash"]}}),
    ] {
        let mut fixture = Fixture::new(Some(&settings), true);
        fixture.command_maximum = Some(1_000);
        let call = call("command");
        let mut plan = plan(&call);
        plan.command_timeout_ms = Some(2_000);
        let (result, requests, humans) = fixture
            .run(
                vec![
                    arany::test_support::scenario::plan_outcome(plan),
                    tool_outcome(call),
                    finish("answer"),
                ],
                Some(arany::ApprovalMode::Request),
                false,
            )
            .await;
        assert_eq!(humans, 0, "{settings}");
        assert_eq!(
            requests[2].input.tools.as_ref().unwrap().steps[1].observations[0].output,
            "Timeout 2000 ms exceeds pinned max 1000 ms. Lower request or change /settings; Goal remains pinned.",
            "{settings}"
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        assert_eq!(view.runs[0].status, arany::RunStatus::Failed, "{settings}");
        let tool = &view.runs[0].tools[0];
        let verdict = tool.permission_verdict.as_ref().unwrap();
        assert_eq!(verdict.decision, Decision::Deny, "{settings}");
        assert!(!verdict.approved, "{settings}");
        assert_eq!(
            tool.observation.as_ref().unwrap().disposition,
            arany::ToolDisposition::Limit,
            "{settings}"
        );
        assert_eq!(
            tool.observation.as_ref().unwrap().limit_cause,
            Some(arany::LimitCause::RequestedTime)
        );
    }
}

#[tokio::test]
async fn a_one_second_command_timeout_is_admitted_and_ends_as_a_limit() {
    let mut fixture = Fixture::new(None, true);
    fixture.command_maximum = Some(1_000);
    let call = arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), "printf 'head\\n'; printf '%100000s' ''; printf 'tail\\n'; printf 'partial error\\n' >&2; printf changed > notes/new.txt; sleep 20".into()],
        cwd: "".into(),
    };
    let mut plan = plan(&call);
    plan.command_timeout_ms = Some(1_000);
    let (result, _, _) = fixture
        .run(
            vec![
                arany::test_support::scenario::plan_outcome(plan),
                tool_outcome(call),
                finish("answer"),
            ],
            Some(arany::ApprovalMode::Request),
            false,
        )
        .await;
    let observation = result.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(
        observation.disposition,
        arany::ToolDisposition::Limit,
        "{}",
        tool_report(Some(observation))
    );
    assert!(
        matches!(observation.attestation, Some(arany::Attestation::Guard(_))),
        "{}",
        tool_report(Some(observation))
    );
    assert_eq!(observation.limit_cause, Some(arany::LimitCause::Timeout));
    let command: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(command["exit_code"], Value::Null);
    assert!(command["changeset"]["files"].as_array().unwrap().is_empty());
    assert!(
        command["stdout"].as_str().unwrap().starts_with("head\n")
            && command["stdout"].as_str().unwrap().ends_with("tail\n")
    );
    assert!(
        command["stdout"]
            .as_str()
            .unwrap()
            .contains(" lines omitted; ")
    );
    assert_eq!(command["stdout_total"]["bytes"], 100010);
    assert_eq!(command["stderr"], "partial error\n");
    assert!(observation.output.len() <= observation.intent.limits.result_bytes as usize);
    assert!(!fixture.workspace.join("notes/new.txt").exists());
    let expected = observation.clone();
    let (events, view) = fixture.reopen(result.session_id).await;
    assert_eq!(view.runs[0].tools[0].observation.as_ref(), Some(&expected));
    assert!(events.iter().any(|envelope| matches!(&envelope.event,
        Event::ToolFinished { observation, .. } if observation == &expected)));
}

#[tokio::test]
async fn guarded_command_over_its_timeout_with_a_background_child_is_a_limit_and_reaps_it() {
    let marker = format!("30.{}", std::process::id());
    let sleepers = || async {
        let listing = tokio::process::Command::new("ps")
            .args(["-A", "-o", "command="])
            .output()
            .await
            .expect("process listing");
        String::from_utf8_lossy(&listing.stdout)
            .lines()
            .filter(|line| line.trim() == format!("sleep {marker}"))
            .count()
    };
    let mut fixture = Fixture::new(None, true);
    // Long enough to observe both children running before the timeout.
    fixture.command_maximum = Some(10_000);
    let call = arany::ToolCall::Command {
        program: "sh".into(),
        args: vec!["-c".into(), format!("sleep {marker} & sleep {marker}")],
        cwd: "".into(),
    };
    let mut plan = plan(&call);
    plan.command_timeout_ms = Some(10_000);
    let observe_both = async {
        for _ in 0..300 {
            if sleepers().await == 2 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    };
    let ((result, _, _), started) = tokio::join!(
        fixture.run(
            vec![
                arany::test_support::scenario::plan_outcome(plan),
                tool_outcome(call),
                finish("answer")
            ],
            Some(arany::ApprovalMode::Request),
            false,
        ),
        observe_both
    );
    assert!(
        started,
        "the foreground and background children both started"
    );
    let tool = &result.run.tools[0];
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(arany::LimitCause::Timeout));
    let partial: Value = serde_json::from_str(&observation.output).unwrap();
    assert!(partial["changeset"]["files"].as_array().unwrap().is_empty());
    assert!(matches!(
        observation.attestation,
        Some(arany::Attestation::Guard(_))
    ));
    // An exited descendant may linger briefly before its new parent collects it.
    let mut reaped = false;
    for _ in 0..20 {
        reaped = sleepers().await == 0;
        if reaped {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reaped, "the background child outlived the Guard teardown");
}

#[tokio::test]
async fn a_64_kib_write_and_edit_publish_unhooked_and_a_hook_that_cannot_receive_them_denies() {
    let ceiling = 64 * 1024;
    let path = "notes/large.txt";
    let written = format!("{}{}", "a".repeat(ceiling / 2), "b".repeat(ceiling / 2));
    let write = arany::ToolCall::Write {
        path: path.into(),
        expected_digest: None,
        content: written.clone(),
    };
    let edit = arany::ToolCall::Edit {
        path: path.into(),
        expected_digest: hash(written.as_bytes()),
        old: "b".repeat(ceiling / 2),
        new: "c".repeat(ceiling / 2),
    };
    let over = arany::ToolCall::Write {
        path: path.into(),
        expected_digest: None,
        content: "x".repeat(ceiling + 1),
    };
    for event in [None, Some("PreToolUse"), Some("PostToolUse")] {
        let mut settings = json!({"permissions": {"defaultMode": "acceptEdits"}});
        let outcomes = match event {
            None => vec![
                tool_outcome(over.clone()),
                tool_outcome(write.clone()),
                tool_outcome(edit.clone()),
                finish("answer"),
            ],
            Some(event) => {
                settings["hooks"] =
                    json!({(event): [{"matcher": "Write", "hooks": [shell_hook("exit 0")]}]});
                vec![tool_outcome(write.clone()), finish("answer")]
            }
        };
        let fixture = Fixture::new(Some(&settings), true);
        let (result, _, humans) = fixture.run(outcomes, None, false).await;
        assert_eq!(humans, 0, "{event:?}");
        let published = std::fs::read(fixture.workspace.join(path)).ok();
        let (events, view) = fixture.reopen(result.session_id).await;
        let started: Vec<_> = events
            .iter()
            .filter_map(|envelope| match &envelope.event {
                Event::ToolStarted { intent, .. } => Some(intent.call.clone()),
                _ => None,
            })
            .collect();
        let tools = &view.runs[0].tools;
        if event.is_none() {
            let refused = events.iter().any(|envelope| {
                matches!(&envelope.event, Event::ProviderCallRecorded { record, .. }
                    if record.refusal.as_ref().is_some_and(|refusal| refusal.operation == "arany_write"
                        && refusal.cause == arany::ProposalRefusalCause::InvalidArguments))
            });
            assert!(refused, "64 KiB + 1 is refused before any intent");
            assert_eq!(
                started,
                [write.clone(), edit.clone()],
                "complete intents persist"
            );
            assert!(tools.iter().all(|tool| {
                tool.observation
                    .as_ref()
                    .is_some_and(|o| o.disposition == arany::ToolDisposition::Succeeded)
            }));
            let expected = format!("{}{}", "a".repeat(ceiling / 2), "c".repeat(ceiling / 2));
            assert_eq!(published.as_deref(), Some(expected.as_bytes()));
            for output in [Output::Text, Output::Jsonl] {
                assert!(render_session(&view, &events, output).contains(&written));
            }
            continue;
        }
        // Denied whole before any handler or effect: no truncation, bypass or unhooked retry.
        assert_eq!(started, std::slice::from_ref(&write), "{event:?}");
        assert!(
            !events
                .iter()
                .any(|envelope| matches!(envelope.event, Event::HookStarted { .. })),
            "{event:?}"
        );
        assert_eq!(published, None, "{event:?}");
        let verdict = tools[0].permission_verdict.as_ref().unwrap();
        assert_eq!(
            (verdict.hooks, verdict.hook_restriction),
            (0, Some(Decision::Deny))
        );
        let observation = tools[0].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
        assert_eq!(
            observation.output, "hook_denied: no operation was dispatched",
            "{event:?}"
        );
    }
}

#[tokio::test]
async fn maximum_size_stale_or_ambiguous_edits_are_refused_with_the_file_preserved() {
    let half = 32 * 1024;
    let path = "notes/large.txt";
    // `old` below occurs twice, so the digest-current Edit is ambiguous.
    let original = format!("{}|{}", "x".repeat(half), "x".repeat(half));
    let stale = hash(b"stale");
    let calls = [
        arany::ToolCall::Write {
            path: path.into(),
            expected_digest: Some(stale.clone()),
            content: "y".repeat(2 * half),
        },
        arany::ToolCall::Edit {
            path: path.into(),
            expected_digest: stale,
            old: "x".repeat(half),
            new: "y".repeat(half),
        },
        arany::ToolCall::Edit {
            path: path.into(),
            expected_digest: hash(original.as_bytes()),
            old: "x".repeat(half),
            new: "y".repeat(half),
        },
    ];
    // One Run per case: three 64 KiB proposals would otherwise need context compaction.
    for call in calls {
        let fixture = Fixture::new(
            Some(&json!({"permissions": {"defaultMode": "acceptEdits"}})),
            true,
        );
        std::fs::write(fixture.workspace.join(path), &original).unwrap();
        let (result, _, _) = fixture
            .run(
                vec![tool_outcome(call.clone()), finish("answer")],
                None,
                false,
            )
            .await;
        assert_eq!(
            std::fs::read(fixture.workspace.join(path)).unwrap(),
            original.as_bytes()
        );
        let (_, view) = fixture.reopen(result.session_id).await;
        let tool = &view.runs[0].tools[0];
        assert_eq!(tool.intent.call, call);
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            arany::ToolDisposition::Conflict,
            "{}",
            observation.output
        );
        assert!(observation.output.len() < 1024, "no content echo");
    }
}

// A lost completion after GO leaves a maximum-size Write unknown: no retry, no further call.
#[tokio::test]
async fn a_lost_maximum_size_write_completion_stays_uncertain_across_reopen() {
    let fixture = Fixture::new(Some(&json!({"permissions": {}})), true);
    let write = arany::ToolCall::Write {
        path: "notes/large.txt".into(),
        expected_digest: None,
        // Two-byte escapes: six-byte ones would not fit the default Run context with the proposal.
        content: "\"".repeat(64 * 1024),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![tool_outcome(write.clone())]));
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let mut input = request(&fixture.workspace, "Inspect and complete the task");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Single;
    input.goal.budget = Some(budget());
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: Some(approvals),
        ..Default::default()
    };
    let outcome = {
        let running = run_with(&fixture.engine, &provider, input, controls);
        tokio::pin!(running);
        loop {
            tokio::select! {
                biased;
                outcome = &mut running => break outcome,
                Some(approval) = inbox.next() => {
                    arany::FaultPlan::lose_guard_receipt(approval.intent.id);
                    approval.decide(true);
                }
            }
        }
    };
    assert!(
        matches!(
            outcome,
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain))
        ),
        "{:?}",
        outcome.as_ref().map(|outcome| (
            outcome.run.status,
            outcome.run.agents[0]
                .provider_calls
                .iter()
                .map(|call| format!(
                    "{:?} {:?} {:?}",
                    call.disposition, call.failure_reason, call.refusal
                ))
                .collect::<Vec<_>>()
        ))
    );
    assert_eq!(
        provider.requests.lock().unwrap().len(),
        1,
        "no Provider call follows"
    );
    let listed = arany::list_sessions(
        StateRoot::open_existing(&fixture.state).unwrap(),
        fixture.workspace.clone(),
    )
    .await
    .unwrap();
    let (events, view) = fixture.reopen(listed[0].id).await;
    let started = events
        .iter()
        .filter(|envelope| matches!(envelope.event, Event::ToolStarted { .. }))
        .count();
    assert_eq!(started, 1, "never retried");
    assert!(
        !events
            .iter()
            .any(|envelope| matches!(envelope.event, Event::RunFinished { .. }))
    );
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    let tool = &view.runs[0].tools[0];
    assert_eq!(tool.intent.call, write);
    assert_eq!(
        tool.observation.as_ref().unwrap().disposition,
        arany::ToolDisposition::Uncertain
    );
}
