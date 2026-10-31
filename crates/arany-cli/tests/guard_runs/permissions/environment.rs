use super::*;
use arany::{
    CommandEnvironment, EnvironmentCaptureEvent, EnvironmentCapturePhase, EnvironmentCaptureStage,
};
use std::os::unix::fs::MetadataExt;

#[tokio::test]
async fn login_capture_decline_is_journaled_and_each_capture_needs_a_fresh_human() {
    let login_shell = nix::unistd::User::from_uid(nix::unistd::geteuid())
        .unwrap()
        .unwrap()
        .shell;
    if !matches!(
        login_shell.file_name().and_then(|name| name.to_str()),
        Some("zsh" | "bash" | "sh")
    ) {
        eprintln!("skipped: the login shell is not one that environment capture supports");
        return;
    }
    let fixture = Fixture::new(None, false);
    let session = fixture
        .engine
        .create_session(fixture.workspace.clone(), None)
        .await
        .unwrap();
    let root = StateRoot::open_existing(&fixture.state).unwrap();
    CommandEnvironment::load(&root).unwrap();
    let file = fixture.state.join("command-environment.json");
    let before = std::fs::read(&file).unwrap();
    if login_shell.metadata().unwrap().nlink() != 1 {
        let result = fixture
            .engine
            .capture_command_environment(
                session,
                &fixture.workspace,
                |_| -> std::future::Ready<arany::ApprovalReply> {
                    panic!("an unadmitted shell must never request execution approval")
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(arany::EngineError::Tool(arany::ToolError::Path))
        ));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        fixture.engine.close().await.unwrap();
        let (events, view) = arany::test_support::goals::replayed(&fixture.state, session).await;
        assert!(view.environment_capture.is_none());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::HostEnvironmentCapture { .. }))
        );
        return;
    }
    let mut asks = 0;
    for _ in 0..2 {
        let stage = fixture
            .engine
            .capture_command_environment(session, &fixture.workspace, |intent| {
                asks += 1;
                assert_eq!(
                    Path::new(&intent.shell.path),
                    login_shell.canonicalize().unwrap()
                );
                std::future::ready(arany::ApprovalReply::Deny)
            })
            .await
            .unwrap();
        assert_eq!(stage, EnvironmentCaptureStage::Declined);
        assert_eq!(std::fs::read(&file).unwrap(), before);
    }
    assert_eq!(asks, 2);
    let store = Store::open(StateRoot::open_existing(&fixture.state).unwrap()).unwrap();
    let mut intent = store
        .load_view(session)
        .await
        .unwrap()
        .unwrap()
        .environment_capture
        .unwrap()
        .intent;
    intent.operation_id = uuid::Uuid::now_v7();
    let operation_id = intent.operation_id;
    for record in [
        EnvironmentCaptureEvent::Requested { intent },
        EnvironmentCaptureEvent::Approved { operation_id },
    ] {
        store
            .append(session, Event::HostEnvironmentCapture { record })
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let stage = fixture
        .engine
        .capture_command_environment(session, &fixture.workspace, |_| {
            std::future::ready(arany::ApprovalReply::Unavailable)
        })
        .await
        .unwrap();
    assert_eq!(stage, EnvironmentCaptureStage::ApprovalUnavailable);
    assert_eq!(std::fs::read(&file).unwrap(), before);
    fixture.engine.close().await.unwrap();
    let (events, view) = arany::test_support::goals::replayed(&fixture.state, session).await;
    let phases: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::HostEnvironmentCapture { record } => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(phases.len(), 9);
    assert!(matches!(
        phases[6],
        EnvironmentCaptureEvent::Finished {
            stage: EnvironmentCaptureStage::Uncertain,
            ..
        }
    ));
    assert!(matches!(
        phases[0],
        EnvironmentCaptureEvent::Requested { .. }
    ));
    assert!(matches!(
        phases[1],
        EnvironmentCaptureEvent::Finished {
            stage: EnvironmentCaptureStage::Declined,
            ..
        }
    ));
    assert_eq!(
        view.environment_capture.unwrap().phase,
        EnvironmentCapturePhase::Finished(EnvironmentCaptureStage::ApprovalUnavailable)
    );
}
