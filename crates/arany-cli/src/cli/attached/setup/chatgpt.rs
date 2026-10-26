use super::{
    SetupError, SetupJournal, SetupSelection, choose, configure_chatgpt, connect_chatgpt,
    inspect_chatgpt_account, native, reconnect_chatgpt, select_chatgpt_account,
};
use crate::accounts::{chatgpt, credentials};
use crate::terminal::AttachedTerminal;
use arany::SessionDefaults;
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
