use super::*;

#[cfg(unix)]
mod goal_workspace_lease_tests {
    use super::*;
    use crate::provider::{ProviderError, ProviderResponse};
    use std::time::Duration;

    struct NeverProvider;

    #[async_trait::async_trait]
    impl Provider for NeverProvider {
        fn identity(&self) -> &crate::provider::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<crate::provider::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    crate::provider::ProviderIdentity::new(
                        "scripted",
                        "lease-test",
                        crate::provider::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(&self, _: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
            panic!("a dropped Goal admission must not call the Provider")
        }
    }

    fn submit(engine: &Engine, request: RunRequest) -> RunFuture<'_> {
        engine
            .submit(
                crate::provider::AdmittedProviders::single(Arc::new(NeverProvider)),
                request,
                RunControls::default(),
            )
            .expect("idle Engine")
            .result
    }

    #[tokio::test]
    async fn dropped_start_acknowledgment_releases_the_lease_once_the_goal_is_paused() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state_path = temp.path().join("state-a");
        let second_state_path = temp.path().join("state-b");
        let account_root = temp.path().join("account");
        let state = StateRoot::admit(&state_path).expect("private State root");
        let second_state = StateRoot::admit(&second_state_path).expect("second private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = (7, 11);
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Goal lease acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");

        let workspace_lock = engine
            .state
            .lock_workspace_in(&account_root, workspace_identity)
            .expect("provisional Workspace lock");
        let (acknowledgment_entered, release_acknowledgment) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalStarted { .. } | Event::GoalResumed { .. })
            });
        let mut leases = WorkspaceLeases::new();
        Engine::retain_goal_workspace(
            &mut leases,
            goal_id,
            workspace_identity,
            Some(workspace_lock),
        );
        let mut admission = Box::pin(engine.append_goal_lifecycle(
            session_id,
            None,
            Event::GoalStarted {
                goal_id,
                objective: "Keep the Workspace lease through Store acknowledgment".into(),
                workspace_identity,
                budget: GoalBudget::default(),
                phase: GoalPhase::Implementation,
                autonomous: false,
                resource_settings: crate::tools::ToolResourceSettings::default(),
            },
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut admission => panic!("append returned before its controlled acknowledgment: {result:?}"),
                result = acknowledgment_entered => result.expect("committed append reached acknowledgment gate"),
            }
        })
        .await
        .expect("Goal append acknowledgment gate");
        drop(admission);
        release_acknowledgment
            .send(())
            .expect("release Store acknowledgment");

        assert!(matches!(
            second_state.lock_workspace_in(&account_root, workspace_identity),
            Err(StoreError::WorkspaceBusy)
        ));
        // The call ends with its dropped future; the committed Goal is paused and no longer holds the writer lock.
        drop(leases);
        let view = engine
            .journal
            .store()
            .load_view(session_id)
            .await
            .expect("canonical Session view")
            .expect("Session exists");
        assert_eq!(view.interrupted_goal_id(), Some(goal_id));
        let writer = second_state
            .lock_workspace_in(&account_root, workspace_identity)
            .expect("paused Goal releases the Workspace writer lock");
        drop(writer);
        let events = engine
            .journal
            .store()
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "no Run or effect was admitted");
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn lost_run_link_acknowledgment_is_admission_uncertain() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state_path = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&state_path).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let session_id = SessionId::new();
        let workspace_identity = crate::tools::workspace_identity(&workspace).expect("identity");
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Lost link acknowledgment".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        let (link_entered, release_link) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalRunLinked { .. })
            });
        let request = RunRequest {
            session_id: Some(session_id),
            title: None,
            objective: "Link the Run before its acknowledgment is lost".into(),
            images: Vec::new(),
            workspace: workspace.clone(),
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: GoalRequest::default(),
        };
        let mut run = submit(&engine, request);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("Run returned before its link was held: {}", result.is_ok()),
                result = link_entered => result.expect("link reached the acknowledgment gate"),
            }
        })
        .await
        .expect("link acknowledgment gate");
        drop(release_link);
        match run.await {
            Err(EngineError::GoalAdmissionUncertain) => {}
            Err(error) => panic!("a possibly committed link must be uncertain, not {error}"),
            Ok(_) => panic!("Run completed without an acknowledged link"),
        }
        engine.close().await.expect("close Engine");
    }

    const RESUMABLE_OBJECTIVE: &str = "Resume the paused Goal after a failed append";

    async fn paused_goal_session(
        engine: &Engine,
        workspace: &std::path::Path,
    ) -> (SessionId, GoalId) {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let workspace_identity = crate::tools::workspace_identity(workspace).expect("identity");
        engine
            .journal
            .append(
                session_id,
                Event::SessionStarted {
                    title: "Resumable Goal".into(),
                    workspace_identity,
                },
            )
            .await
            .expect("Session start");
        engine
            .journal
            .append(
                session_id,
                Event::GoalStarted {
                    goal_id,
                    objective: RESUMABLE_OBJECTIVE.into(),
                    workspace_identity,
                    budget: GoalBudget::default(),
                    phase: GoalPhase::Implementation,
                    autonomous: false,
                    resource_settings: crate::tools::ToolResourceSettings::default(),
                },
            )
            .await
            .expect("Goal start");
        (session_id, goal_id)
    }

    fn resume_request(
        session_id: SessionId,
        goal_id: GoalId,
        workspace: &std::path::Path,
    ) -> RunRequest {
        RunRequest {
            session_id: Some(session_id),
            title: None,
            objective: RESUMABLE_OBJECTIVE.into(),
            images: Vec::new(),
            workspace: workspace.to_path_buf(),
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: GoalRequest {
                id: Some(goal_id),
                phase: None,
                budget: None,
                resource_settings: crate::tools::ToolResourceSettings::default(),
                finish_after_run: true,
                autonomous: false,
            },
        }
    }

    #[tokio::test]
    async fn goal_resume_failure_before_commit_is_store_error() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&engine, &workspace).await;
        crate::fault::FaultPlan::fail_goal_append(session_id, |event| {
            matches!(event, Event::GoalResumed { .. })
        });
        match submit(&engine, resume_request(session_id, goal_id, &workspace)).await {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted resume must report its Store error, not {error}"),
            Ok(_) => panic!("Run resumed without a committed GoalResumed"),
        }
        let events = engine
            .journal
            .store()
            .load_session(session_id)
            .await
            .expect("canonical Events");
        assert_eq!(events.len(), 2, "GoalResumed did not commit");
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn lost_goal_resume_acknowledgment_is_admission_uncertain() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&engine, &workspace).await;
        let (resume_entered, release_resume) =
            crate::fault::FaultPlan::gate_goal_append_acknowledgment(session_id, |event| {
                matches!(event, Event::GoalResumed { .. })
            });
        let mut run = submit(&engine, resume_request(session_id, goal_id, &workspace));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut run => panic!("Run returned before its resume was held: {}", result.is_ok()),
                result = resume_entered => result.expect("resume reached the acknowledgment gate"),
            }
        })
        .await
        .expect("resume acknowledgment gate");
        drop(release_resume);
        match run.await {
            Err(EngineError::GoalAdmissionUncertain) => {}
            Err(error) => panic!("a possibly committed resume must be uncertain, not {error}"),
            Ok(_) => panic!("Run completed without an acknowledged resume"),
        }
        engine.close().await.expect("close Engine");
    }

    #[tokio::test]
    async fn goal_run_link_failure_before_commit_is_store_error() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private State root");
        let engine = Engine::open(state, EngineConfig::default()).expect("Engine");
        let (session_id, goal_id) = paused_goal_session(&engine, &workspace).await;
        crate::fault::FaultPlan::fail_goal_append(session_id, |event| {
            matches!(event, Event::GoalRunLinked { .. })
        });
        match submit(&engine, resume_request(session_id, goal_id, &workspace)).await {
            Err(EngineError::Store(_)) => {}
            Err(error) => panic!("an uncommitted link must report its Store error, not {error}"),
            Ok(_) => panic!("Run completed without a committed link"),
        }
        engine.close().await.expect("close Engine");
    }
}

