use super::*;
use crate::{Event, EventEnvelope, SessionId, WorkspaceIdentity};

pub(crate) fn intent() -> EnvironmentCaptureIntent {
    EnvironmentCaptureIntent {
        operation_id: Uuid::now_v7(),
        shell: LoginShellIdentity {
            path: "/bin/sh".into(),
            device: 1,
            inode: 2,
            sha256: "1".repeat(64),
        },
        home: "/home/owner".into(),
        home_device: 1,
        home_inode: 3,
        environment_sha256: "2".repeat(64),
    }
}

fn append(events: &mut Vec<EventEnvelope>, session: SessionId, record: EnvironmentCaptureEvent) {
    let event = Event::HostEnvironmentCapture { record };
    let payload = event.payload().unwrap();
    let decoded = Event::decode(event.kind(), event.version(), &payload, None, None).unwrap();
    assert_eq!(decoded, event);
    events.push(EventEnvelope {
        sequence: events.len() as u64 + 1,
        session_id: session,
        run_id: None,
        agent_run_id: None,
        event,
        created_at_ms: 0,
    });
}

#[test]
fn capture_requires_fresh_ordered_authority_and_replay_retains_unknown_without_execution() {
    let session = SessionId::new();
    let mut events = vec![EventEnvelope {
        sequence: 1,
        session_id: session,
        run_id: None,
        agent_run_id: None,
        event: Event::SessionStarted {
            title: "Capture".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 1,
            },
        },
        created_at_ms: 0,
    }];
    let intent = intent();
    let operation_id = intent.operation_id;
    append(
        &mut events,
        session,
        EnvironmentCaptureEvent::Requested {
            intent: intent.clone(),
        },
    );
    let requested = SessionView::replay(session, &events).unwrap().unwrap();
    for record in [
        EnvironmentCaptureEvent::Captured {
            operation_id,
            sha256: "3".repeat(64),
        },
        EnvironmentCaptureEvent::Finished {
            operation_id,
            stage: EnvironmentCaptureStage::Saved,
        },
    ] {
        let mut view = requested.clone();
        assert!(apply(&mut view, &record).is_err());
    }
    append(
        &mut events,
        session,
        EnvironmentCaptureEvent::Approved { operation_id },
    );
    let approved = SessionView::replay(session, &events).unwrap().unwrap();
    assert_eq!(
        approved.environment_capture.as_ref().unwrap().outcome(),
        EnvironmentCaptureStage::Uncertain
    );
    assert!(
        apply(
            &mut approved.clone(),
            &EnvironmentCaptureEvent::Approved { operation_id }
        )
        .is_err()
    );
    assert!(
        apply(
            &mut approved.clone(),
            &EnvironmentCaptureEvent::Finished {
                operation_id: Uuid::now_v7(),
                stage: EnvironmentCaptureStage::Uncertain
            }
        )
        .is_err()
    );
    append(
        &mut events,
        session,
        EnvironmentCaptureEvent::Captured {
            operation_id,
            sha256: "3".repeat(64),
        },
    );
    let captured = SessionView::replay(session, &events).unwrap().unwrap();
    assert_eq!(
        captured.environment_capture.as_ref().unwrap().outcome(),
        EnvironmentCaptureStage::Uncertain
    );
    append(
        &mut events,
        session,
        EnvironmentCaptureEvent::Finished {
            operation_id,
            stage: EnvironmentCaptureStage::Uncertain,
        },
    );
    let closed = SessionView::replay(session, &events).unwrap().unwrap();
    assert_eq!(
        closed.environment_capture.unwrap().outcome(),
        EnvironmentCaptureStage::Uncertain
    );
}
