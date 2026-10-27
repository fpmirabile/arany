use super::*;
use crate::session::reducer::worker::editor_criterion_evidence;
use crate::tools::types::{CommandChangeset, CommandResult, OutputTotal};

fn command_fixture() -> Fixture {
    command_fixture_with_tools(false)
}

fn command_fixture_with_tools(only_command: bool) -> Fixture {
    let mut f = fixture_with_grant(
        if only_command {
            vec!["command".into()]
        } else {
            vec!["command".into(), "read".into(), "write".into()]
        },
        vec![WorkerTool::Read, WorkerTool::Write, WorkerTool::Command],
        None,
    );
    for event in &mut f.events {
        match event {
            Event::RunStarted { config, .. } => {
                config.worker_policy.as_mut().unwrap().parent_grant.commands = vec!["build".into()]
            }
            Event::GoalPlanContractRecorded { contract, .. } => {
                contract.commands = vec!["build".into()]
            }
            Event::ProviderCallRecorded { record, .. } => {
                if let Some(children) = &mut record.delegation {
                    for child in children {
                        if let ChildAssignment::Editor { assignment } = child {
                            assignment.commands = vec!["build".into()];
                            assignment.mode = WorkerMode::Background;
                            if only_command {
                                assignment.criterion = command_criterion();
                            }
                        }
                    }
                }
            }
            Event::AgentSpawned {
                assignment: Some(pins),
                ..
            } => {
                pins.request.commands = vec!["build".into()];
                pins.request.mode = WorkerMode::Background;
                pins.grant.commands = vec!["build".into()];
                if only_command {
                    pins.grant.tools = vec![WorkerTool::Command];
                    pins.request.criterion = command_criterion();
                }
            }
            _ => {}
        }
    }
    f
}

fn command_criterion() -> GoalSuccessCriterion {
    GoalSuccessCriterion::ToolCallSucceeded {
        call: Box::new(ToolCall::Command {
            program: "build".into(),
            args: vec![],
            cwd: ".".into(),
        }),
    }
}

fn command(f: &mut Fixture, paths: &[&str]) -> EffectIntent {
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Command {
            program: "build".into(),
            args: vec![],
            cwd: ".".into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    let changeset = CommandChangeset {
        files: paths
            .iter()
            .map(|path| FileChange {
                path: (*path).into(),
                before: None,
                after: Some(FileVersion::from_bytes(b"new", false)),
            })
            .collect(),
    };
    let output = CommandResult {
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
        stdout_total: OutputTotal::default(),
        stderr_total: OutputTotal::default(),
        changeset,
        selection: None,
    };
    f.observed(
        &intent,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&intent)),
        &serde_json::to_string(&output).unwrap(),
    );
    intent
}

fn publication(f: &Fixture, source: &EffectIntent) -> EffectIntent {
    let Event::ToolFinished { observation, .. } = f.events.last().unwrap() else {
        unreachable!()
    };
    let result: CommandResult = serde_json::from_str(&observation.output).unwrap();
    f.intent(
        f.child,
        ToolCall::ApplyChangeset {
            source: ChangesetSource::CommandReceipt {
                source_intent_digest: source.digest(),
                changeset: result.changeset,
            },
        },
        EffectTarget::Workspace,
    )
}

fn rejected(f: &Fixture, intent: &EffectIntent) {
    let mut events = f.envelopes();
    events.push(f.envelope(
        Event::ToolStarted {
            run_id: f.run,
            agent_run_id: intent.agent_run_id,
            intent: intent.clone().into(),
            active_ceiling_ms: 0,
            verdict: None,
        },
        events.len() as u64 + 1,
    ));
    assert!(matches!(
        SessionView::replay_open(f.session, &events),
        Err(ReplayError::InvalidTransition)
    ));
}

#[test]
fn editor_admission_refuses_partially_granted_command_lists() {
    for only_command in [false, true] {
        let f = command_fixture_with_tools(only_command);
        let view = f.view();
        let mut request = view.runs[0].agents[1]
            .assignment
            .as_ref()
            .unwrap()
            .pins
            .request
            .clone();
        request.commands.push("unadmitted".into());
        assert_eq!(
            crate::session::validate_editor_request(&view, &view.runs[0], &request, None),
            Err(crate::session::ProposalRefusalCause::EditorCommands)
        );
    }
}

