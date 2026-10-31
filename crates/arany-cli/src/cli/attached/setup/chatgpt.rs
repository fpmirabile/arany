use super::{
    SavedDefaults, SetupError, SetupJournal, SetupSelection, await_operation, await_setup_ack,
    choose,
    error::{chatgpt_edit_is_uncertain, chatgpt_keyring_available},
    native, review_warning,
};
use crate::accounts::{
    chatgpt::{self, CallbackListener, RiskAcknowledgment, RiskPrompt, registration::Registration},
    credentials::{self, AccountStorage},
};
use crate::terminal::AttachedTerminal;
use arany::{
    CollaborationPolicy, Effort, ModelEntry, ProviderKind, ProviderSetupDisposition,
    ProviderSetupTargetKind, SessionDefaults,
};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::Path;

pub(super) enum ChatGptSetupAccount {
    Existing(uuid::Uuid),
    Switched(uuid::Uuid),
    Connected(uuid::Uuid),
    Reauthorized(uuid::Uuid),
}

impl ChatGptSetupAccount {
    pub(super) fn id(&self) -> uuid::Uuid {
        match self {
            Self::Existing(id)
            | Self::Switched(id)
            | Self::Connected(id)
            | Self::Reauthorized(id) => *id,
        }
    }

    pub(super) fn status(&self) -> &'static str {
        match self {
            Self::Existing(_) => "saved ChatGPT account remains selected",
            Self::Switched(_) => "saved ChatGPT account was selected",
            Self::Connected(_) => "new ChatGPT account was saved",
            Self::Reauthorized(_) => "ChatGPT account was reconnected",
        }
    }

    pub(super) fn finish(
        self,
        defaults: SessionDefaults,
        notice: Option<String>,
    ) -> Result<Option<SetupSelection>, String> {
        if matches!(self, Self::Existing(_))
            && (defaults.model.is_none() || defaults.effort.is_none())
        {
            notice.map_or(Ok(None), Err)
        } else {
            Ok(Some(SetupSelection::ChatGpt {
                defaults,
                notice,
                catalog: Vec::new(),
            }))
        }
    }
}

