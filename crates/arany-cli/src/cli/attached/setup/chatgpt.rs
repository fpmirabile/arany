use super::{
    SavedDefaults, SetupError, SetupJournal, SetupSelection, await_setup_ack, choose,
    configure_chatgpt, connect_chatgpt, error::chatgpt_edit_is_uncertain, native,
    reconnect_chatgpt, review_warning,
};
use crate::accounts::{
    chatgpt,
    credentials::{self, AccountStorage},
};
use crate::terminal::AttachedTerminal;
use arany::{
    CollaborationPolicy, ProviderKind, ProviderSetupDisposition, ProviderSetupTargetKind,
    SessionDefaults,
};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(super) enum ChatGptSetupAccount {
    Existing(uuid::Uuid),
    Switched(uuid::Uuid),
    Connected(uuid::Uuid),
    Reauthorized(uuid::Uuid),
    PermissionDisabled { id: uuid::Uuid, local_cleared: bool },
}

impl ChatGptSetupAccount {
    pub(super) fn id(&self) -> uuid::Uuid {
        match self {
            Self::Existing(id)
            | Self::Switched(id)
            | Self::Connected(id)
            | Self::Reauthorized(id) => *id,
            Self::PermissionDisabled { id, .. } => *id,
        }
    }

    pub(super) fn status(&self) -> &'static str {
        match self {
            Self::Existing(_) => "saved ChatGPT account remains selected",
            Self::Switched(_) => "saved ChatGPT account was selected",
            Self::Connected(_) => "new ChatGPT account was saved",
            Self::Reauthorized(_) => "ChatGPT account was reconnected",
            Self::PermissionDisabled { .. } => {
                "verified ChatGPT sign-in was saved; plan usage is disabled"
            }
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
        match chatgpt_account_action(terminal, saved, connected).await? {
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
    saved: uuid::Uuid,
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
        let (_, accounts) = chatgpt::account_inspections().map_err(SetupError::recoverable)?;
        let permission_missing = accounts
            .iter()
            .find(|account| account.id == saved)
            .is_some_and(|account| account.access == chatgpt::AccountAccess::PlanDisabled);
        let (_, ids) = chatgpt::saved_account_ids().map_err(SetupError::recoverable)?;
        if permission_missing {
            (
                "ChatGPT plan disabled",
                "Inspect, enable plan or choose another account",
                "Enable plan",
                ids.len() > 1,
            )
        } else {
            (
                "ChatGPT account disconnected",
                "Inspect, reconnect or add",
                "Reconnect",
                ids.len() > 1,
            )
        }
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

pub(super) fn chatgpt_defaults(account_id: uuid::Uuid) -> SessionDefaults {
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
        chatgpt::AccountAccess::PlanDisabled => "unavailable; plan permission disabled",
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
}
