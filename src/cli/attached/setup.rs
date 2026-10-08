use crate::cli::chatgpt::{
    self, CallbackListener, RiskAcknowledgment, RiskPrompt, registration::Registration,
};
use crate::cli::credentials::{self, AccountStorage, CredentialError, SavedAccount};
use arany::{
    AttachedTerminal, CollaborationPolicy, Composer, Effort, MAX_ANTHROPIC_WORKSPACE_ID_BYTES,
    MAX_NATIVE_API_KEY_BYTES, ModelEntry, NativeApiCredentials, SessionDefaults, ShutdownSignal,
    StateRoot, StoreError, TerminalError, TerminalInput, list_native_models_with_credentials,
    validate_native_api_key,
};
use std::future::Future;
use std::path::Path;

pub(super) enum SetupError {
    Recoverable(String),
    Presentation(String),
    Shutdown(ShutdownSignal),
}

impl From<String> for SetupError {
    fn from(message: String) -> Self {
        arany::record_development_failure(arany::DevelopmentFailure::Setup);
        Self::Recoverable(message)
    }
}

impl From<&str> for SetupError {
    fn from(message: &str) -> Self {
        arany::record_development_failure(arany::DevelopmentFailure::Setup);
        Self::Recoverable(message.to_owned())
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
            SetupError::Recoverable(message) | SetupError::Presentation(message) => message,
            SetupError::Shutdown(signal) => format!("terminated by {}", signal.name()),
        }
    }
}

pub(super) enum SetupSelection {
    Native {
        defaults: SessionDefaults,
        notice: String,
        catalog: Vec<ModelEntry>,
    },
    ChatGpt {
        defaults: SessionDefaults,
        notice: Option<String>,
        catalog: Vec<ModelEntry>,
    },
}

pub(super) enum SavedDefaults {
    Unconfigured,
    Cancelled,
    Selected(SessionDefaults),
    AccessChecked(SessionDefaults),
}

enum ChatGptSetupAccount {
    Existing(uuid::Uuid),
    Switched(uuid::Uuid),
    Connected(uuid::Uuid),
    Reauthorized(uuid::Uuid),
    PermissionDisabled { id: uuid::Uuid, local_cleared: bool },
}

impl ChatGptSetupAccount {
    fn id(&self) -> uuid::Uuid {
        match self {
            Self::Existing(id)
            | Self::Switched(id)
            | Self::Connected(id)
            | Self::Reauthorized(id) => *id,
            Self::PermissionDisabled { id, .. } => *id,
        }
    }

    fn status(&self) -> &'static str {
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

