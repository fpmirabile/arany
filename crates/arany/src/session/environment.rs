use super::{ReplayError, SessionView};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginShellIdentity {
    pub path: String,
    pub device: u64,
    pub inode: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentCaptureIntent {
    pub operation_id: Uuid,
    pub shell: LoginShellIdentity,
    pub home: String,
    pub home_device: u64,
    pub home_inode: u64,
    pub environment_sha256: String,
}

impl EnvironmentCaptureIntent {
    pub const RUNTIME_MS: u64 =
        crate::tools::unguarded::environment::LOGIN_SHELL_CAPTURE_TIMEOUT_MS;
    pub const OUTPUT_BYTES: usize =
        crate::tools::unguarded::environment::LOGIN_SHELL_CAPTURE_OUTPUT_BYTES;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentCaptureStage {
    Declined,
    ApprovalUnavailable,
    PinnedInput,
    Spawn,
    Exit,
    Timeout,
    OutputLimit,
    InvalidEnvironment,
    Save,
    Uncertain,
    Saved,
}

impl EnvironmentCaptureStage {
    pub fn summary(self) -> &'static str {
        match self {
            Self::Saved => "Login-shell environment captured; it applies from the next Run",
            Self::Uncertain => {
                "Host login-shell capture outcome is unknown; no automatic retry. Reopen /permissions to verify the record"
            }
            Self::Declined => "Login-shell capture declined; command environment unchanged",
            Self::ApprovalUnavailable => {
                "Login-shell capture approval unavailable; command environment unchanged"
            }
            Self::PinnedInput => {
                "Login-shell capture refused: pinned input changed; command environment unchanged"
            }
            Self::Spawn => "Login-shell capture could not start; command environment unchanged",
            Self::Exit => {
                "Login-shell capture failed: non-zero exit; command environment unchanged"
            }
            Self::Timeout => {
                "Login-shell capture stopped: runtime bound; command environment unchanged"
            }
            Self::OutputLimit => {
                "Login-shell capture stopped: output cap; command environment unchanged"
            }
            Self::InvalidEnvironment => {
                "Login-shell capture failed: invalid environment; command environment unchanged"
            }
            Self::Save => "Login-shell capture could not save; command environment unchanged",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentCaptureEvent {
    Requested {
        intent: EnvironmentCaptureIntent,
    },
    Approved {
        operation_id: Uuid,
    },
    Captured {
        operation_id: Uuid,
        sha256: String,
    },
    Finished {
        operation_id: Uuid,
        stage: EnvironmentCaptureStage,
    },
}

impl EnvironmentCaptureEvent {
    pub(crate) fn validate(&self) -> Result<(), ReplayError> {
        let valid = match self {
            Self::Requested { intent } => {
                intent.operation_id.get_version() == Some(uuid::Version::SortRand)
                    && intent.shell.valid()
                    && valid_path(&intent.home)
                    && crate::validate::valid_digest(&intent.environment_sha256)
            }
            Self::Approved { operation_id } | Self::Finished { operation_id, .. } => {
                operation_id.get_version() == Some(uuid::Version::SortRand)
            }
            Self::Captured {
                operation_id,
                sha256,
            } => {
                operation_id.get_version() == Some(uuid::Version::SortRand)
                    && crate::validate::valid_digest(sha256)
            }
        };
        valid.then_some(()).ok_or(ReplayError::InvalidPayload)
    }
}

impl LoginShellIdentity {
    pub(crate) fn valid(&self) -> bool {
        valid_path(&self.path) && crate::validate::valid_digest(&self.sha256)
    }
}

fn valid_path(path: &str) -> bool {
    std::path::Path::new(path).is_absolute()
        && path.len() <= crate::validate::MAX_PATH_BYTES
        && !path.contains('\0')
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnvironmentCapturePhase {
    Requested,
    Approved,
    Captured(String),
    Finished(EnvironmentCaptureStage),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironmentCaptureView {
    pub intent: EnvironmentCaptureIntent,
    pub phase: EnvironmentCapturePhase,
}

impl EnvironmentCaptureView {
    pub fn outcome(&self) -> EnvironmentCaptureStage {
        match self.phase {
            EnvironmentCapturePhase::Finished(stage) => stage,
            EnvironmentCapturePhase::Requested => EnvironmentCaptureStage::Declined,
            EnvironmentCapturePhase::Approved | EnvironmentCapturePhase::Captured(_) => {
                EnvironmentCaptureStage::Uncertain
            }
        }
    }
}

pub(crate) fn apply(
    view: &mut SessionView,
    record: &EnvironmentCaptureEvent,
) -> Result<(), ReplayError> {
    use EnvironmentCaptureEvent as Event;
    use EnvironmentCapturePhase as Phase;
    if let Event::Requested { intent } = record {
        if view.runs.last().is_some_and(|run| {
            matches!(
                run.status,
                super::RunStatus::Pending | super::RunStatus::Active
            )
        }) || view.environment_capture.as_ref().is_some_and(|capture| {
            !matches!(capture.phase, Phase::Finished(_))
                || capture.intent.operation_id == intent.operation_id
        }) {
            return Err(ReplayError::InvalidTransition);
        }
        view.environment_capture = Some(EnvironmentCaptureView {
            intent: intent.clone(),
            phase: Phase::Requested,
        });
        return Ok(());
    }
    let capture = view
        .environment_capture
        .as_mut()
        .ok_or(ReplayError::InvalidTransition)?;
    let (operation_id, phase) = match (record, &capture.phase) {
        (Event::Approved { operation_id }, Phase::Requested) => (*operation_id, Phase::Approved),
        (
            Event::Captured {
                operation_id,
                sha256,
            },
            Phase::Approved,
        ) => (*operation_id, Phase::Captured(sha256.clone())),
        (
            Event::Finished {
                operation_id,
                stage,
            },
            phase,
        ) if can_finish(phase, *stage) => (*operation_id, Phase::Finished(*stage)),
        _ => return Err(ReplayError::InvalidTransition),
    };
    if operation_id != capture.intent.operation_id {
        return Err(ReplayError::InvalidTransition);
    }
    capture.phase = phase;
    Ok(())
}

fn can_finish(phase: &EnvironmentCapturePhase, stage: EnvironmentCaptureStage) -> bool {
    use EnvironmentCapturePhase as Phase;
    use EnvironmentCaptureStage as Stage;
    match phase {
        Phase::Requested => matches!(stage, Stage::Declined | Stage::ApprovalUnavailable),
        Phase::Approved => !matches!(
            stage,
            Stage::Saved | Stage::Save | Stage::Declined | Stage::ApprovalUnavailable
        ),
        Phase::Captured(_) => matches!(
            stage,
            Stage::Saved | Stage::Save | Stage::PinnedInput | Stage::Uncertain
        ),
        Phase::Finished(_) => false,
    }
}