pub(super) async fn chatgpt_setup(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<Option<SetupSelection>, SetupError> {
    match chatgpt::registration::validate_saved_setup(workspace) {
        Ok(()) => {}
        Err(chatgpt::AuthorizationError::InvalidSavedState) => {
            if !native::reset_saved_setup(
                terminal,
                workspace,
                credentials::SetupData::ChatGpt,
                journal,
            )
            .await?
            {
                return Ok(None);
            }
        }
        Err(error) => return Err(SetupError::recoverable(error)),
    }
    let account = loop {
        let (saved, connected) = match chatgpt::selected_registration() {
            Ok(saved) => saved,
            Err(chatgpt::AuthorizationError::NoSelectedAccount) => {
                break connect_chatgpt(terminal, workspace, journal).await?;
            }
            Err(error) => return Err(SetupError::recoverable(error)),
        };
        match chatgpt_account_action(terminal, connected).await? {
            Some(ChatGptAccountAction::Inspect) => inspect_chatgpt_account(terminal).await?,
            Some(ChatGptAccountAction::UseSaved) => {
                break choose_saved_chatgpt(terminal, workspace, saved, journal).await?;
            }
            Some(ChatGptAccountAction::Reconnect) => {
                let target = chatgpt::selected_reauthorization_target(saved)
                    .map_err(SetupError::recoverable)?;
                break reconnect_chatgpt(terminal, workspace, target, journal).await?;
            }
            Some(ChatGptAccountAction::ConnectNew) => {
                break connect_chatgpt(terminal, workspace, journal).await?;
            }
            None => break None,
        }
    };
    let Some(account) = account else {
        return Ok(None);
    };
    let (defaults, notice, catalog) = configure_chatgpt(terminal, workspace, &account).await?;
    let mut selection = account.finish(defaults, notice).map_err(SetupError::from)?;
    if let Some(SetupSelection::ChatGpt { catalog: rows, .. }) = &mut selection {
        *rows = catalog;
    }
    Ok(selection)
}

enum ChatGptAccountAction {
    UseSaved,
    Reconnect,
    ConnectNew,
    Inspect,
}

async fn chatgpt_account_action(
    terminal: &mut AttachedTerminal,
    connected: bool,
) -> Result<Option<ChatGptAccountAction>, SetupError> {
    let (title, instruction, reconnect_label, other_saved) = if connected {
        (
            "ChatGPT account",
            "Reuse, reconnect, inspect, or add",
            "Reconnect",
            true,
        )
    } else {
        let (_, ids) = chatgpt::saved_account_ids().map_err(SetupError::recoverable)?;
        (
            "ChatGPT account disconnected",
            "Inspect, reconnect or add",
            "Reconnect",
            ids.len() > 1,
        )
    };
    let mut choices = Vec::new();
    if other_saved {
        choices.push((b'1', "Use saved"));
    }
    choices.extend([
        (b'2', reconnect_label),
        (b'3', "Connect new"),
        (b'4', "Inspect"),
    ]);
    Ok(
        match choose(terminal, 2, title, instruction, &choices).await? {
            Some(b'1') => Some(ChatGptAccountAction::UseSaved),
            Some(b'2') => Some(ChatGptAccountAction::Reconnect),
            Some(b'3') => Some(ChatGptAccountAction::ConnectNew),
            Some(b'4') => Some(ChatGptAccountAction::Inspect),
            None => None,
            Some(_) => return Err("invalid ChatGPT account choice".into()),
        },
    )
}

async fn choose_saved_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    expected_selected: uuid::Uuid,
    journal: &mut SetupJournal,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let (selected, ids) = chatgpt::saved_account_ids().map_err(SetupError::recoverable)?;
    if selected != expected_selected {
        return Err("ChatGPT account changed during setup; use /setup again".into());
    }
    let connected = chatgpt::selected_account_id().ok() == Some(selected);
    let ids = if connected {
        ids
    } else {
        ids.into_iter().filter(|id| *id != selected).collect()
    };
    if ids.is_empty() {
        return Err("no connected saved ChatGPT account; reconnect or add one".into());
    }
    if ids.len() == 1 {
        let target = ids[0];
        if target == selected {
            return Ok(Some(ChatGptSetupAccount::Existing(selected)));
        }
        select_chatgpt_account(workspace, selected, target, journal).await?;
        return Ok(Some(ChatGptSetupAccount::Switched(target)));
    }
    let labels = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let short = id.simple().to_string();
            format!(
                "{}{} {}",
                index + 1,
                if *id == selected { '*' } else { ' ' },
                &short[24..]
            )
        })
        .collect::<Vec<_>>();
    let choices = labels
        .iter()
        .enumerate()
        .map(|(index, label)| (b'1' + index as u8, label.as_str()))
        .collect::<Vec<_>>();
    let Some(choice) = choose(
        terminal,
        3,
        "Saved ChatGPT",
        if connected {
            "* is current; choose account"
        } else {
            "Choose saved account"
        },
        &choices,
    )
    .await?
    else {
        return Ok(None);
    };
    let target = ids
        .get(usize::from(choice - b'1'))
        .copied()
        .ok_or("invalid saved account choice")?;
    if target == selected {
        return Ok(Some(ChatGptSetupAccount::Existing(selected)));
    }
    select_chatgpt_account(workspace, selected, target, journal).await?;
    Ok(Some(ChatGptSetupAccount::Switched(target)))
}

pub(super) fn selected_chatgpt_defaults() -> Result<SavedDefaults, SetupError> {
    match chatgpt::selected_account_id() {
        Ok(id) => Ok(SavedDefaults::Selected(chatgpt_defaults(id))),
        Err(chatgpt::AuthorizationError::NoSelectedAccount) => Ok(SavedDefaults::Unconfigured),
        Err(error) => Err(SetupError::recoverable(error)),
    }
}