    fn finish(
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

pub(super) async fn resolve(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    workspace: &Path,
) -> Result<Option<SetupSelection>, SetupError> {
    wizard(terminal, state_dir, workspace).await
}

pub(super) async fn saved_defaults(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
) -> Result<SavedDefaults, String> {
    if let Some(defaults) = super::models::last_saved_defaults(workspace)? {
        let disconnected = if defaults.provider.as_deref() == Some("chatgpt") {
            match chatgpt::selected_account_id(workspace) {
                Ok(_) => false,
                Err(chatgpt::AuthorizationError::NoSelectedAccount) => true,
                Err(error) => return Err(error.to_string()),
            }
        } else {
            false
        };
        if !disconnected {
            return Ok(SavedDefaults::Selected(defaults));
        }
    }
    let path = StateRoot::account_path().map_err(|error| error.to_string())?;
    let legacy = StateRoot::default_path().map_err(|error| error.to_string())?;
    let (native, chatgpt) = saved_account_presence(&path, &legacy)?;
    match (native, chatgpt) {
        (true, true) => match choose(
            terminal,
            1,
            "Choose saved access",
            "Both saved; choose billing route",
            &[(b'0', "Cancel"), (b'1', "API key"), (b'2', "ChatGPT plan")],
        )
        .await?
        {
            Some(b'1') => saved_native_defaults(terminal, workspace)
                .await
                .map_err(Into::into),
            Some(b'2') => selected_chatgpt_defaults(workspace),
            Some(b'0') | None => Ok(SavedDefaults::Cancelled),
            _ => Err("invalid saved access choice".into()),
        },
        (true, false) => saved_native_defaults(terminal, workspace)
            .await
            .map_err(Into::into),
        (false, true) => selected_chatgpt_defaults(workspace),
        (false, false) => Ok(SavedDefaults::Unconfigured),
    }
}

pub(super) async fn authorize_saved_access(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    defaults: &SessionDefaults,
) -> Result<bool, SetupError> {
    let target = selected_keyring_slot(workspace, defaults).await?;
    let Some(slot) = target else {
        return Ok(true);
    };
    let expected = defaults
        .account_id
        .zip(defaults.provider.clone())
        .filter(|(_, provider)| matches!(provider.as_str(), "openai" | "anthropic"));
    let Some(result) = await_operation(
        terminal,
        "Authorize saved access",
        "Approve this Arany credential in your OS password store; Esc or Ctrl+C exits",
        credentials::authorize_keyring_slot(slot.clone(), expected),
    )
    .await?
    else {
        return Ok(false);
    };
    if !result.map_err(|error| error.to_string())? {
        return Err(
            "Saved sign-in is missing from the OS password store; use /setup to reconnect".into(),
        );
    }
    if selected_keyring_slot(workspace, defaults).await? != Some(slot) {
        return Err("Saved account changed during authorization; restart Arany".into());
    }
    Ok(true)
}

async fn selected_keyring_slot(
    workspace: &Path,
    defaults: &SessionDefaults,
) -> Result<Option<String>, SetupError> {
    let Some(id) = defaults.account_id else {
        return Ok(None);
    };
    match defaults.provider.as_deref() {
        Some("chatgpt") => {
            chatgpt::selected_keyring_slot(workspace, id).map_err(|error| error.to_string().into())
        }
        Some(provider @ ("openai" | "anthropic")) => {
            credentials::selected_keyring_slot(workspace, id, provider)
                .await
                .map_err(|error| error.to_string().into())
        }
        _ => Ok(None),
    }
}

fn saved_account_presence(current: &Path, legacy: &Path) -> Result<(bool, bool), String> {
    let mut native = false;
    let mut chatgpt = false;
    for path in std::iter::once(current).chain((legacy != current).then_some(legacy)) {
        match crate::cli::open_optional_state(path) {
            Ok(Some(state)) => {
                native |= state
                    .saved_account_record_present()
                    .map_err(|error| error.to_string())?;
                chatgpt |= state
                    .chatgpt_accounts_record_present()
                    .map_err(|error| error.to_string())?;
            }
            Ok(None) => {}
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("private account state unavailable".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok((native, chatgpt))
}

async fn saved_native_defaults(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
) -> Result<SavedDefaults, SetupError> {
    let Some(inspected) = await_operation(
        terminal,
        "Authorize saved access",
        "Approve this Arany credential in your OS password store; Esc or Ctrl+C exits",
        credentials::inspect_interactive(workspace),
    )
    .await?
    else {
        return Ok(SavedDefaults::Cancelled);
    };
    let account = inspected
        .map_err(|error| error.to_string())?
        .account
        .ok_or("saved API account unavailable; use /setup to repair it")?;
    Ok(SavedDefaults::AccessChecked(defaults(&account)))
}

fn selected_chatgpt_defaults(workspace: &Path) -> Result<SavedDefaults, String> {
    match chatgpt::selected_account_id(workspace) {
        Ok(id) => Ok(SavedDefaults::Selected(chatgpt_defaults(id))),
        Err(chatgpt::AuthorizationError::NoSelectedAccount) => Ok(SavedDefaults::Unconfigured),
        Err(error) => Err(error.to_string()),
    }
}

fn chatgpt_defaults(account_id: uuid::Uuid) -> SessionDefaults {
    SessionDefaults {
        provider: Some("chatgpt".into()),
        model: None,
        effort: None,
        account_id: Some(account_id),
        policy: CollaborationPolicy::default(),
    }
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

async fn inspect_or_file(workspace: &Path) -> Result<credentials::InspectedAccount, String> {
    Ok(match credentials::inspect(workspace).await {
        Ok(inspected) => inspected,
        Err(CredentialError::Unavailable | CredentialError::TimedOut) => {
            credentials::InspectedAccount {
                account: None,
                storage: AccountStorage::PrivateFile,
            }
        }
        Err(error) => return Err(error.to_string()),
    })
}

async fn wizard(
    terminal: &mut AttachedTerminal,
    _state_dir: &Path,
    workspace: &Path,
) -> Result<Option<SetupSelection>, SetupError> {
    let Some(access) = choose(
        terminal,
        1,
        "Choose access method",
        "ChatGPT plan uses your subscription",
        &[(b'1', "API key"), (b'2', "ChatGPT plan")],
    )
    .await?
    else {
        return Ok(None);
    };
    if access == b'2' {
        let account = match chatgpt::selected_registration(workspace) {
            Ok((saved, true)) => match choose(
                terminal,
                2,
                "ChatGPT account",
                "Reuse, reconnect, or add",
                &[
                    (b'1', "Use saved"),
                    (b'2', "Reconnect"),
                    (b'3', "Connect new"),
                ],
            )
            .await?
            {
                Some(b'1') => choose_saved_chatgpt(terminal, workspace, saved).await?,
                Some(b'2') => {
                    let target = chatgpt::selected_reauthorization_target(workspace, saved)
                        .map_err(|error| error.to_string())?;
                    reconnect_chatgpt(terminal, workspace, target).await?
                }
                Some(b'3') => connect_chatgpt(terminal, workspace).await?,
                None => None,
                Some(_) => return Err("invalid ChatGPT account choice".into()),
            },
            Ok((saved, false)) => {
                let target = chatgpt::selected_reauthorization_target(workspace, saved)
                    .map_err(|error| error.to_string())?;
                let permission_missing = target.plan_permission_missing;
                let reconnect_label = if permission_missing {
                    "Enable plan"
                } else {
                    "Reconnect"
                };
                let (_, ids) =
                    chatgpt::saved_account_ids(workspace).map_err(|error| error.to_string())?;
                let other_saved = ids.len() > 1;
                let reconnect_choice = if other_saved { b'2' } else { b'1' };
                let new_choice = if other_saved { b'3' } else { b'2' };
                let choices = if other_saved {
                    &[
                        (b'1', "Use saved"),
                        (b'2', reconnect_label),
                        (b'3', "Connect new"),
                    ][..]
                } else {
                    &[(b'1', reconnect_label), (b'2', "Connect new")][..]
                };
                match choose(
                    terminal,
                    2,
                    if permission_missing {
                        "ChatGPT plan disabled"
                    } else {
                        "ChatGPT account disconnected"
                    },
                    if permission_missing {
                        "Enable plan or choose another account"
                    } else {
                        "Reconnect or add"
                    },
                    choices,
                )
                .await?
                {
                    Some(b'1') if other_saved => {
                        choose_saved_chatgpt(terminal, workspace, saved).await?
                    }
                    Some(choice) if choice == reconnect_choice => {
                        reconnect_chatgpt(terminal, workspace, target).await?
                    }
                    Some(choice) if choice == new_choice => {
                        connect_chatgpt(terminal, workspace).await?
                    }
                    None => None,
                    Some(_) => return Err("invalid ChatGPT account choice".into()),
                }
            }
            Err(chatgpt::AuthorizationError::NoSelectedAccount) => {
                connect_chatgpt(terminal, workspace).await?
            }
            Err(error) => return Err(error.to_string().into()),
        };
        let Some(account) = account else {
            return Ok(None);
        };
        let (defaults, notice, catalog) = configure_chatgpt(terminal, workspace, &account).await?;
        let mut selection = account.finish(defaults, notice).map_err(SetupError::from)?;
        if let Some(SetupSelection::ChatGpt { catalog: rows, .. }) = &mut selection {
            *rows = catalog;
        }
        return Ok(selection);
    }
    let storage = inspect_or_file(workspace).await?.storage;
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
        "openai"
    } else {
        "anthropic"
    };
    let Some(key) = read_api_key(terminal).await? else {
        return Ok(None);
    };
    let workspace_id = if provider == "anthropic" {
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
    let credentials = NativeApiCredentials::new(provider, key, workspace_id)
        .map_err(|_| "invalid selected native API credentials")?;
    let Some(items) = load_catalog(terminal, provider, &credentials).await? else {
        return Ok(None);
    };
    if items.is_empty() {
        return Err("selected account has no visible models; use /setup to retry".into());
    }
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
    .map_err(|error| error.to_string())?;
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
    credentials::save(workspace, account, storage)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some(SetupSelection::Native {
        defaults,
        notice,
        catalog: items,
    }))
}

async fn choose_saved_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    expected_selected: uuid::Uuid,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let (selected, ids) =
        chatgpt::saved_account_ids(workspace).map_err(|error| error.to_string())?;
    if selected != expected_selected {
        return Err("ChatGPT account changed during setup; use /setup again".into());
    }
    let connected = chatgpt::selected_account_id(workspace).ok() == Some(selected);
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
        chatgpt::select_saved_account(workspace.to_path_buf(), selected, target)
            .await
            .map_err(|error| error.to_string())?;
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
    chatgpt::select_saved_account(workspace.to_path_buf(), selected, target)
        .await
        .map_err(|error| error.to_string())?;
    Ok(Some(ChatGptSetupAccount::Switched(target)))
}

async fn connect_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    terminal.draw_setup(
        0,
        "Checking keyring",
        "Selecting protected account storage",
        0,
        None,
    )?;
    let storage = if chatgpt_keyring_available(credentials::probe_chatgpt_keyring().await)? {
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
    authorize_chatgpt(terminal, workspace, storage, None).await
}

async fn reconnect_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    target: chatgpt::ReauthorizationTarget,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    if target.storage == AccountStorage::Keyring
        && !chatgpt_keyring_available(credentials::probe_chatgpt_keyring().await)?
    {
        return Err("Saved ChatGPT keyring is unavailable. Unlock your desktop password store and retry Reconnect; or choose Connect new, then Use private file (not encrypted)".into());
    }
    authorize_chatgpt(terminal, workspace, target.storage, Some(target)).await
}

async fn authorize_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    storage: AccountStorage,
    target: Option<chatgpt::ReauthorizationTarget>,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let Some(acknowledgment) = review_chatgpt_risk(terminal, storage).await? else {
        return Ok(None);
    };
    let registration =
        Registration::open_or_create(workspace).map_err(|error| error.to_string())?;
    if target
        .as_ref()
        .is_some_and(|selected| selected.host_id != registration.host_id())
    {
        return Err("ChatGPT registration changed; restart /setup".into());
    }
    if target.is_none()
        && registration.has_unfinished_sign_in()
        && choose(
            terminal,
            4,
            "Resume sign-in?",
            "Reuse saved registration",
            &[(b'0', "Back"), (b'1', "Resume")],
        )
        .await?
            != Some(b'1')
    {
        return Ok(None);
    }
    let listener = CallbackListener::bind(acknowledgment)
        .await
        .map_err(|error| error.to_string())?;
    let attempt = match &target {
        Some(selected) => chatgpt::AuthorizationAttempt::for_existing(selected, listener.port()),
        None => registration.authorization_attempt(listener.port()),
    }
    .map_err(|error| error.to_string())?;
    chatgpt::open_authorization_url(&attempt.authorization_url())
        .await
        .map_err(|_| {
            "Could not open the system browser; no ChatGPT account was changed".to_owned()
        })?;
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
        "Checking the signed account and plan permission",
        exchange.redeem(workspace),
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
    let expected = target.as_ref().map(|selected| selected.id);
    match credentials {
        chatgpt::VerifiedSignIn::PlanEnabled(credentials) => {
            let id = chatgpt::save_verified(
                workspace.to_path_buf(),
                credentials,
                acknowledgment,
                expected,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(Some(if expected.is_some() {
                ChatGptSetupAccount::Reauthorized(id)
            } else {
                ChatGptSetupAccount::Connected(id)
            }))
        }
        chatgpt::VerifiedSignIn::PlanDisabled(identity) => {
            let outcome = chatgpt::save_without_plan_permission(
                workspace.to_path_buf(),
                identity,
                acknowledgment,
                expected,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(Some(ChatGptSetupAccount::PermissionDisabled {
                id: outcome.id,
                local_cleared: outcome.local_cleared,
            }))
        }
    }
}

fn chatgpt_keyring_available(probe: Result<(), CredentialError>) -> Result<bool, String> {
    match probe {
        Ok(()) => Ok(true),
        Err(CredentialError::Unavailable | CredentialError::TimedOut) => Ok(false),
        Err(CredentialError::Locked) => Err(
            "OS credential store is locked. Unlock your desktop password store, then retry /setup"
                .into(),
        ),
        Err(error) => Err(error.to_string()),
    }
}

async fn configure_chatgpt(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    account: &ChatGptSetupAccount,
) -> Result<(SessionDefaults, Option<String>, Vec<ModelEntry>), SetupError> {
    let account_id = account.id();
    let defaults = chatgpt_defaults(account_id);
    if let ChatGptSetupAccount::PermissionDisabled { local_cleared, .. } = account {
        let cleanup = if *local_cleared {
            ""
        } else {
            "; removal of the previous keyring token was not confirmed"
        };
        return Ok((
            defaults,
            Some(format!(
                "Verified ChatGPT sign-in saved; plan usage is disabled{cleanup}. Use /setup to enable the plan or explicitly choose an API account; no model check or Run started"
            )),
            Vec::new(),
        ));
    }
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
            return Ok((
                defaults,
                Some(format!(
                    "Model catalog failed; {}. Use /provider chatgpt then /model to retry: {error}",
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
    if terminal.is_linear() {
        terminal.draw_setup_warning(&warning, 0, false, None)?;
        return match choose(
            terminal,
            2,
            "ChatGPT plan consent",
            "Accept the plan and storage risks?",
            &[(b'1', "Back"), (b'2', "Accept")],
        )
        .await?
        {
            Some(b'2') => prompt
                .accept("Accept")
                .map(Some)
                .map_err(|error| error.to_string().into()),
            _ => Ok(None),
        };
    }
    let mut page = 0usize;
    let mut accept_selected = false;
    let mut notice = None::<&str>;
    loop {
        let has_more =
            terminal.draw_setup_warning(&warning, page, accept_selected, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit if has_more => page += 1,
            TerminalInput::PageDown | TerminalInput::Down | TerminalInput::Tab if has_more => {
                page += 1;
            }
            TerminalInput::PageUp => {
                page = page.saturating_sub(1);
                accept_selected = false;
            }
            TerminalInput::Up if has_more => page = page.saturating_sub(1),
            TerminalInput::Left | TerminalInput::Up if !has_more => accept_selected = false,
            TerminalInput::Right | TerminalInput::Down if !has_more => accept_selected = true,
            TerminalInput::Tab if !has_more => accept_selected = !accept_selected,
            TerminalInput::Character(_) => {
                notice = Some(if has_more {
                    "Read every page before accepting"
                } else {
                    "Tab chooses; Enter confirms"
                });
            }
            TerminalInput::Submit if accept_selected => {
                return prompt
                    .accept("Accept")
                    .map(Some)
                    .map_err(|error| error.to_string().into());
            }
            TerminalInput::Submit => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                accept_selected = false;
                terminal.discard_draft_input();
                notice = Some("Tab chooses; Enter confirms");
            }
            TerminalInput::Resize => {
                page = 0;
                accept_selected = false;
                notice = Some("Resize: reread");
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(0)?;
                page = 0;
                accept_selected = false;
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

async fn await_chatgpt_step<T>(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    operation: impl Future<Output = Result<T, chatgpt::AuthorizationError>>,
) -> Result<Option<T>, SetupError> {
    await_operation(terminal, title, instruction, operation)
        .await?
        .transpose()
        .map_err(|error| error.to_string().into())
}

pub(super) async fn await_operation<T>(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    operation: impl Future<Output = T>,
) -> Result<Option<T>, SetupError> {
    tokio::pin!(operation);
    loop {
        terminal.draw_setup(4, title, instruction, 0, None)?;
        tokio::select! {
            result = &mut operation => return Ok(Some(result)),
            input = terminal.next_input() => match input? {
                TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                    terminal.discard_draft_input();
                    return Ok(None);
                }
                TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
                TerminalInput::Suspend => terminal.suspend_and_resume(0)?,
                TerminalInput::LineRejected | TerminalInput::LineContinued => terminal.discard_draft_input(),
                _ => {}
            },
        }
    }
}

fn native_catalog_default<'a>(provider: &str, items: &'a [ModelEntry]) -> Option<(&'a str, bool)> {
    let preferred: &[&str] = match provider {
        "openai" => &["gpt-5.6-luna"],
        "anthropic" => &["claude-sonnet-5-5", "claude-sonnet-5"],
        _ => return None,
    };
    preferred
        .iter()
        .find_map(|id| items.iter().find(|item| item.id == *id))
        .map(|item| (item.id.as_str(), true))
        .or_else(|| items.first().map(|item| (item.id.as_str(), false)))
}

fn defaults(account: &SavedAccount) -> SessionDefaults {
    SessionDefaults {
        provider: Some(account.provider.clone()),
        model: Some(account.model.clone()),
        effort: account.effort,
        account_id: Some(account.id),
        policy: CollaborationPolicy::default(),
    }
}

async fn choose(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
) -> Result<Option<u8>, SetupError> {
    choose_focused(terminal, step, title, instruction, choices, None).await
}

pub(super) async fn choose_focused(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    current: Option<u8>,
) -> Result<Option<u8>, SetupError> {
    choose_display(terminal, step, title, instruction, choices, current).await
}

async fn choose_display(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    current: Option<u8>,
) -> Result<Option<u8>, SetupError> {
    let mut selected = choices
        .iter()
        .position(|(value, _)| Some(*value) == current)
        .unwrap_or(0);
    let mut input = String::new();
    let mut rejected = 0u16;
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(step, title, instruction, choices, selected, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Character(character) if terminal.is_linear() => {
                if rejected == 0
                    && (character.is_ascii_graphic() || character == ' ')
                    && input.len() < 64
                {
                    input.push(character);
                } else {
                    rejected = rejected.saturating_add(1);
                    notice = Some("Invalid choice input; use Backspace to correct");
                }
            }
            TerminalInput::Character(_) => notice = Some("Use Up/Down and Enter to select"),
            TerminalInput::Backspace => {
                if rejected > 0 {
                    rejected -= 1;
                } else {
                    input.pop();
                }
            }
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down | TerminalInput::Tab => {
                selected = (selected + 1).min(choices.len() - 1);
            }
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = choices.len() - 1,
            TerminalInput::Submit => {
                if !terminal.is_linear() || (rejected == 0 && input.trim().is_empty()) {
                    return Ok(Some(choices[selected].0));
                }
                if rejected == 0
                    && let Some((value, _)) = choices
                        .iter()
                        .find(|(_, label)| label.eq_ignore_ascii_case(input.trim()))
                {
                    return Ok(Some(*value));
                }
                terminal.discard_draft_input();
                input.clear();
                rejected = 0;
                notice = Some("Type a listed choice name; Ctrl+C cancels");
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                input.clear();
                rejected = 0;
                terminal.discard_draft_input();
                notice = Some("Invalid choice line; Ctrl+C cancels");
            }
            TerminalInput::Suspend => terminal.suspend_and_resume(input.len())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
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
    loop {
        terminal.draw_setup(
            3,
            "Enter API key",
            "Hidden input; up to 512 ASCII characters",
            key.len(),
            notice.take(),
        )?;
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
                terminal.suspend_and_resume(0)?;
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
    loop {
        terminal.draw_setup_text_input(
            "API workspace ID",
            "From Claude Console; wrkspc_ followed by letters or digits, up to 128 characters",
            &draft,
            notice.take(),
        )?;
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
                        "anthropic",
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
            TerminalInput::Suspend => terminal.suspend_and_resume(draft.text().len())?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
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
    .map_err(|error| error.to_string().into())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let legacy = StateRoot::admit(&temp.path().join("legacy")).unwrap();
        assert_eq!(
            saved_account_presence(&current, legacy.path()).unwrap(),
            (false, false)
        );
        legacy
            .replace_chatgpt_accounts_record(b"not a parsed account index")
            .unwrap();
        assert_eq!(
            saved_account_presence(&current, legacy.path()).unwrap(),
            (false, true)
        );
        let root = StateRoot::admit(&current).unwrap();
        root.replace_saved_account_record(b"not a parsed native account")
            .unwrap();
        assert_eq!(
            saved_account_presence(&current, legacy.path()).unwrap(),
            (true, true)
        );
        assert_eq!(
            saved_account_presence(&current, &current).unwrap(),
            (true, false)
        );
    }

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
            ChatGptSetupAccount::PermissionDisabled {
                id: account_id,
                local_cleared: true,
            },
            ChatGptSetupAccount::PermissionDisabled {
                id: account_id,
                local_cleared: false,
            },
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

    #[test]
    fn chatgpt_file_fallback_never_treats_locked_keyring_as_missing() {
        assert_eq!(chatgpt_keyring_available(Ok(())), Ok(true));
        assert_eq!(
            chatgpt_keyring_available(Err(CredentialError::Unavailable)),
            Ok(false)
        );
        assert_eq!(
            chatgpt_keyring_available(Err(CredentialError::TimedOut)),
            Ok(false)
        );
        assert!(chatgpt_keyring_available(Err(CredentialError::Locked)).is_err());
    }
}
