use super::*;

#[test]
fn release_is_causal_and_verification_requires_fresh_primary_evidence() {
    let mut f = fixture();
    rejected(
        &f,
        Event::WorkerLeaseReleased {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerLeaseRelease {
                assignment_id: f.child,
                cause_sequence: f.events.len() as u64,
            },
        },
    );
    f.mutation();
    let cause = f.finish_worker();
    rejected(
        &f,
        Event::WorkerLeaseReleased {
            run_id: f.run,
            agent_run_id: f.primary,
            record: WorkerLeaseRelease {
                assignment_id: f.child,
                cause_sequence: cause - 1,
            },
        },
    );
    f.release(cause);
    let verification = WorkerVerification {
        assignment_id: f.child,
        criterion_digest: crate::validate::digest(
            &serde_json::to_vec(&GoalSuccessCriterion::WorkspaceFileContent {
                path: "src/a".into(),
                content: "new".into(),
            })
            .unwrap(),
        ),
        passed: true,
        evidence: vec![cause - 3],
    };
    rejected(
        &f,
        Event::WorkerVerificationRecorded {
            run_id: f.run,
            agent_run_id: f.primary,
            record: verification,
        },
    );
    f.verify();
    let release = f
        .events
        .iter()
        .find(|e| matches!(e, Event::WorkerLeaseReleased { .. }))
        .unwrap()
        .clone();
    rejected(&f, release);
}

#[test]
fn landed_editor_write_survives_cancel_and_unknown_effect_blocks_resume() {
    for unknown in [false, true] {
        let mut f = fixture();
        f.mutation();
        if unknown {
            let Event::ToolFinished { observation, .. } = f.events.last_mut().unwrap() else {
                unreachable!()
            };
            observation.disposition = ToolDisposition::Uncertain;
        } else {
            f.push(Event::WorkerStopObserved {
                run_id: f.run,
                agent_run_id: f.primary,
                record: WorkerStopRecord {
                    reason: WorkerStopReason::Cancelled,
                },
            });
            let cause = f.push(Event::AgentFinished {
                run_id: f.run,
                agent_run_id: f.child,
                disposition: AgentDisposition::Cancelled,
                summary: None,
                result: None,
                worker: Some(WorkerOutcome {
                    disposition: WorkerDisposition::Cancelled,
                    cause: WorkerCause::Cancelled,
                }),
            });
            f.release(cause);
        }
        let view = SessionView::replay(f.session, &f.envelopes())
            .unwrap()
            .unwrap();
        assert_eq!(view.goal(f.goal).unwrap().can_resume(&view), !unknown);
        let history = &view.runs[0].worker_history()[0];
        assert_eq!(
            history.publication,
            if unknown {
                WorkerPublication::Unknown
            } else {
                WorkerPublication::Confirmed
            }
        );
        assert_eq!(history.lease_released, !unknown);
    }
}