fn chatgpt_defaults(account_id: uuid::Uuid) -> SessionDefaults {
    SessionDefaults {
        provider: Some(
            ProviderKind::ChatGpt
                .native_name()
                .expect("native Provider name")
                .into(),
        ),
        model: None,
        effort: None,
        account_id: Some(account_id),
        policy: CollaborationPolicy::default(),
    }
}

pub(super) async fn remove_chatgpt_saved_access(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<(), SetupError> {
    let (selected, ids, expected_index) = match chatgpt::saved_account_index(workspace) {
        Ok(accounts) => accounts,
        Err(chatgpt::AuthorizationError::NoSelectedAccount) => {
            await_setup_ack(terminal, "ChatGPT account", "No saved account").await?;
            return Ok(());
        }
        Err(error) => return Err(SetupError::recoverable(error)),
    };
    let Some(account) =
        choose_saved_chatgpt_id(terminal, Some(selected), &ids, "Select ChatGPT account").await?
    else {
        return Ok(());
    };
    if !review_warning(
        terminal,
        "Remove ChatGPT access?",
        &format!("Clear local ChatGPT account {account}. No request is sent to the Provider, and remote access is unchanged."),
        "Remove local ChatGPT access?",
    )
    .await?
    {
        return Ok(());
    }
    let operation = journal
        .start(
            ProviderSetupTargetKind::ChatGptAccount,
            account.to_string(),
            None,
        )
        .await?;
    let outcome = match chatgpt::remove_account_locally(
        workspace.to_owned(),
        account,
        expected_index,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            let disposition = if chatgpt_edit_is_uncertain(&error) {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            journal.finish(operation, None, disposition).await?;
            return Err(SetupError::chatgpt_removal(error));
        }
    };
    journal
        .finish(
            operation,
            outcome.local_cleared.then(|| Sha256::digest([]).into()),
            if outcome.local_cleared {
                ProviderSetupDisposition::Applied
            } else {
                ProviderSetupDisposition::Uncertain
            },
        )
        .await?;
    let notice = if outcome.local_cleared {
        "ChatGPT disconnected locally; remote access remains active"
    } else {
        "ChatGPT credential cleanup is uncertain; inspect saved access before retrying"
    };
    await_setup_ack(terminal, "ChatGPT account", notice).await?;
    if !outcome.local_cleared {
        return Err(SetupError::Presentation(
            "ChatGPT account cleanup is uncertain; restart setup and inspect saved access before continuing".into(),
        ));
    }
    Ok(())
}

async fn choose_saved_chatgpt_id(
    terminal: &mut AttachedTerminal,
    selected: Option<uuid::Uuid>,
    ids: &[uuid::Uuid],
    title: &str,
) -> Result<Option<uuid::Uuid>, SetupError> {
    let choices: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            (
                u8::try_from(index + 1).unwrap_or(u8::MAX),
                selected.map_or_else(
                    || id.to_string(),
                    |selected| chatgpt_account_label(*id, selected),
                ),
            )
        })
        .collect();
    let rendered: Vec<_> = choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect();
    let choice = choose(
        terminal,
        1,
        title,
        "Account IDs only; no token access",
        &rendered,
    )
    .await?;
    Ok(choice.and_then(|choice| {
        choices
            .iter()
            .find(|(value, _)| *value == choice)
            .map(|(index, _)| ids[usize::from(*index - 1)])
    }))
}

fn chatgpt_account_label(id: uuid::Uuid, selected: uuid::Uuid) -> String {
    format!("{id}{}", if id == selected { " · selected" } else { "" })
}

