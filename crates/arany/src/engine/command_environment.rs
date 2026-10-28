use super::*;
use crate::session::{
    EnvironmentCaptureEvent as Capture, EnvironmentCaptureIntent, EnvironmentCapturePhase,
    EnvironmentCaptureStage as Stage, RunStatus,
};
use crate::tools::unguarded::environment;
use crate::tools::{ApprovalReply, CommandEnvironment, CommandEnvironmentError};
use std::path::Path;

impl Engine {
    pub async fn capture_command_environment<F, A>(
        &self,
        session_id: SessionId,
        workspace: &Path,
        approve: F,
    ) -> Result<Stage, EngineError>
    where
        F: FnOnce(EnvironmentCaptureIntent) -> A,
        A: Future<Output = ApprovalReply>,
    {
        let _slot = self.claim()?;
        if self.config.guard_executable.is_none() {
            return Err(EngineError::GuardUnavailable);
        }
        let mut lock = lock_session(&self.state, session_id)?;
        let view = self
            .journal
            .store()
            .load_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        ensure_session_workspace(
            &view,
            WorkspaceInputs::admit_identity(workspace, &self.state)?,
        )?;
        if view.provider_setup_blocks_continuation() {
            return Err(EngineError::ProviderSetupBlocked);
        }
        if view
            .runs
            .last()
            .is_some_and(|run| matches!(run.status, RunStatus::Active | RunStatus::Pending))
        {
            return Err(EngineError::SessionNotIdle);
        }
        lock.keep();
        if let Some(capture) = view.environment_capture
            && !matches!(capture.phase, EnvironmentCapturePhase::Finished(_))
        {
            self.capture_event(
                session_id,
                Capture::Finished {
                    operation_id: capture.intent.operation_id,
                    stage: capture.outcome(),
                },
            )
            .await?;
        }
        let record = CommandEnvironment::load(&self.state)?;
        let intent = environment::intent(&record, workspace)?;
        self.capture_login_environment(session_id, record, intent, approve)
            .await
    }

    async fn capture_login_environment<F, A>(
        &self,
        session_id: SessionId,
        record: CommandEnvironment,
        intent: EnvironmentCaptureIntent,
        approve: F,
    ) -> Result<Stage, EngineError>
    where
        F: FnOnce(EnvironmentCaptureIntent) -> A,
        A: Future<Output = ApprovalReply>,
    {
        let operation_id = intent.operation_id;
        self.capture_event(
            session_id,
            Capture::Requested {
                intent: intent.clone(),
            },
        )
        .await?;
        let reply = approve(intent.clone()).await;
        if reply != ApprovalReply::AllowOnce {
            let stage = if reply == ApprovalReply::Deny {
                Stage::Declined
            } else {
                Stage::ApprovalUnavailable
            };
            self.capture_event(
                session_id,
                Capture::Finished {
                    operation_id,
                    stage,
                },
            )
            .await?;
            return Ok(stage);
        }
        self.capture_event(session_id, Capture::Approved { operation_id })
            .await?;
        let launcher = self
            .config
            .guard_executable
            .clone()
            .ok_or(EngineError::InvalidRequest)?;
        // A dropped caller leaves the bounded supervisor running to reap its process group, never to save or retry.
        let capture = tokio::spawn(environment::capture(launcher, intent, record.clone()))
            .await
            .unwrap_or(Err(Stage::Uncertain));
        let stage = match capture {
            Ok(captured) => {
                self.save_captured_environment(session_id, operation_id, &captured, record.sha256())
                    .await?
            }
            Err(stage) => stage,
        };
        if self
            .capture_event(
                session_id,
                Capture::Finished {
                    operation_id,
                    stage,
                },
            )
            .await
            .is_err()
        {
            return Ok(Stage::Uncertain);
        }
        Ok(stage)
    }

    async fn save_captured_environment(
        &self,
        session_id: SessionId,
        operation_id: uuid::Uuid,
        captured: &CommandEnvironment,
        expected: &str,
    ) -> Result<Stage, EngineError> {
        self.capture_event(
            session_id,
            Capture::Captured {
                operation_id,
                sha256: captured.sha256().into(),
            },
        )
        .await?;
        Ok(match captured.replace_captured(&self.state, expected) {
            Ok(()) => Stage::Saved,
            Err(CommandEnvironmentError::Unconfirmed) => Stage::Uncertain,
            Err(CommandEnvironmentError::Invalid) => Stage::PinnedInput,
            Err(_) => Stage::Save,
        })
    }

    async fn capture_event(
        &self,
        session_id: SessionId,
        record: Capture,
    ) -> Result<(), EngineError> {
        self.journal
            .append(session_id, Event::HostEnvironmentCapture { record })
            .await?;
        Ok(())
    }
}