mod run_steering_tests {
    use super::*;

    #[test]
    fn run_steering_merges_waiting_notes_and_bounds_the_run() {
        let steering = RunSteering::new();
        assert_eq!(
            steering.offer_note(" \n".into()),
            Err(SteeringRefused::Empty)
        );
        assert_eq!(
            steering.offer_note("x".repeat(MAX_STEERING_NOTE_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        // A note that can never fit with its label is TooLarge, not a full Run.
        assert_eq!(
            steering.offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES + 1)),
            Err(SteeringRefused::TooLarge)
        );
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("the largest single note fits an empty Run");
        assert!(steering.withdraw_note().is_some());
        steering.offer_note("first".into()).expect("first note");
        steering.offer_note("second".into()).expect("merged note");
        assert_eq!(
            steering.withdraw_note().as_deref(),
            Some("first\n\nsecond"),
            "withdrawal returns the whole merged text"
        );
        assert!(
            steering.withdraw_note().is_none(),
            "nothing is waiting twice"
        );

        steering.offer_note("one".into()).expect("note offered");
        steering.offer_note("two".into()).expect("note merged");
        assert_eq!(steering.take_note().as_deref(), Some("one\n\ntwo"));
        assert!(
            steering.withdraw_note().is_none(),
            "a note the Engine took cannot be withdrawn"
        );
        assert_eq!(steering.delivered_notes(), 0);
        steering.note_recorded();
        assert_eq!(steering.delivered_notes(), 1);

        let taken =
            "x".repeat(MAX_STEERING_NOTE_BYTES - "one\n\ntwo".len() - 2 * MAX_STEERING_LABEL_BYTES);
        steering
            .offer_note(taken.clone())
            .expect("fills the Run bound");
        assert_eq!(steering.take_note(), Some(taken));
        steering.note_recorded();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );
        steering.begin_run();
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("a new Run has its own byte quota");
        assert!(steering.take_note().is_some());
        steering.note_recorded();
        assert_eq!(
            steering.offer_note("y".into()),
            Err(SteeringRefused::RunFull)
        );

        assert!(steering.pause());
        assert!(!steering.pause(), "only the first pause request is new");
        assert!(steering.pause_requested());
    }