#[test]
fn command_publication_is_child_causal_lease_bounded_and_required_for_criterion() {
    for (outside, only_command) in [(false, false), (true, false), (false, true)] {
        let mut f = command_fixture_with_tools(only_command);
        if only_command {
            let view = f.view();
            assert_eq!(
                view.runs[0].agents[1]
                    .assignment
                    .as_ref()
                    .unwrap()
                    .pins
                    .grant
                    .tools,
                [WorkerTool::Command]
            );
        }
        let source = command(
            &mut f,
            if outside {
                &["src/a", "src/unleased"]
            } else {
                &["src/a"]
            },
        );
        let intent = publication(&f, &source);
        if outside {
            rejected(&f, &intent);
            assert!(
                f.view().runs[0].worker_history()[0]
                    .changed_paths
                    .is_empty()
            );
            continue;
        }
        let count = f.events.len();
        f.call(
            f.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::ToolRequested,
            None,
        );
        assert!(
            matches!(
                SessionView::replay_open(f.session, &f.envelopes()),
                Err(ReplayError::InvalidTransition)
            ),
            "a captured batch requires publication before continuation"
        );
        f.events.truncate(count);
        let mut forged = intent.clone();
        let ToolCall::ApplyChangeset {
            source:
                ChangesetSource::CommandReceipt {
                    source_intent_digest,
                    ..
                },
        } = &mut forged.call
        else {
            unreachable!()
        };
        *source_intent_digest = [9; 32];
        rejected(&f, &forged);
        f.started(&intent);
        f.observed(
            &intent,
            ToolDisposition::Succeeded,
            Some(Attestation::for_site(&intent)),
            "{}",
        );
        let view = f.view();
        assert!(view.runs[0].agents[1].tools[1].checkpoint.is_some());
        let report = &view.runs[0].worker_history()[0].changed_paths;
        assert!(report.contains(&crate::validate::hex_digest(b"new")));
        assert!(report.contains("src/a"));
        let mut run = view.runs[0].clone();
        run.agents[1]
            .assignment
            .as_mut()
            .unwrap()
            .pins
            .request
            .criterion = GoalSuccessCriterion::ToolCallSucceeded {
            call: Box::new(source.call.clone()),
        };
        assert_eq!(editor_criterion_evidence(&run, f.child).unwrap().len(), 2);
        run.agents[1].tools.pop();
        assert!(editor_criterion_evidence(&run, f.child).unwrap().is_empty());
        f.call(
            f.child,
            AgentPhase::ChildWork,
            ProviderCallDisposition::ToolRequested,
            None,
        );
        f.view();
        for end in 1..=f.events.len() {
            assert!(SessionView::replay(f.session, &f.envelopes()[..end]).is_ok());
        }
    }
}

#[test]
fn uncertain_editor_cache_blocks_continuation_and_recovery_after_interruption() {
    for unknown in [false, true] {
        let mut f = command_fixture();
        command(&mut f, &[]);
        let Event::ToolFinished { observation, .. } = f.events.last_mut().unwrap() else {
            unreachable!()
        };
        observation.cache_update = if unknown {
            CacheUpdate::Uncertain
        } else {
            CacheUpdate::NotApplicable
        };
        let view = f.view();
        assert_eq!(view.runs[0].agents[1].tools[0].outcome_unknown(), unknown);
        if unknown {
            assert!(!wave_settled(&view.runs[0]));
            let count = f.events.len();
            f.call(
                f.child,
                AgentPhase::ChildWork,
                ProviderCallDisposition::Finished,
                None,
            );
            assert!(matches!(
                SessionView::replay_open(f.session, &f.envelopes()),
                Err(ReplayError::InvalidTransition)
            ));
            f.events.truncate(count);
        }
        let recovered = SessionView::replay(f.session, &f.envelopes())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.runs[0].status, RunStatus::Interrupted);
        let goal = recovered.goal(f.goal).unwrap();
        assert_eq!(goal.active_run, None);
        assert_eq!(
            goal.can_resume(&recovered),
            !unknown,
            "uncertain: {unknown}"
        );
    }
}

