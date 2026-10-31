use crate::accounts::chatgpt;
use crate::accounts::credentials::CredentialError;
use crate::terminal::{ShutdownSignal, TerminalError};
use arany::CustomProfileError;
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

    pub(super) fn chatgpt_removal(error: chatgpt::AuthorizationError) -> Self {
        match error {
            error if chatgpt_edit_is_uncertain(&error) => Self::Presentation(
                "ChatGPT account cleanup is uncertain; restart setup and inspect saved access before continuing.".into(),
            ),
            error => Self::recoverable_unrecorded(error),
        }
    }

    pub(super) fn custom_write(error: CustomProfileError) -> Self {
        if matches!(&error, CustomProfileError::PublicationUncertain) {
            Self::Presentation(
                "Custom route publication is uncertain. Inspect saved routes before continuing; no retry was attempted."
                    .into(),
            )
        } else {
            Self::recoverable_unrecorded(error)
        }
    }

    pub(super) fn chatgpt_authorization(error: chatgpt::AuthorizationError) -> Self {
        if chatgpt_edit_is_uncertain(&error) {
            Self::Uncertain(error.to_string())
        } else {
            Self::recoverable(error)
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

pub(super) fn chatgpt_edit_is_uncertain(error: &chatgpt::AuthorizationError) -> bool {
    matches!(
        error,
        chatgpt::AuthorizationError::SignOutStorageUncertain
            | chatgpt::AuthorizationError::RefreshCleanupUncertain
            | chatgpt::AuthorizationError::RenewalStorageUncertain
            | chatgpt::AuthorizationError::RefreshOutcomeUnknown
            | chatgpt::AuthorizationError::CredentialStore(
                CredentialError::WriteOutcomeUnknown | CredentialError::DeleteOutcomeUnknown
            )
    )
}

pub(super) fn chatgpt_keyring_available(
    probe: Result<(), CredentialError>,
) -> Result<bool, SetupError> {
    match probe {
        Ok(()) => Ok(true),
        Err(CredentialError::Unavailable | CredentialError::TimedOut) => Ok(false),
        Err(CredentialError::Locked) => Err(
            "OS credential store is locked. Unlock your desktop password store, then retry /setup"
                .into(),
        ),
        Err(error) => Err(SetupError::recoverable(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_file_fallback_never_treats_locked_keyring_as_missing() {
        assert!(matches!(chatgpt_keyring_available(Ok(())), Ok(true)));
        assert!(matches!(
            chatgpt_keyring_available(Err(CredentialError::Unavailable)),
            Ok(false)
        ));
        assert!(matches!(
            chatgpt_keyring_available(Err(CredentialError::TimedOut)),
            Ok(false)
        ));
        assert!(matches!(
            chatgpt_keyring_available(Err(CredentialError::Locked)),
            Err(SetupError::Recoverable(_))
        ));
    }
}