async fn inspect_chatgpt_account(terminal: &mut AttachedTerminal) -> Result<(), SetupError> {
    let (selected, accounts) = chatgpt::account_inspections().map_err(SetupError::recoverable)?;
    if accounts.is_empty() {
        await_setup_ack(terminal, "ChatGPT account", "No saved account").await?;
        return Ok(());
    }
    let ids = accounts
        .iter()
        .map(|account| account.id)
        .collect::<Vec<_>>();
    let Some(id) =
        choose_saved_chatgpt_id(terminal, selected, &ids, "Inspect ChatGPT account").await?
    else {
        return Ok(());
    };
    let account = accounts
        .iter()
        .find(|account| account.id == id)
        .ok_or("saved ChatGPT account unavailable")?;
    let backend = match account.storage {
        AccountStorage::Keyring => "OS password store",
        AccountStorage::PrivateFile => "private file (not encrypted)",
    };
    let access = match account.access {
        chatgpt::AccountAccess::Connected => "connected; saved metadata only, not checked",
        chatgpt::AccountAccess::Disconnected => "unavailable; disconnected",
        chatgpt::AccountAccess::RenewalPending => "unavailable; renewal incomplete or uncertain",
        chatgpt::AccountAccess::SignOutPending => "unavailable; sign-out incomplete or uncertain",
    };
    await_setup_ack(terminal, "ChatGPT account details", &format!(
        "Provider: ChatGPT\nAccount: {}\nBackend: {backend}\nBilling: ChatGPT plan; no API-key billing\nAccess: {access}", account.id,
    )).await?;
    Ok(())
}

async fn select_chatgpt_account(
    workspace: &Path,
    selected: uuid::Uuid,
    target: uuid::Uuid,
    journal: &mut SetupJournal,
) -> Result<(), SetupError> {
    let operation = journal
        .start(
            ProviderSetupTargetKind::ChatGptAccount,
            target.to_string(),
            None,
        )
        .await?;
    match chatgpt::select_saved_account(workspace.to_path_buf(), selected, target).await {
        Ok(()) => {
            journal
                .finish(
                    operation,
                    Some(Sha256::digest(target.as_bytes()).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await
        }
        Err(error) => {
            let uncertain = chatgpt_edit_is_uncertain(&error);
            let disposition = if uncertain {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            journal.finish(operation, None, disposition).await?;
            if uncertain {
                return Err(SetupError::Presentation(
                    "ChatGPT account selection is uncertain; restart setup and inspect saved access before continuing".into(),
                ));
            }
            Err(SetupError::recoverable(error))
        }
    }
}

async fn connect_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    terminal.draw_setup(
        0,
        "Checking keyring",
        "Selecting protected account storage",
        0,
        None,
    )?;
    let storage = if chatgpt_keyring_available(credentials::probe_keyring().await)? {
        AccountStorage::Keyring
    } else {
        let Some(b'1') = choose(
            terminal,
            1,
            "Choose storage",
            "Password store unavailable. Unencrypted file",
            &[(b'2', "Cancel"), (b'1', "Use private file")],
        )
        .await?
        else {
            return Ok(None);
        };
        AccountStorage::PrivateFile
    };
    authorize_chatgpt(terminal, workspace, storage, None, journal).await
}

async fn reconnect_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    target: chatgpt::ReauthorizationTarget,
    journal: &mut SetupJournal,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    if target.storage == AccountStorage::Keyring
        && !chatgpt_keyring_available(credentials::probe_keyring().await)?
    {
        return Err("Saved ChatGPT keyring is unavailable. Unlock your desktop password store and retry Reconnect; or choose Connect new, then Use private file (not encrypted)".into());
    }
    authorize_chatgpt(terminal, workspace, target.storage, Some(target), journal).await
}

async fn authorize_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    storage: AccountStorage,
    target: Option<chatgpt::ReauthorizationTarget>,
    journal: &mut SetupJournal,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let Some(acknowledgment) = review_chatgpt_risk(terminal, storage).await? else {
        return Ok(None);
    };
    let operation = journal
        .start(
            ProviderSetupTargetKind::ChatGptAccount,
            target.as_ref().map_or_else(
                || "new-chatgpt-account".to_owned(),
                |selected| selected.id.to_string(),
            ),
            None,
        )
        .await?;
    let result = complete_chatgpt_authorization(terminal, workspace, target, acknowledgment).await;
    settle_chatgpt_authorization(journal, operation, result).await
}

