use super::prompts::{await_operation, await_setup_ack, choose, review_warning};
use super::{
    AUTHORIZE_PROMPT, AUTHORIZE_TITLE, SavedDefaults, SetupError, SetupJournal, SetupSelection,
};
use crate::accounts::credentials::{self, AccountStorage, CredentialError, SavedAccount};
use crate::presentation::escape_terminal;
use crate::terminal::{AttachedTerminal, Composer, TerminalInput};
use arany::{
    Effort, MAX_ANTHROPIC_WORKSPACE_ID_BYTES, MAX_NATIVE_API_KEY_BYTES, ModelEntry,
    NativeApiCredentials, ProviderKind, ProviderSetupDisposition, ProviderSetupTargetKind,
    RecordKind, SessionDefaults, list_native_models_with_credentials, validate_native_api_key,
};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) fn saved_account_presence(path: &Path) -> Result<(bool, bool), SetupError> {
    let Some(state) = arany::StateRoot::open_optional(path).map_err(SetupError::recoverable)?
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
    let Some(catalog) = load_catalog(terminal, &account.provider.profile(), &credentials).await?
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
            let disposition = if error.outcome_unknown() {
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
            let uncertain = error.outcome_unknown();
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

async fn inspect_or_file() -> Result<credentials::InspectedAccount, CredentialError> {
    Ok(match credentials::inspect().await {
        Ok(inspected) => inspected,
        Err(CredentialError::Unavailable | CredentialError::TimedOut) => {
            credentials::InspectedAccount {
                account: None,
                storage: AccountStorage::PrivateFile,
            }
        }
        Err(error) => return Err(error),
    })
}

pub(super) async fn native_setup(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<Option<SetupSelection>, SetupError> {
    let inspected = match inspect_or_file().await {
        Ok(inspected) => inspected,
        Err(CredentialError::InvalidAccount) => {
            if !reset_saved_setup(terminal, workspace, credentials::SetupData::Native, journal)
                .await?
            {
                return Ok(None);
            }
            inspect_or_file().await.map_err(SetupError::recoverable)?
        }
        Err(error) => return Err(SetupError::recoverable(error)),
    };
    let expected = inspected
        .account
        .as_ref()
        .map(SavedAccount::snapshot_digest)
        .transpose()
        .map_err(SetupError::recoverable)?;
    let storage = inspected.storage;
    if storage == AccountStorage::PrivateFile
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
    let Some(provider) = choose(
        terminal,
        2,
        "Choose Provider",
        "Only the selected Provider receives requests",
        &[(b'1', "OpenAI"), (b'2', "Anthropic")],
    )
    .await?
    else {
        return Ok(None);
    };
    let provider = if provider == b'1' {
        ProviderKind::OpenAi
    } else {
        ProviderKind::Anthropic
    };
    let Some(key) = read_api_key(terminal).await? else {
        return Ok(None);
    };
    let workspace_id = if provider == ProviderKind::Anthropic {
        match choose(
            terminal,
            3,
            "API workspace",
            "Use the key's scope or choose a Console workspace",
            &[
                (b'1', "Key scoped to workspace"),
                (b'2', "Choose API workspace"),
            ],
        )
        .await?
        {
            Some(b'1') => None,
            Some(b'2') => {
                let Some(id) = read_workspace_id(terminal, &key).await? else {
                    return Ok(None);
                };
                Some(id)
            }
            None => return Ok(None),
            _ => return Err("invalid API workspace selection".into()),
        }
    } else {
        None
    };
    let provider = provider.native_name().expect("native Provider name");
    let credentials = NativeApiCredentials::new(provider, key, workspace_id)
        .map_err(|_| "invalid selected native API credentials")?;
    let Some(items) = load_catalog(terminal, provider, &credentials).await? else {
        return Ok(None);
    };
    let (model, preferred) = native_catalog_default(provider, &items)
        .ok_or("selected account has no visible models; use /setup to retry")?;
    let notice = if preferred {
        format!("{provider} ready: {model} · low. Use /model to change.")
    } else {
        format!(
            "{provider} ready: {model} · low. The catalog does not list prices; use /model to change."
        )
    };
    let workspace_id = credentials.anthropic_workspace_id().map(str::to_owned);
    let account = SavedAccount::new(
        provider.into(),
        model.to_owned(),
        Some(Effort::Low),
        credentials.into_api_key(),
    )
    .and_then(|account| account.with_anthropic_workspace(workspace_id))
    .map_err(SetupError::recoverable)?;
    let defaults = defaults(&account);
    terminal.draw_setup(
        0,
        "Saving account",
        match storage {
            AccountStorage::Keyring => "Using the OS credential store",
            AccountStorage::PrivateFile => "Using the private 0600 file",
        },
        0,
        None,
    )?;
    let account_id = account.id;
    let operation = journal
        .start(
            ProviderSetupTargetKind::NativeAccount,
            account_id.to_string(),
            expected,
        )
        .await?;
    settle_native_save(
        journal,
        operation,
        account_id,
        credentials::save(workspace, account, storage, expected).await,
    )
    .await?;
    Ok(Some(SetupSelection::Native {
        defaults,
        notice,
        catalog: items,
    }))
}

pub(super) async fn settle_native_save(
    journal: &SetupJournal,
    operation: uuid::Uuid,
    account_id: uuid::Uuid,
    result: Result<(), CredentialError>,
) -> Result<(), SetupError> {
    match result {
        Ok(()) => {
            journal
                .finish(
                    operation,
                    Some(Sha256::digest(account_id.as_bytes()).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await?;
        }
        Err(error) => {
            let disposition = if error.outcome_unknown() {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            journal.finish(operation, None, disposition).await?;
            return Err(SetupError::native_save(error));
        }
    }
    Ok(())
}

fn native_catalog_default<'a>(provider: &str, items: &'a [ModelEntry]) -> Option<(&'a str, bool)> {
    let preferred: &[&str] = match ProviderKind::parse(provider) {
        Some(ProviderKind::OpenAi) => &["gpt-5.6-luna"],
        Some(ProviderKind::Anthropic) => &["claude-sonnet-5-5", "claude-sonnet-5"],
        Some(ProviderKind::ChatGpt | ProviderKind::Custom) | None => return None,
    };
    preferred
        .iter()
        .find_map(|id| items.iter().find(|item| item.id == *id))
        .map(|item| (item.id.as_str(), true))
        .or_else(|| items.first().map(|item| (item.id.as_str(), false)))
}

async fn load_catalog(
    terminal: &mut AttachedTerminal,
    provider: &str,
    credentials: &NativeApiCredentials,
) -> Result<Option<Vec<arany::ModelEntry>>, SetupError> {
    await_operation(
        terminal,
        "Loading models",
        "Reading the selected account's model catalog; Ctrl+C cancels",
        list_native_models_with_credentials(provider, credentials),
    )
    .await?
    .transpose()
    .map_err(SetupError::recoverable)
}

async fn read_api_key(terminal: &mut AttachedTerminal) -> Result<Option<String>, SetupError> {
    terminal.begin_secret_input()?;
    let result = read_api_key_hidden(terminal).await;
    terminal.end_secret_input()?;
    result
}

async fn read_api_key_hidden(
    terminal: &mut AttachedTerminal,
) -> Result<Option<String>, SetupError> {
    let mut key = String::new();
    let mut rejected = 0u16;
    let mut notice = None::<&str>;
    let instruction = format!("Hidden input; up to {MAX_NATIVE_API_KEY_BYTES} ASCII characters");
    loop {
        terminal.draw_setup(3, "Enter API key", &instruction, key.len(), notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Character(character)
                if rejected == 0
                    && character.is_ascii_graphic()
                    && key.len() < MAX_NATIVE_API_KEY_BYTES =>
            {
                key.push(character);
            }
            TerminalInput::Paste => match terminal.take_paste() {
                Ok(text)
                    if rejected == 0
                        && key.len() + text.len() <= MAX_NATIVE_API_KEY_BYTES
                        && text.bytes().all(|byte| byte.is_ascii_graphic()) =>
                {
                    key.push_str(&text);
                }
                _ => {
                    rejected = rejected.saturating_add(1);
                    notice = Some("Invalid or overlong API key paste; Backspace to correct");
                }
            },
            TerminalInput::Character(_) => {
                rejected = rejected.saturating_add(1);
                notice = Some("Invalid or overlong API key");
            }
            TerminalInput::Backspace => {
                if rejected > 0 {
                    rejected -= 1;
                } else {
                    key.pop();
                }
            }
            TerminalInput::Submit if rejected == 0 && validate_native_api_key(&key).is_ok() => {
                return Ok(Some(key));
            }
            TerminalInput::Submit => {
                key.clear();
                rejected = 0;
                notice = Some("Invalid or overlong API key; retry");
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                key.clear();
                rejected = 0;
                terminal.discard_draft_input();
                notice = Some("Invalid or overlong API key line; retry");
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                terminal.begin_secret_input()?;
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => {
                return Err(SetupError::Shutdown(signal));
            }
            _ => {}
        }
    }
}

async fn read_workspace_id(
    terminal: &mut AttachedTerminal,
    key: &str,
) -> Result<Option<String>, SetupError> {
    let mut draft = Composer::default();
    let mut rejected = 0u16;
    let mut notice = None;
    let instruction = format!(
        "From Claude Console; wrkspc_ followed by letters or digits, up to {MAX_ANTHROPIC_WORKSPACE_ID_BYTES} characters"
    );
    loop {
        terminal.draw_setup_text_input("API workspace ID", &instruction, &draft, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Character(character)
                if rejected == 0
                    && (character.is_ascii_graphic() || character == ' ')
                    && draft.text().len() < MAX_ANTHROPIC_WORKSPACE_ID_BYTES =>
            {
                draft.apply(TerminalInput::Character(character));
            }
            TerminalInput::Paste => match terminal.take_paste() {
                Ok(text)
                    if rejected == 0
                        && draft.text().len() + text.len() <= MAX_ANTHROPIC_WORKSPACE_ID_BYTES
                        && text
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() || byte == b' ') =>
                {
                    if draft.insert_paste(&text).is_err() {
                        notice = Some("Error: invalid workspace ID paste; unchanged");
                    }
                }
                _ => {
                    notice = Some("Error: invalid or overlong workspace ID paste; unchanged");
                }
            },
            TerminalInput::Character(_) => {
                if terminal.is_linear() {
                    rejected = rejected.saturating_add(1);
                }
                notice = Some("Invalid or overlong workspace ID; use Backspace to correct");
            }
            TerminalInput::Backspace if rejected > 0 => rejected -= 1,
            input @ (TerminalInput::Backspace
            | TerminalInput::Delete
            | TerminalInput::Left
            | TerminalInput::Right
            | TerminalInput::Home
            | TerminalInput::End) => {
                draft.apply(input);
            }
            TerminalInput::Submit => {
                if rejected == 0
                    && NativeApiCredentials::new(
                        ProviderKind::Anthropic
                            .native_name()
                            .expect("native Provider name"),
                        key.to_owned(),
                        Some(draft.text().to_owned()),
                    )
                    .is_ok()
                {
                    return Ok(Some(draft.take()));
                }
                if terminal.is_linear() {
                    draft.clear();
                    rejected = 0;
                    terminal.discard_draft_input();
                }
                notice = Some("Error: enter wrkspc_ followed by letters or digits");
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                draft.clear();
                rejected = 0;
                terminal.discard_draft_input();
                notice = Some("Error: invalid workspace ID line; retry");
            }
            TerminalInput::Suspend => terminal.suspend_and_resume(draft.input_capacity())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arany::StateRoot;

    #[test]
    fn setup_account_requires_explicit_effort_for_unreviewed_model() {
        assert!(
            SavedAccount::new("openai".into(), "future-model".into(), None, "key".into()).is_err()
        );
        assert!(
            SavedAccount::new(
                "openai".into(),
                "future-model".into(),
                Some(Effort::High),
                "key".into(),
            )
            .is_ok()
        );
        for (provider, preference) in [
            ("openai", "gpt-5.6-luna"),
            ("anthropic", "claude-sonnet-5-5"),
        ] {
            let mut items = vec![ModelEntry::exact_custom("future-model".into())];
            assert_eq!(
                native_catalog_default(provider, &items),
                Some(("future-model", false))
            );
            items.push(ModelEntry::exact_custom(preference.into()));
            assert_eq!(
                native_catalog_default(provider, &items),
                Some((preference, true))
            );
            assert_eq!(items[0].id, "future-model", "catalog order is unchanged");
            let (model, _) = native_catalog_default(provider, &items).unwrap();
            let account = SavedAccount::new(
                provider.into(),
                model.into(),
                Some(Effort::Low),
                "synthetic-key".into(),
            )
            .unwrap();
            assert_eq!(defaults(&account).effort, Some(Effort::Low));
            assert!(native_catalog_default(provider, &[]).is_none());
        }
        assert!(native_catalog_default("custom:local", &[]).is_none());
    }

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
