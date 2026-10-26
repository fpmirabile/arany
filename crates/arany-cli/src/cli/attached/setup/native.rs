use super::{
    AUTHORIZE_PROMPT, AUTHORIZE_TITLE, SavedDefaults, SetupError, SetupJournal, SetupSelection,
    await_operation, await_setup_ack, choose, review_warning,
};
use crate::accounts::credentials::{self, AccountStorage, CredentialError, SavedAccount};
use crate::presentation::escape_terminal;
use crate::terminal::AttachedTerminal;
use arany::{ProviderSetupDisposition, ProviderSetupTargetKind, RecordKind, SessionDefaults};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) fn saved_account_presence(path: &Path) -> Result<(bool, bool), SetupError> {
    let Some(state) = crate::files::open_optional_state(path).map_err(SetupError::recoverable)?
    else {
        return Ok((false, false));
    };
    Ok((
        state
            .record(RecordKind::SavedAccount)
            .present()
            .map_err(SetupError::recoverable)?,
        state
            .record(RecordKind::ChatgptAccounts)
            .present()
            .map_err(SetupError::recoverable)?,
    ))
}

pub(super) async fn saved_native_defaults(
    terminal: &mut AttachedTerminal,
) -> Result<SavedDefaults, SetupError> {
    let Some(inspected) = await_operation(
        terminal,
        AUTHORIZE_TITLE,
        AUTHORIZE_PROMPT,
        credentials::inspect_interactive(),
    )
    .await?
    else {
        return Ok(SavedDefaults::Cancelled);
    };
    let account = inspected
        .map_err(SetupError::recoverable)?
        .account
        .ok_or("saved API account unavailable; use /setup to repair it")?;
    Ok(SavedDefaults::AccessChecked(defaults(&account)))
}

pub(super) async fn inspect_native_account(
    terminal: &mut AttachedTerminal,
) -> Result<(), SetupError> {
    let inspected = credentials::inspect()
        .await
        .map_err(SetupError::recoverable)?;
    let Some(account) = inspected.account else {
        await_setup_ack(terminal, "Native API account", "No saved account").await?;
        return Ok(());
    };
    let details = format!(
        "Provider: {}\nModel: {}\nEffort: {}\nStorage: {}\nCredential: stored; hidden",
        escape_terminal(&account.provider.profile()),
        escape_terminal(&account.model),
        account
            .effort
            .map_or("default".to_owned(), |effort| effort.to_string()),
        match inspected.storage {
            AccountStorage::Keyring => "OS password store",
            AccountStorage::PrivateFile => "private file",
        },
    );
    await_setup_ack(terminal, "Native API account", &details).await?;
    Ok(())
}

pub(super) async fn select_native_account(
    terminal: &mut AttachedTerminal,
) -> Result<Option<SetupSelection>, SetupError> {
    let inspected = credentials::inspect()
        .await
        .map_err(SetupError::recoverable)?;
    let Some(account) = inspected.account else {
        await_setup_ack(terminal, "Native API account", "No saved account to select").await?;
        return Ok(None);
    };
    if inspected.storage == AccountStorage::PrivateFile
        && choose(
            terminal,
            1,
            "Use private file?",
            "NOT encrypted; same-user apps read key",
            &[(b'2', "Cancel"), (b'1', "Use private file")],
        )
        .await?
            != Some(b'1')
    {
        return Ok(None);
    }
    let credentials = credentials::load_selected(account.id, &account.provider.profile())
        .await
        .map_err(SetupError::recoverable)?;
    let Some(catalog) =
        super::load_catalog(terminal, &account.provider.profile(), &credentials).await?
    else {
        return Ok(None);
    };
    let notice = format!(
        "{} ready: {} · use /model to change.",
        account.provider.profile(),
        account.model
    );
    Ok(Some(SetupSelection::Native {
        defaults: defaults(&account),
        notice,
        catalog,
    }))
}

pub(super) fn defaults(account: &SavedAccount) -> SessionDefaults {
    SessionDefaults {
        provider: Some(account.provider.profile()),
        model: Some(account.model.clone()),
        effort: account.effort,
        account_id: Some(account.id),
        policy: arany::CollaborationPolicy::default(),
    }
}

