use crate::session::events::*;

#[test]
fn child_grant_round_trips_and_rejects_missing_or_misplaced_pins() {
    let (run_id, agent_run_id) = (RunId::new(), AgentRunId::new());
    let spawned = |role, ordinal, objective: Option<&str>, grant| Event::AgentSpawned {
        profile: None,
        assignment: None,
        definition: None,
        run_id,
        agent_run_id,
        role,
        ordinal,
        objective: objective.map(String::from),
        grant,
    };
    let decode = |event: &Event, payload: &str| {
        Event::decode(
            event.kind(),
            payload_version(event.kind()).expect("payload version"),
            payload,
            Some(run_id),
            Some(agent_run_id),
        )
    };
    for grant in [
        ChildGrant::NoTools,
        ChildGrant::ReadOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        },
    ] {
        let event = spawned(AgentRole::Child, 1, Some("inspect"), Some(grant));
        let payload = event.payload().expect("child payload");
        assert_eq!(decode(&event, &payload).expect("child replay"), event);
    }
    let primary = spawned(AgentRole::Primary, 0, None, None);
    let payload = primary.payload().expect("primary payload");
    assert_eq!(decode(&primary, &payload).expect("primary replay"), primary);

    let child = spawned(
        AgentRole::Child,
        1,
        Some("inspect"),
        Some(ChildGrant::ReadOnly {
            unguarded: crate::tools::UnguardedGrant::Denied,
        }),
    );
    let payload = child.payload().expect("child payload");
    let mut named = child.clone();
    if let Event::AgentSpawned { profile, .. } = &mut named {
        *profile = Some("inspect".into());
    }
    assert_eq!(decode(&named, &named.payload().unwrap()).unwrap(), named);
    let mut value: serde_json::Value = serde_json::from_str(&payload).expect("JSON");
    for profile in [
        serde_json::json!("../inspect"),
        serde_json::json!(""),
        serde_json::json!("x".repeat(65)),
        serde_json::json!(1),
    ] {
        let mut invalid = value.clone();
        invalid["profile"] = profile;
        assert!(decode(&child, &invalid.to_string()).is_err());
    }
    assert!(
        Event::decode(
            child.kind(),
            child.version() - 1,
            &payload,
            Some(run_id),
            Some(agent_run_id)
        )
        .is_err_and(|error| matches!(error, ReplayError::UnknownEvent))
    );
    let mut missing = value.clone();
    missing.as_object_mut().expect("object").remove("profile");
    assert!(decode(&child, &missing.to_string()).is_err());
    missing = value.clone();
    missing.as_object_mut().expect("object").remove("grant");
    assert!(decode(&child, &missing.to_string()).is_err());
    for invalid in [
        serde_json::json!("read_only"),
        serde_json::json!({"read_only":{}}),
        serde_json::json!({"read_only":{"unguarded":"human_only","automatic":true}}),
    ] {
        let mut forged = value.clone();
        forged["grant"] = invalid;
        assert!(decode(&child, &forged.to_string()).is_err());
    }
    value["grant"] = serde_json::Value::Null;
    assert!(decode(&child, &value.to_string()).is_err());
    value["grant"] = "write".into();
    assert!(decode(&child, &value.to_string()).is_err());

    let primary_payload = primary.payload().expect("primary payload");
    let mut primary_value: serde_json::Value =
        serde_json::from_str(&primary_payload).expect("JSON");
    primary_value["grant"] = "no_tools".into();
    assert!(decode(&primary, &primary_value.to_string()).is_err());
}
