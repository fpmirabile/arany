use super::*;
use crate::test_support::worker_teams::RETIRED_WORKER_KINDS;

#[test]
fn live_editor_lifecycle_and_every_cold_prefix_keep_workspace_facts() {
    let f = completed_fixture();
    let events = f.envelopes();
    for end in 1..=events.len() {
        let cold = SessionView::replay(f.session, &events[..end])
            .unwrap()
            .unwrap();
        let decoded: Vec<_> = events[..end]
            .iter()
            .map(|e| EventEnvelope {
                event: Event::decode(
                    e.event.kind(),
                    &e.event.payload().unwrap(),
                    e.run_id,
                    e.agent_run_id,
                )
                .unwrap(),
                ..e.clone()
            })
            .collect();
        assert_eq!(
            SessionView::replay(f.session, &decoded).unwrap().unwrap(),
            cold
        );
        let Some(run) = cold.runs.first() else {
            continue;
        };
        if let Some(child) = run.agents.iter().find(|a| a.id == f.child) {
            let unknown = child.tools.iter().any(ToolView::outcome_unknown);
            if unknown {
                assert!(!cold.goal(f.goal).unwrap().can_resume(&cold));
            }
            if child.tools.iter().any(|t| {
                t.observation
                    .as_ref()
                    .is_some_and(|o| o.disposition == ToolDisposition::Succeeded)
            }) {
                assert_ne!(
                    run.worker_history()[0].publication,
                    WorkerPublication::Unpublished
                );
            }
        }
    }
    let view = f.view();
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    let w = view.runs[0].agents[1].assignment.as_ref().unwrap();
    assert!(w.settled());
    assert!(w.verification.as_ref().unwrap().1.passed);
    assert!(
        !f.events
            .iter()
            .any(|e| RETIRED_WORKER_KINDS.contains(&e.kind()))
    );
    let plan = crate::session::rewind::plan(&view, &view.rewinds.checkpoints[0]).unwrap();
    assert_eq!(plan.files.len(), 1);
    assert_eq!(plan.files[0].path, "src/a");
    assert!(plan.files[0].restore_to.is_none());
}

#[test]
fn live_replay_refuses_private_targets_and_old_release_fields() {
    let f = fixture();
    assert!(
        serde_json::from_value::<EffectTarget>(serde_json::json!({
            "kind": "worker_view",
            "assignment_id": f.child,
            "base_digest": vec![1_u8; 32],
            "revision": 0,
            "cache_generation": null,
        }))
        .is_err()
    );
    let data = serde_json::json!({
        "assignment_id": f.child,
        "purpose": "mutation",
        "tool_id": uuid::Uuid::now_v7(),
        "ordinal": 0,
        "bytes": 3,
        "sha256": crate::validate::digest(b"new"),
        "content": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"new"),
    })
    .to_string();
    assert!(matches!(
        Event::decode("WorkerDataChunk", &data, Some(f.run), Some(f.child)),
        Err(ReplayError::UnknownEvent)
    ));
    let release = serde_json::json!({"assignment_id":f.child,"cause_sequence":1,"handover_sequence":null,"reason":"integrated"}).to_string();
    assert!(
        Event::decode(
            "WorkerLeaseReleased",
            &release,
            Some(f.run),
            Some(f.primary)
        )
        .is_err()
    );
}

#[test]
fn researcher_completion_cannot_retain_editor_checkpoint_metadata() {
    let mut f = fixture();
    f.call(
        f.child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::ToolRequested,
        None,
    );
    let intent = f.intent(
        f.child,
        ToolCall::Read {
            path: "src/a".into(),
            offset: None,
            limit: Some(32),
        },
        EffectTarget::Workspace,
    );
    f.started(&intent);
    let mut view = f.view();
    view.runs[0].agents[1].assignment = None;
    let mut event = Event::ToolFinished {
        run_id: f.run,
        agent_run_id: f.child,
        checkpoint: Some(crate::Checkpoint::not_needed()),
        observation: ToolObservation {
            spill: None,
            intent: intent.clone(),
            disposition: ToolDisposition::Succeeded,
            attestation: Some(Attestation::for_site(&intent)),
            output: "{}".into(),
            enforcement_cause: None,
            limit_cause: None,
            approval_cause: None,
            guard_stage: None,
            cache_update: CacheUpdate::NotApplicable,
        },
    };
    assert_eq!(
        view.finish_child_tool(&event, f.events.len() as u64 + 1),
        Err(ReplayError::InvalidTransition)
    );
    if let Event::ToolFinished { checkpoint, .. } = &mut event {
        *checkpoint = None;
    }
    assert!(
        view.finish_child_tool(&event, f.events.len() as u64 + 1)
            .is_ok()
    );
}