pub(super) async fn settle_chatgpt_authorization(
    journal: &SetupJournal,
    operation: uuid::Uuid,
    result: Result<Option<ChatGptSetupAccount>, SetupError>,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let (digest, disposition) = match &result {
        Err(SetupError::Uncertain(_)) => (None, ProviderSetupDisposition::Uncertain),
        Ok(Some(account)) => (
            Some(Sha256::digest(account.id().as_bytes()).into()),
            ProviderSetupDisposition::Applied,
        ),
        Ok(None) | Err(SetupError::Shutdown(_)) => (None, ProviderSetupDisposition::Cancelled),
        Err(_) => (None, ProviderSetupDisposition::Failed),
    };
    journal.finish(operation, digest, disposition).await?;
    if disposition == ProviderSetupDisposition::Uncertain {
        return Err(SetupError::Uncertain(
            "ChatGPT account storage is uncertain; inspect saved access before continuing".into(),
        ));
    }
    result
}

async fn begin_chatgpt_authorization(
    workspace: &Path,
    target: Option<&chatgpt::ReauthorizationTarget>,
    acknowledgment: RiskAcknowledgment,
) -> Result<Option<(CallbackListener, chatgpt::AuthorizationAttempt)>, SetupError> {
    let registration =
        Registration::open_or_create(workspace).map_err(SetupError::chatgpt_authorization)?;
    if target
        .as_ref()
        .is_some_and(|selected| selected.host_id != registration.host_id())
    {
        return Err("ChatGPT registration changed; restart /setup".into());
    }
    let listener = CallbackListener::bind(acknowledgment)
        .await
        .map_err(SetupError::chatgpt_authorization)?;
    let attempt = match &target {
        Some(selected) => chatgpt::AuthorizationAttempt::for_existing(selected, listener.port()),
        None => registration.authorization_attempt(listener.port()),
    }
    .map_err(SetupError::chatgpt_authorization)?;
    chatgpt::open_authorization_url(&attempt.authorization_url())
        .await
        .map_err(|_| {
            SetupError::from("Could not open the system browser; no ChatGPT account was changed")
        })?;
    Ok(Some((listener, attempt)))
}