#[test]
fn editor_reports_are_bounded_untrusted_latest_observation_facts() {
    let mut f = command_fixture();
    for event in &mut f.events {
        match event {
            Event::AgentSpawned {
                assignment: Some(pins),
                ..
            } => {
                pins.request.lease = vec!["src".into()];
                pins.grant.write_paths = pins.request.lease.clone();
            }
            Event::ProviderCallRecorded { record, .. } => {
                if let Some(children) = &mut record.delegation {
                    for child in children {
                        if let ChildAssignment::Editor { assignment } = child {
                            assignment.lease = vec!["src".into()];
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let paths: Vec<_> = (0..64).map(|index| format!("src/file{index:03}")).collect();
    let source = command(
        &mut f,
        &paths.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    let intent = publication(&f, &source);
    f.started(&intent);
    f.observed(
        &intent,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&intent)),
        "{}",
    );
    let mut view = f.view();
    view.runs[0].agents[1].result = Some("forged-path-with-forged-digest".into());
    let report = &view.runs[0].worker_history()[0].changed_paths;
    assert!(report.len() <= MAX_EDITOR_CHANGE_REPORT_BYTES);
    assert!(report.starts_with("Changed paths (untrusted"));
    assert!(report.ends_with("Further changed paths omitted.\n"));
    assert!(!report.contains("forged"));
    for row in report
        .lines()
        .skip(1)
        .take_while(|row| row.starts_with('{'))
    {
        let row: serde_json::Value = serde_json::from_str(row).unwrap();
        assert_eq!(row["after_sha256"], crate::validate::hex_digest(b"new"));
    }
}

#[test]
fn background_lease_admission_is_symmetric_with_sibling_and_primary_writes() {
    let mut f = command_fixture();
    let mut sibling = f.events.last().unwrap().clone();
    let other = AgentRunId::new();
    let Event::AgentSpawned {
        agent_run_id,
        ordinal,
        assignment: Some(pins),
        ..
    } = &mut sibling
    else {
        unreachable!()
    };
    *agent_run_id = other;
    *ordinal = 2;
    pins.request.lease = vec!["src/b".into()];
    pins.grant.write_paths = pins.request.lease.clone();
    pins.request.criterion = GoalSuccessCriterion::WorkspaceFileContent {
        path: "src/b".into(),
        content: "new".into(),
    };
    let request = pins.request.clone();
    let Event::ProviderCallRecorded { record, .. } = f.events.iter_mut().rev().find(|event| matches!(event, Event::ProviderCallRecorded { record, .. } if record.delegation.is_some())).unwrap() else { unreachable!() };
    record
        .delegation
        .as_mut()
        .unwrap()
        .push(ChildAssignment::Editor {
            assignment: Box::new(request),
        });
    f.push(sibling);
    let view = f.view();
    let run = &view.runs[0];
    assert!(coordination_ready(run));
    assert!(!wave_settled(run));
    for (actor, path, allowed) in [
        (f.primary, "src/a", false),
        (f.primary, "src/b", false),
        (f.primary, "src/primary", true),
        (f.child, "src/a", true),
        (f.child, "src/b", false),
        (other, "src/a", false),
        (other, "src/b", true),
    ] {
        let call = ToolCall::Write {
            path: path.into(),
            expected_digest: None,
            content: "new".into(),
        };
        assert_eq!(
            super::super::tool::validate_tool_lease(run, actor, &call).is_ok(),
            allowed
        );
    }
    let call = ToolCall::Command {
        program: "build".into(),
        args: vec![],
        cwd: ".".into(),
    };
    for (actor, allowed) in [(f.primary, false), (f.child, true), (other, true)] {
        assert_eq!(
            super::super::tool::validate_tool_lease(run, actor, &call).is_ok(),
            allowed
        );
    }
    let source = command(&mut f, &["src/a"]);
    let intent = publication(&f, &source);
    f.started(&intent);
    f.observed(
        &intent,
        ToolDisposition::Succeeded,
        Some(Attestation::for_site(&intent)),
        "{}",
    );
    f.view();
}

#[test]
fn changed_path_reports_keep_only_the_latest_digest_for_each_path() {
    let mut f = fixture();
    f.mutation();
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Write {
            path: "src/a".into(),
            expected_digest: Some(crate::validate::hex_digest(b"new")),
            content: "latest".into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    f.observed(&intent, ToolDisposition::Succeeded, Some(Attestation::for_site(&intent)),
        &serde_json::json!({"sha256":crate::validate::hex_digest(b"latest"),"bytes":6,"executable":false,
            "before":{"sha256":crate::validate::hex_digest(b"new"),"bytes":3,"executable":false}}).to_string());
    let view = f.view();
    let report = &view.runs[0].worker_history()[0].changed_paths;
    assert_eq!(report.matches("src/a").count(), 1);
    assert!(report.contains(&crate::validate::hex_digest(b"latest")));
    assert!(!report.contains(&crate::validate::hex_digest(b"new")));
}