pub(super) async fn remove_native_saved_access(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<(), SetupError> {
    let removal =
        credentials::NativeRemoval::prepare(workspace).map_err(SetupError::recoverable)?;
    let Some(removal) = removal else {
        await_setup_ack(terminal, "Native API account", "No saved account").await?;
        return Ok(());
    };
    if review_warning(
        terminal,
        "Remove native API account?",
        "Delete Arany's saved API credential and model preferences. This does not revoke the Provider's credential remotely.",
        "Remove local API access?",
    )
    .await?
    {
        let operation = journal
            .start(
                ProviderSetupTargetKind::NativeAccount,
                "native".into(),
                None,
            )
            .await?;
        settle_native_removal(journal, operation, removal.apply(workspace)).await?;
        await_setup_ack(
            terminal,
            "Native API account",
            "Removed locally; the Provider credential remains active",
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn settle_native_removal(
    journal: &SetupJournal,
    operation: uuid::Uuid,
    result: Result<(), CredentialError>,
) -> Result<(), SetupError> {
    match result {
        Ok(()) => {
            journal
                .finish(
                    operation,
                    Some(Sha256::digest([]).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await
        }
        Err(error) => {
            let disposition = if matches!(
                error,
                CredentialError::DeleteOutcomeUnknown | CredentialError::WriteOutcomeUnknown
            ) {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            journal.finish(operation, None, disposition).await?;
            Err(SetupError::native_removal(error))
        }
    }
}

pub(super) async fn reset_saved_setup(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    data: credentials::SetupData,
    journal: &mut SetupJournal,
) -> Result<bool, SetupError> {
    let reset =
        credentials::SetupReset::prepare(workspace, data).map_err(SetupError::recoverable)?;
    let (title, instruction) = match data {
        credentials::SetupData::Native => (
            "Reset API setup?",
            "Outdated or damaged setup. Reset API setup and models; keep history and ChatGPT. Old password-store entries may remain; no remote sign-out.",
        ),
        credentials::SetupData::ChatGpt => (
            "Reset ChatGPT setup?",
            "Outdated or damaged setup. Reset ChatGPT accounts, sign-in and models; keep history and API accounts. Password-store entries may remain; no remote sign-out.",
        ),
        credentials::SetupData::Models => (
            "Reset saved model choices?",
            "Clear outdated or damaged model choices; keep accounts, history and folder trust.",
        ),
    };
    if choose(
        terminal,
        2,
        title,
        instruction,
        &[(b'0', "Back"), (b'1', "Reset local setup")],
    )
    .await?
        != Some(b'1')
    {
        return Ok(false);
    }
    let (kind, identity) = match data {
        credentials::SetupData::Native => {
            (ProviderSetupTargetKind::NativeAccount, "native".to_owned())
        }
        credentials::SetupData::ChatGpt => (
            ProviderSetupTargetKind::ChatGptAccount,
            "chatgpt-setup".to_owned(),
        ),
        credentials::SetupData::Models => (
            ProviderSetupTargetKind::ModelPreferences,
            "models".to_owned(),
        ),
    };
    let operation = journal.start(kind, identity, None).await?;
    match reset.apply(workspace) {
        Ok(()) => {
            journal
                .finish(
                    operation,
                    Some(Sha256::digest([]).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await?;
        }
        Err(error) => {
            let uncertain = matches!(
                &error,
                CredentialError::WriteOutcomeUnknown | CredentialError::DeleteOutcomeUnknown
            );
            let disposition = if uncertain {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            journal.finish(operation, None, disposition).await?;
            if uncertain {
                return Err(SetupError::Presentation(
                    "Saved setup reset is uncertain; restart and inspect saved access before continuing".into(),
                ));
            }
            return Err(SetupError::from(format!(
                "{error}; reset may be incomplete; reopen /setup to check it"
            )));
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::{Effort, StateRoot};

    #[test]
    fn saved_account_defaults_pin_only_the_selected_account() {
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::High),
            "synthetic-key".into(),
        )
        .expect("account");
        let defaults = defaults(&account);
        assert_eq!(defaults.provider.as_deref(), Some("openai"));
        assert_eq!(defaults.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(defaults.effort, Some(Effort::High));
        assert_eq!(defaults.account_id, Some(account.id));

        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("current");
        assert!(matches!(
            saved_account_presence(&current),
            Ok((false, false))
        ));
        let root = StateRoot::admit(&current).unwrap();
        root.record(RecordKind::ChatgptAccounts)
            .replace(b"not a parsed account index")
            .unwrap();
        assert!(matches!(
            saved_account_presence(&current),
            Ok((false, true))
        ));
        root.record(RecordKind::SavedAccount)
            .replace(b"not a parsed native account")
            .unwrap();
        assert!(matches!(saved_account_presence(&current), Ok((true, true))));
    }
}