async fn complete_chatgpt_authorization(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    target: Option<chatgpt::ReauthorizationTarget>,
    acknowledgment: RiskAcknowledgment,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let Some((listener, attempt)) =
        begin_chatgpt_authorization(workspace, target.as_ref(), acknowledgment).await?
    else {
        return Ok(None);
    };
    let Some((exchange, acknowledgment)) = await_chatgpt_step(
        terminal,
        "Waiting for ChatGPT",
        if target.is_some() {
            "Choose the same account in browser; Esc cancels"
        } else {
            "Complete sign-in in your browser; Esc cancels"
        },
        listener.receive(attempt),
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(credentials) = await_chatgpt_step(
        terminal,
        "Verifying ChatGPT",
        "Checking the signed account identity",
        exchange.redeem(),
    )
    .await?
    else {
        return Ok(None);
    };
    terminal.draw_setup(
        6,
        "Saving ChatGPT account",
        "Keep Arany open until protected storage confirms",
        0,
        None,
    )?;
    save_chatgpt_authorization(
        workspace,
        target.as_ref().map(|selected| selected.id),
        credentials,
        acknowledgment,
    )
    .await
    .map(Some)
}

async fn save_chatgpt_authorization(
    workspace: &Path,
    expected: Option<uuid::Uuid>,
    credentials: chatgpt::VerifiedCredentials,
    acknowledgment: RiskAcknowledgment,
) -> Result<ChatGptSetupAccount, SetupError> {
    chatgpt::save_verified(
        workspace.to_path_buf(),
        credentials,
        acknowledgment,
        expected,
    )
    .await
    .map(|id| {
        if expected.is_some() {
            ChatGptSetupAccount::Reauthorized(id)
        } else {
            ChatGptSetupAccount::Connected(id)
        }
    })
    .map_err(SetupError::chatgpt_authorization)
}

async fn await_chatgpt_step<T>(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    operation: impl Future<Output = Result<T, chatgpt::AuthorizationError>>,
) -> Result<Option<T>, SetupError> {
    await_operation(terminal, title, instruction, operation)
        .await?
        .transpose()
        .map_err(SetupError::chatgpt_authorization)
}

fn catalog_chatgpt_defaults(
    account_id: uuid::Uuid,
    selected_id: uuid::Uuid,
    models: &[chatgpt::ChatGptModel],
) -> Result<SessionDefaults, String> {
    if selected_id != account_id {
        return Err("ChatGPT account changed during setup; use /setup again".into());
    }
    let model = models
        .iter()
        .find(|model| model.slug == "gpt-5.6-luna")
        .or_else(|| models.first())
        .ok_or("selected account has no visible ChatGPT models")?;
    let mut defaults = chatgpt_defaults(account_id);
    defaults.model = Some(model.slug.clone());
    defaults.effort = Some(Effort::Low);
    Ok(defaults)
}

async fn configure_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    account: &ChatGptSetupAccount,
) -> Result<(SessionDefaults, Option<String>, Vec<ModelEntry>), SetupError> {
    let account_id = account.id();
    let defaults = chatgpt_defaults(account_id);
    let catalog = await_chatgpt_step(
        terminal,
        "Loading ChatGPT models",
        "Reading visible models; Esc keeps the account",
        async { Ok(chatgpt::selected_models(workspace.to_path_buf(), Some(account_id)).await) },
    )
    .await?;
    let Some(catalog) = catalog else {
        return Ok((defaults, None, Vec::new()));
    };
    let (selected_id, rows) = match catalog {
        Ok(catalog) => catalog,
        Err(error) => {
            let recovery = if chatgpt_edit_is_uncertain(&error) {
                "Use /setup to reconnect before continuing"
            } else {
                "Use /provider chatgpt then /model to retry"
            };
            return Ok((
                defaults,
                Some(format!(
                    "Model catalog failed; {}. {recovery}: {error}",
                    account.status()
                )),
                Vec::new(),
            ));
        }
    };
    if selected_id != account_id {
        return Err("ChatGPT account changed during setup; use /setup again".into());
    }
    if rows.is_empty() {
        return Ok((
            defaults,
            Some(format!(
                "No visible ChatGPT models; {}. Use /model later",
                account.status()
            )),
            Vec::new(),
        ));
    }
    let defaults = catalog_chatgpt_defaults(account_id, selected_id, &rows)?;
    let model = defaults.model.as_deref().expect("nonempty visible catalog");
    let notice = if model == "gpt-5.6-luna" {
        format!("ChatGPT ready: {model} · low. Use /model to change.")
    } else {
        format!(
            "ChatGPT ready: {model} · low. The catalog does not list prices; use /model to change."
        )
    };
    let catalog_rows = rows
        .into_iter()
        .map(|row| ModelEntry {
            id: row.slug,
            context_window_tokens: row.context_window_tokens,
            runnable: false,
            efforts: Vec::new(),
        })
        .collect();
    Ok((defaults, Some(notice), catalog_rows))
}