    #[test]
    fn run_steering_charges_a_label_per_note_and_refuses_the_next_tiny_note() {
        let steering = RunSteering::new();
        let mut taken = 0;
        while steering.offer_note("n".into()).is_ok() {
            assert!(steering.take_note().is_some());
            taken += 1;
        }
        assert_eq!(
            taken,
            MAX_STEERING_NOTE_BYTES / (1 + MAX_STEERING_LABEL_BYTES)
        );
        assert_eq!(
            steering.offer_note("n".into()),
            Err(SteeringRefused::RunFull),
            "the next one-byte note exceeds the bound with its label"
        );
    }

    #[test]
    fn run_steering_take_reserves_quota_and_poisoned_lock_fails_closed() {
        let steering = RunSteering::new();
        steering
            .offer_note("x".repeat(MAX_STEERING_NOTE_BYTES - MAX_STEERING_LABEL_BYTES))
            .expect("within quota");
        assert!(steering.take_note().is_some());
        assert_eq!(
            steering.offer_note("unrecorded takes still count".into()),
            Err(SteeringRefused::RunFull)
        );

        let poisoned = RunSteering::new();
        let handle = poisoned.clone();
        let _ = std::thread::spawn(move || {
            let _guard = handle.state.lock().expect("unpoisoned");
            panic!("poison the steering lock");
        })
        .join();
        assert_eq!(
            poisoned.offer_note("x".into()),
            Err(SteeringRefused::Unavailable)
        );
        assert!(poisoned.take_note().is_none());
        assert_eq!(poisoned.delivered_notes(), 0);
    }
}
