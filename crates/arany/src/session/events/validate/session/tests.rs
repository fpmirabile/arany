use crate::session::events::*;

#[test]
fn provider_setup_applied_requires_a_resulting_digest() {
    let operation_id = uuid::Uuid::now_v7();
    let invalid = Event::ProviderSetupEditFinished {
        operation_id,
        resulting_digest: None,
        disposition: ProviderSetupDisposition::Applied,
    };
    assert!(invalid.payload().is_err());
    let payload = format!(
        r#"{{"operation_id":"{operation_id}","resulting_digest":null,"disposition":"Applied"}}"#
    );
    assert!(Event::decode(invalid.kind(), 1, &payload, None, None).is_err());
}

#[test]
fn provider_setup_current_payloads_require_explicit_nullable_digests() {
    let operation_id = uuid::Uuid::now_v7();
    let started = Event::ProviderSetupEditStarted {
        operation_id,
        target: ProviderSetupTarget {
            kind: ProviderSetupTargetKind::CustomProfile,
            identity: "example-route".into(),
        },
        expected_digest: None,
    };
    let mut cases = vec![(started, "expected_digest")];
    for disposition in [
        ProviderSetupDisposition::Failed,
        ProviderSetupDisposition::Cancelled,
        ProviderSetupDisposition::Uncertain,
    ] {
        cases.push((
            Event::ProviderSetupEditFinished {
                operation_id,
                resulting_digest: None,
                disposition,
            },
            "resulting_digest",
        ));
    }
    for (event, field) in cases {
        let payload = event.payload().unwrap();
        assert_eq!(
            Event::decode(
                event.kind(),
                payload_version(event.kind()).unwrap(),
                &payload,
                None,
                None
            )
            .unwrap(),
            event
        );
        let mut document: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert!(document[field].is_null());
        document.as_object_mut().unwrap().remove(field);
        let omitted = serde_json::to_string(&document).unwrap();
        assert!(
            Event::decode(
                event.kind(),
                payload_version(event.kind()).unwrap(),
                &omitted,
                None,
                None
            )
            .is_err(),
            "{field} must be present"
        );
    }
}

#[test]
fn provider_setup_events_use_registered_current_payloads() {
    let events = [
        Event::ProviderSetupEditStarted {
            operation_id: uuid::Uuid::parse_str("01900000-0000-7000-8000-000000000001")
                .expect("operation ID"),
            target: ProviderSetupTarget {
                kind: ProviderSetupTargetKind::Roster,
                identity: "roster".into(),
            },
            expected_digest: None,
        },
        Event::ProviderSetupEditStarted {
            operation_id: uuid::Uuid::parse_str("01900000-0000-7000-8000-000000000001")
                .expect("operation ID"),
            target: ProviderSetupTarget {
                kind: ProviderSetupTargetKind::RoleRouting,
                identity: "roles".into(),
            },
            expected_digest: None,
        },
        Event::ProviderSetupEditStarted {
            operation_id: uuid::Uuid::parse_str("01900000-0000-7000-8000-000000000001")
                .expect("operation ID"),
            target: ProviderSetupTarget {
                kind: ProviderSetupTargetKind::CustomProfile,
                identity: "example-route".into(),
            },
            expected_digest: None,
        },
        Event::ProviderSetupEditFinished {
            operation_id: uuid::Uuid::parse_str("01900000-0000-7000-8000-000000000001")
                .expect("operation ID"),
            resulting_digest: Some([1; 32]),
            disposition: ProviderSetupDisposition::Applied,
        },
    ];
    let fixtures = [
        r#"{"operation_id":"01900000-0000-7000-8000-000000000001","target":{"kind":"Roster","identity":"roster"},"expected_digest":null}"#,
        r#"{"operation_id":"01900000-0000-7000-8000-000000000001","target":{"kind":"RoleRouting","identity":"roles"},"expected_digest":null}"#,
        r#"{"operation_id":"01900000-0000-7000-8000-000000000001","target":{"kind":"CustomProfile","identity":"example-route"},"expected_digest":null}"#,
        r#"{"operation_id":"01900000-0000-7000-8000-000000000001","resulting_digest":[1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1],"disposition":"Applied"}"#,
    ];
    for (event, fixture) in events.into_iter().zip(fixtures) {
        let payload = event.payload().expect("setup payload");
        let version = payload_version(event.kind()).expect("registered setup kind");
        assert_eq!(
            version,
            if event.kind() == "ProviderSetupEditStarted" {
                3
            } else {
                1
            }
        );
        assert_eq!(payload, fixture);
        assert_eq!(
            Event::decode(event.kind(), version, &payload, None, None).expect("decode"),
            event
        );
    }
}

#[test]
fn accepted_message_requires_current_nullable_skill() {
    let run_id = Some(RunId::new());

    for skill in [serde_json::Value::Null, serde_json::json!("ask-matt")] {
        let payload = serde_json::json!({"text":"do X", "images":[], "skill":skill});
        let event =
            Event::decode("MessageAccepted", 7, &payload.to_string(), run_id, None).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&event.payload().unwrap()).unwrap(),
            payload
        );
        assert!(Event::decode("MessageAccepted", 6, &payload.to_string(), run_id, None).is_err());
    }
    for fixture in [
        r#"{"skill":null,"text":"do X","images":[]}"#,
        r#"{"skill":"ask-matt","text":"do X","images":[]}"#,
    ] {
        assert_eq!(
            Event::decode("MessageAccepted", 7, fixture, run_id, None)
                .unwrap()
                .payload()
                .unwrap(),
            fixture
        );
        assert!(Event::decode("MessageAccepted", 6, fixture, run_id, None).is_err());
    }
    for payload in [
        serde_json::json!({"text":"do X", "images":[]}),
        serde_json::json!({"text":"do X", "images":[], "skill":"../escape"}),
        serde_json::json!({"text":"do X", "images":[], "skill":false}),
    ] {
        assert!(Event::decode("MessageAccepted", 7, &payload.to_string(), run_id, None).is_err());
    }
}

#[test]
fn clearing_event_has_one_strict_current_payload() {
    let run = RunId::new();
    let event = Event::ToolResultsCleared {
        run_id: run,
        covered_tool_count: 4,
        cleared_bytes: 1234,
    };
    let current = event.version();
    let payload = event.payload().unwrap();
    assert_eq!(
        Event::decode(event.kind(), current, &payload, Some(run), None).unwrap(),
        event
    );
    for (version, body, agent) in [
        (current - 1, payload.clone(), None),
        (current + 1, payload.clone(), None),
        (current, payload.clone(), Some(AgentRunId::new())),
        (current, r#"{"covered_tool_count":4}"#.into(), None),
        (
            current,
            r#"{"covered_tool_count":0,"cleared_bytes":1}"#.into(),
            None,
        ),
        (
            current,
            r#"{"covered_tool_count":4,"cleared_bytes":1,"extra":true}"#.into(),
            None,
        ),
    ] {
        assert!(Event::decode(event.kind(), version, &body, Some(run), agent).is_err());
    }
}