async fn review_chatgpt_risk(
    terminal: &mut AttachedTerminal,
    storage: AccountStorage,
) -> Result<Option<RiskAcknowledgment>, SetupError> {
    let prompt = RiskPrompt::new(storage);
    let warning = prompt.text();
    if review_warning(
        terminal,
        "ChatGPT plan consent",
        &warning,
        "Accept the plan and storage risks?",
    )
    .await?
    {
        prompt
            .accept("Accept")
            .map(Some)
            .map_err(SetupError::recoverable)
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_accounts_with_colliding_prefixes_have_distinct_labels() {
        let first = uuid::Uuid::parse_str("019ad2a1-0000-7000-8000-000000000001").unwrap();
        let second = uuid::Uuid::parse_str("019ad2a1-0000-7000-8000-000000000002").unwrap();
        assert_ne!(
            chatgpt_account_label(first, first),
            chatgpt_account_label(second, first)
        );
        assert!(chatgpt_account_label(second, first).contains(&second.to_string()));
    }

    #[test]
    fn chatgpt_setup_selects_low_effort_from_the_matching_visible_catalog() {
        let account_id = uuid::Uuid::now_v7();
        let unverified = chatgpt_defaults(account_id);
        assert_eq!(unverified.account_id, Some(account_id));
        assert!(unverified.model.is_none());
        assert!(unverified.effort.is_none());
        assert!(
            ChatGptSetupAccount::Existing(account_id)
                .finish(chatgpt_defaults(account_id), None)
                .expect("saved-account cancellation")
                .is_none(),
            "cancelling saved-account setup must preserve Session defaults"
        );
        assert!(matches!(
            ChatGptSetupAccount::Connected(account_id).finish(unverified, None),
            Ok(Some(SetupSelection::ChatGpt { defaults, notice: None, .. }))
                if defaults.account_id == Some(account_id)
                && defaults.model.is_none()
        ));
        assert!(matches!(
            ChatGptSetupAccount::Switched(account_id)
                .finish(chatgpt_defaults(account_id), None),
            Ok(Some(SetupSelection::ChatGpt { defaults, notice: None, .. }))
                if defaults.account_id == Some(account_id)
                && defaults.model.is_none()
        ));

        let failed_catalog = "Model catalog failed; retry /model".to_owned();
        for account in [
            ChatGptSetupAccount::Switched(account_id),
            ChatGptSetupAccount::Connected(account_id),
            ChatGptSetupAccount::Reauthorized(account_id),
        ] {
            assert!(matches!(
                account.finish(chatgpt_defaults(account_id), Some(failed_catalog.clone())),
                Ok(Some(SetupSelection::ChatGpt { defaults, notice: Some(notice), .. }))
                    if defaults.account_id == Some(account_id)
                    && defaults.model.is_none()
                    && notice == failed_catalog
            ));
        }
        assert_eq!(
            ChatGptSetupAccount::Existing(account_id)
                .finish(chatgpt_defaults(account_id), Some(failed_catalog.clone()))
                .err(),
            Some(failed_catalog),
            "a failed saved-account reuse must preserve prior Session defaults"
        );

        let mut models = vec![chatgpt::ChatGptModel {
            slug: "model-one".into(),
            display_name: "First visible model".into(),
            context_window_tokens: None,
        }];
        let selected = catalog_chatgpt_defaults(account_id, account_id, &models)
            .expect("matching account catalog");
        assert_eq!(selected.model.as_deref(), Some("model-one"));
        assert_eq!(selected.effort, Some(Effort::Low));
        assert!(matches!(
            ChatGptSetupAccount::Existing(account_id).finish(selected, None),
            Ok(Some(SetupSelection::ChatGpt { defaults, notice: None, .. }))
                if defaults.model.as_deref() == Some("model-one")
                && defaults.effort == Some(Effort::Low)
        ));
        models.push(chatgpt::ChatGptModel {
            slug: "gpt-5.6-luna".into(),
            display_name: "GPT-5.6 Luna".into(),
            context_window_tokens: None,
        });
        let selected = catalog_chatgpt_defaults(account_id, account_id, &models).unwrap();
        assert_eq!(selected.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(selected.effort, Some(Effort::Low));
        assert_eq!(
            models[0].slug, "model-one",
            "selection must not reorder catalog rows"
        );
        assert!(catalog_chatgpt_defaults(account_id, account_id, &[]).is_err());
        assert!(catalog_chatgpt_defaults(account_id, uuid::Uuid::now_v7(), &models,).is_err());
    }
}
