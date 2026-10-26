use crate::accounts::credentials::CredentialError;
use crate::terminal::{ShutdownSignal, TerminalError};
use std::fmt::Display;

pub(in crate::cli::attached) enum SetupError {
    Recoverable(String),
    Presentation(String),
    Uncertain(String),
    Shutdown(ShutdownSignal),
}

impl SetupError {
    pub(super) fn native_removal(error: CredentialError) -> Self {
        match error {
            CredentialError::DeleteOutcomeUnknown | CredentialError::WriteOutcomeUnknown => {
                Self::Presentation(
                    "Native account removal is uncertain; restart setup and inspect saved access before continuing."
                        .into(),
                )
            }
            _ => Self::recoverable_unrecorded(error),
        }
    }

    pub(super) fn native_save(error: CredentialError) -> Self {
        match error {
            CredentialError::WriteOutcomeUnknown | CredentialError::DeleteOutcomeUnknown => {
                Self::Presentation(
                    "Native account storage is uncertain; restart setup and inspect saved access before continuing".into(),
                )
            }
            _ => Self::recoverable(error),
        }
    }

    pub(super) fn recoverable(error: impl Display) -> Self {
        arany::record_development_failure(arany::DevelopmentFailure::Setup);
        Self::recoverable_unrecorded(error)
    }

    pub(super) fn recoverable_unrecorded(error: impl Display) -> Self {
        Self::Recoverable(error.to_string())
    }
}

impl From<String> for SetupError {
    fn from(message: String) -> Self {
        Self::recoverable(message)
    }
}

impl From<&str> for SetupError {
    fn from(message: &str) -> Self {
        Self::recoverable(message)
    }
}

impl From<TerminalError> for SetupError {
    fn from(error: TerminalError) -> Self {
        Self::Presentation(error.to_string())
    }
}

impl From<SetupError> for String {
    fn from(error: SetupError) -> Self {
        match error {
            SetupError::Recoverable(message)
            | SetupError::Presentation(message)
            | SetupError::Uncertain(message) => message,
            SetupError::Shutdown(signal) => format!("terminated by {}", signal.name()),
        }
    }
}