fn stale_goal_fixture() -> Fixture {
    let mut f = fixture();
    for event in &mut f.events {
        match event {
            Event::GoalPlanContractRecorded { contract, .. } => {
                contract.success_criterion = GoalSuccessCriterion::WorkspaceFileContent {
                    path: "src/base".into(),
                    content: "original".into(),
                };
            }
            Event::ToolFinished { observation, .. } if matches!(&observation.intent.call, ToolCall::Read { path, .. } if path == "src/base") =>
            {
                observation.output =
                    serde_json::json!({"sha256": crate::validate::hex_digest(b"original")})
                        .to_string();
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
            Event::AgentSpawned {
                assignment: Some(pins),
                ..
            } => {
                pins.request.lease = vec!["src".into()];
                pins.grant.write_paths = pins.request.lease.clone();
            }
            _ => {}
        }
    }
    f
}

fn change_goal_file(f: &mut Fixture) {
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Write {
            path: "src/base".into(),
            expected_digest: Some(crate::validate::hex_digest(b"original")),
            content: "changed".into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    f.observed(&intent, ToolDisposition::Succeeded, Some(Attestation::for_site(&intent)),
        &serde_json::json!({"sha256":crate::validate::hex_digest(b"changed"),"bytes":7,"executable":false,
            "before": {"sha256":crate::validate::hex_digest(b"original"),"bytes":8,"executable":false}}).to_string());
}

#[test]
fn editor_mutation_invalidates_goal_even_after_primary_verifies_a_distinct_assignment() {
    let mut f = stale_goal_fixture();
    assert!(f.view().goal(f.goal).unwrap().success_criterion_satisfied);
    change_goal_file(&mut f);
    assert!(!f.view().goal(f.goal).unwrap().success_criterion_satisfied);
    f.mutation();
    let cause = f.finish_worker();
    f.release(cause);
    f.verify();
    let view = f.view();
    assert!(
        view.runs[0].agents[1]
            .assignment
            .as_ref()
            .unwrap()
            .verification
            .as_ref()
            .unwrap()
            .1
            .passed
    );
    assert!(!view.goal(f.goal).unwrap().success_criterion_satisfied);
    f.close();
    let view = f.view();
    rejected(
        &f,
        Event::GoalFinished {
            goal_id: f.goal,
            status: GoalStatus::Completed,
            active_ms: view.goal(f.goal).unwrap().active_ms,
            report: GoalReport {
                summary: "done".into(),
                changes: Vec::new(),
                evidence: Vec::new(),
                unresolved: Vec::new(),
                cached_input_tokens: 0,
            },
            exhausted: false,
            budget_refusal: None,
        },
    );
}

fn second_foreground_wave(f: &mut Fixture) {
    let mut spawn = f
        .events
        .iter()
        .find(|event| {
            matches!(
                event,
                Event::AgentSpawned {
                    assignment: Some(_),
                    ..
                }
            )
        })
        .unwrap()
        .clone();
    let Event::AgentSpawned {
        agent_run_id,
        ordinal,
        assignment: Some(pins),
        ..
    } = &mut spawn
    else {
        unreachable!()
    };
    f.child = AgentRunId::new();
    *agent_run_id = f.child;
    *ordinal = 2;
    pins.request.wave = 2;
    pins.request.lease = vec!["src".into()];
    pins.request.criterion = GoalSuccessCriterion::WorkspaceFileContent {
        path: "src/b".into(),
        content: "new".into(),
    };
    pins.grant.write_paths = pins.request.lease.clone();
    let delegation = vec![ChildAssignment::Editor {
        assignment: Box::new(pins.request.clone()),
    }];
    f.call(
        f.primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::Delegated,
        Some(delegation),
    );
    f.push(spawn);
    f.changes.files[0].path = "src/b".into();
}

fn write_observation(f: &mut Fixture, actor: AgentRunId, before: &str, after: &str) -> u64 {
    f.call(
        actor,
        if actor == f.primary {
            AgentPhase::RootSynthesis
        } else {
            AgentPhase::ChildWork
        },
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        actor,
        ToolCall::Write {
            path: "src/a".into(),
            expected_digest: Some(crate::validate::hex_digest(before.as_bytes())),
            content: after.into(),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    f.observed(&intent, ToolDisposition::Succeeded, Some(Attestation::for_site(&intent)),
        &serde_json::json!({"sha256":crate::validate::hex_digest(after.as_bytes()),"bytes":after.len(),"executable":false,
            "before":{"sha256":crate::validate::hex_digest(before.as_bytes()),"bytes":before.len(),"executable":false}}).to_string())
}

fn verification_event(f: &Fixture, record: WorkerVerification) -> Event {
    Event::WorkerVerificationRecorded {
        run_id: f.run,
        agent_run_id: f.primary,
        record,
    }
}

fn verified(f: &Fixture, actor: AgentRunId) -> bool {
    f.view().runs[0]
        .agents
        .iter()
        .find(|agent| agent.id == actor)
        .unwrap()
        .assignment
        .as_ref()
        .unwrap()
        .verification
        .as_ref()
        .unwrap()
        .1
        .passed
}

#[test]
fn a_later_foreground_wave_invalidates_old_primary_verification_until_fresh_evidence() {
    let mut f = fixture();
    f.mutation();
    let cause = f.finish_worker();
    f.release(cause);
    f.verify();
    let first = f.child;
    let Event::WorkerVerificationRecorded {
        record: initial, ..
    } = f.events.last().unwrap()
    else {
        unreachable!()
    };
    let mut initial = initial.clone();
    second_foreground_wave(&mut f);
    let child = f.child;
    let child_evidence = write_observation(&mut f, child, "new", "changed");
    f.mutation();
    let cause = f.finish_worker();
    f.release(cause);
    rejected(&f, verification_event(&f, initial.clone()));
    let mut forged_child_evidence = initial.clone();
    forged_child_evidence.evidence = vec![child_evidence];
    rejected(&f, verification_event(&f, forged_child_evidence));
    initial.passed = false;
    f.push(verification_event(&f, initial.clone()));
    assert!(!verified(&f, first));
    let primary = f.primary;
    let fresh = write_observation(&mut f, primary, "changed", "new");
    initial.passed = true;
    initial.evidence = vec![fresh];
    f.push(verification_event(&f, initial));
    assert!(verified(&f, first));
    for end in 1..=f.events.len() {
        assert!(SessionView::replay(f.session, &f.envelopes()[..end]).is_ok());
    }
}

#[test]
fn primary_verification_tools_require_release_but_never_require_a_prior_verification() {
    let mut f = fixture();
    f.mutation();
    let cause = f.finish_worker();
    let view = f.view();
    assert!(!coordination_ready(&view.runs[0]));
    assert!(!wave_settled(&view.runs[0]));
    f.release(cause);
    let view = f.view();
    assert!(coordination_ready(&view.runs[0]));
    assert!(!wave_settled(&view.runs[0]));
    f.verify();
    let view = f.view();
    assert!(coordination_ready(&view.runs[0]));
    assert!(wave_settled(&view.runs[0]));
}
