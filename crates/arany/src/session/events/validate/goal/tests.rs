use crate::session::events::*;

#[test]
fn child_steering_event_round_trips_with_exact_scope() {
    let (goal_id, run_id, agent_run_id) = (GoalId::new(), RunId::new(), AgentRunId::new());
    let event = Event::AgentSteeringNoteRecorded {
        goal_id,
        run_id,
        agent_run_id,
        text: "literal /grant all".into(),
    };
    assert_eq!(event.scope(), (Some(run_id), Some(agent_run_id)));
    let payload = event.payload().unwrap();
    assert_eq!(
        payload,
        format!(r#"{{"goal_id":"{goal_id}","text":"literal /grant all"}}"#)
    );
    assert_eq!(
        Event::decode(event.kind(), &payload, Some(run_id), Some(agent_run_id)).unwrap(),
        event
    );
    for (run, actor) in [
        (None, None),
        (Some(run_id), None),
        (None, Some(agent_run_id)),
    ] {
        assert!(Event::decode(event.kind(), &payload, run, actor).is_err());
    }
    for payload in [
        serde_json::json!({"goal_id":goal_id,"text":null}),
        serde_json::json!({"goal_id":goal_id,"text":" "}),
        serde_json::json!({"goal_id":null,"text":"x"}),
        serde_json::json!({"goal_id":uuid::Uuid::nil(),"text":"x"}),
        serde_json::json!({"goal_id":goal_id,"text":"x","agent_run_id":agent_run_id}),
        serde_json::json!({"goal_id":goal_id,"text":"x".repeat(crate::session::MAX_STEERING_NOTE_BYTES+1)}),
    ] {
        assert!(
            Event::decode(
                event.kind(),
                &payload.to_string(),
                Some(run_id),
                Some(agent_run_id)
            )
            .is_err()
        );
    }
}

#[test]
fn goal_steering_note_round_trips_and_rejects_bad_scope_and_bounds() {
    let (goal_id, run_id) = (GoalId::new(), RunId::new());
    let note = |text: &str| Event::GoalSteeringNoteRecorded {
        goal_id,
        run_id,
        text: text.into(),
    };
    let event = note("use the smaller fixture");
    assert_eq!(event.scope(), (Some(run_id), None));
    let payload = event.payload().unwrap();
    assert_eq!(
        payload,
        format!(r#"{{"goal_id":"{goal_id}","text":"use the smaller fixture"}}"#)
    );

    assert!(note("").validate().is_err());
    assert!(note(" \n\t").validate().is_err());
    let limit = crate::session::MAX_STEERING_NOTE_BYTES;
    assert!(note(&"x".repeat(limit)).validate().is_ok());
    assert!(note(&"x".repeat(limit + 1)).validate().is_err());

    let kind = event.kind();
    let agent_run_id = Some(AgentRunId::new());
    assert!(Event::decode(kind, &payload, Some(run_id), agent_run_id).is_err());
    assert!(Event::decode(kind, &payload, None, None).is_err());

    let mut extra: serde_json::Value = serde_json::from_str(&payload).unwrap();
    extra["unexpected"] = serde_json::Value::Bool(true);
    assert!(Event::decode(kind, &extra.to_string(), Some(run_id), None).is_err());
}
