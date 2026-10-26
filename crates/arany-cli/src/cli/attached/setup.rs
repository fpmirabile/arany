#[path = "setup/chatgpt.rs"]
mod chatgpt_flow;
mod error;
mod native;
mod roster;
mod routing;

use chatgpt_flow::{
    ChatGptSetupAccount, chatgpt_defaults, chatgpt_setup, remove_chatgpt_saved_access,
    selected_chatgpt_defaults,
};
pub(super) use error::SetupError;
use error::chatgpt_keyring_available;

use crate::accounts::chatgpt::{
    self, CallbackListener, RiskAcknowledgment, RiskPrompt, registration::Registration,
};
use crate::accounts::credentials::{self, AccountStorage};
use crate::presentation::{escape_terminal, format_duration};
use crate::terminal::{AttachedTerminal, Composer, TerminalInput};
#[cfg(all(test, unix, debug_assertions))]
use arany::RecordKind;
use arany::{
    CollaborationPolicy, CustomCheckStatus, CustomProfile, CustomProfileDraft, CustomProfileError,
    CustomProtocol, Effort, Event, HistoryRecovery, MAX_ANTHROPIC_WORKSPACE_ID_BYTES,
    MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES, MAX_CUSTOM_PROFILE_EFFORTS_BYTES,
    MAX_CUSTOM_PROFILE_ENDPOINT_BYTES, MAX_CUSTOM_PROFILE_MODEL_BYTES,
    MAX_CUSTOM_PROFILE_NAME_BYTES, MAX_NATIVE_API_KEY_BYTES, MessagesAuth, ModelEntry,
    NativeApiCredentials, ProviderKind, ProviderSetupDisposition, ProviderSetupTarget,
    ProviderSetupTargetKind, RunCancellation, SessionDefaults, SessionId, StateRoot, Store,
    StoreError, create_session, validate_native_api_key,
};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::path::{Path, PathBuf};

const AUTHORIZE_TITLE: &str = "Authorize saved access";
const AUTHORIZE_PROMPT: &str =
    "Approve this Arany credential in your OS password store; Esc or Ctrl+C exits";
const SETUP_MESSAGE_PAGE_CHARS: usize = 36;
const MAX_SETUP_CHOICE_INPUT_BYTES: usize = 64;

#[cfg(all(test, unix, debug_assertions))]
#[path = "../../../tests/session_run/setup/removal.rs"]
mod removal;

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
    Custom {
        defaults: SessionDefaults,
        notice: String,
    },
}

pub(super) enum SavedDefaults {
    Unconfigured,
    Cancelled,
    Selected(SessionDefaults),
    AccessChecked(SessionDefaults),
}

pub(super) struct SetupJournal {
    state_dir: PathBuf,
    workspace: PathBuf,
    session_id: Option<SessionId>,
    conversation: Option<arany::SessionView>,
}

impl SetupJournal {
    pub(super) fn new(state_dir: &Path, workspace: &Path, session_id: Option<SessionId>) -> Self {
        Self {
            state_dir: state_dir.to_owned(),
            workspace: workspace.to_owned(),
            session_id,
            conversation: None,
        }
    }

    pub(super) fn for_conversation(
        state_dir: &Path,
        workspace: &Path,
        view: &arany::SessionView,
    ) -> Self {
        let mut journal = Self::new(
            state_dir,
            workspace,
            (view.created_sequence != 0).then_some(view.id),
        );
        if journal.session_id.is_none() {
            journal.conversation = Some(view.clone());
        }
        journal
    }

    pub(super) fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    async fn start(
        &mut self,
        kind: ProviderSetupTargetKind,
        identity: String,
        expected_digest: Option<[u8; 32]>,
    ) -> Result<uuid::Uuid, SetupError> {
        if self.session_id.is_none() {
            self.session_id = Some(if let Some(view) = &self.conversation {
                super::materialize_conversation(&self.state_dir, &self.workspace, view)
                    .await?
                    .id
            } else {
                let root = StateRoot::admit(&self.state_dir)
                    .map_err(|_| "state directory unavailable or unsafe")?;
                create_session(root, self.workspace.clone(), None)
                    .await
                    .map_err(SetupError::recoverable)?
            });
        }
        arany::start_provider_setup_edit(
            StateRoot::open_existing(&self.state_dir)
                .map_err(|_| "state directory unavailable or unsafe")?,
            self.workspace.clone(),
            self.session_id.expect("setup Session was materialized"),
            ProviderSetupTarget { kind, identity },
            expected_digest,
        )
        .await
        .map_err(|_| SetupError::Presentation("could not journal Provider setup edit".into()))
    }

    async fn finish_record(
        &self,
        operation: uuid::Uuid,
        bytes: Option<&[u8]>,
        result: Result<(), StoreError>,
        label: &str,
    ) -> Result<(), SetupError> {
        match result {
            Ok(()) => {
                self.finish(
                    operation,
                    Some(Sha256::digest(bytes.unwrap_or_default()).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await
            }
            Err(error) => {
                let uncertain = matches!(error, StoreError::ReplaceUnconfirmed);
                self.finish(
                    operation,
                    None,
                    if uncertain {
                        ProviderSetupDisposition::Uncertain
                    } else {
                        ProviderSetupDisposition::Failed
                    },
                )
                .await?;
                if uncertain {
                    Err(SetupError::Uncertain(format!(
                        "{label} publication is unknown; inspect saved state and start a new Session; no retry was attempted"
                    )))
                } else {
                    Err(SetupError::recoverable(error))
                }
            }
        }
    }

    async fn finish(
        &self,
        operation_id: uuid::Uuid,
        resulting_digest: Option<[u8; 32]>,
        disposition: ProviderSetupDisposition,
    ) -> Result<(), SetupError> {
        let store = Store::open(
            StateRoot::open_existing(&self.state_dir)
                .map_err(|_| "state directory unavailable or unsafe")?,
        )
        .map_err(|_| SetupError::Presentation("setup journal unavailable".into()))?;
        let result = store
            .append(
                self.session_id.expect("setup edit has a Session"),
                Event::ProviderSetupEditFinished {
                    operation_id,
                    resulting_digest,
                    disposition,
                },
            )
            .await;
        let close = store.close().await;
        result
            .map_err(|_| SetupError::Presentation("could not settle Provider setup edit".into()))?;
        close.map_err(|_| SetupError::Presentation("could not close setup journal".into()))
    }
}

pub(super) enum HistoryAdmission {
    Ready,
    Reset,
    Archived(arany::ArchivedHistory),
    Exit,
}

pub(super) async fn admit_history(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    workspace: &Path,
) -> Result<HistoryAdmission, SetupError> {
    let ready = |reset_attempted| {
        if reset_attempted {
            HistoryAdmission::Reset
        } else {
            HistoryAdmission::Ready
        }
    };
    let mut reset_attempted = false;
    let mut uncertain = false;
    loop {
        let root = match crate::files::open_optional_state(state_dir) {
            Ok(None) => return Ok(ready(reset_attempted)),
            Ok(Some(_)) => StateRoot::admit(state_dir),
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                StateRoot::admit(state_dir)
            }
            Err(error) => Err(error),
        };
        let opened = root.and_then(|root| {
            let archived = root.archive_incompatible_history()?;
            Store::open(root).map(|store| (store, archived))
        });
        let error = match opened {
            Ok((store, archived)) => {
                store.close().await.map_err(|_| "store shutdown failed")?;
                return Ok(archived.map_or(ready(reset_attempted), HistoryAdmission::Archived));
            }
            Err(error) => error,
        };
        let reset = if error.history_recovery().is_resettable() {
            StateRoot::open_existing(state_dir)
                .and_then(|root| root.prepare_history_reset(workspace))
                .map(Some)
        } else {
            Ok(None)
        };
        let reason = if uncertain {
            "Reset may be incomplete; check storage access and retry."
        } else {
            match reset.as_ref().err().unwrap_or(&error).history_recovery() {
                HistoryRecovery::InvalidHistory => "Saved history is incompatible or damaged.",
                HistoryRecovery::Damaged => "Saved history is damaged.",
                HistoryRecovery::ArchiveConflict => {
                    "An interrupted history archive no longer matches its files; inspect the archive folder in the state directory."
                }
                HistoryRecovery::MigrationConflict => {
                    "An interrupted history migration no longer matches its files; inspect the migrations folder in the state directory."
                }
                HistoryRecovery::Busy => "History is in use; close the other Arany and retry.",
                HistoryRecovery::Changed => "History changed during recovery; retry.",
                HistoryRecovery::StorageFull => "Storage is full; free space and retry.",
                HistoryRecovery::Inaccessible | HistoryRecovery::InvalidStateDirectory => {
                    "History files are unsafe or inaccessible; check permissions and retry."
                }
                HistoryRecovery::Unavailable => {
                    "History cannot be opened; check storage access and retry."
                }
            }
        };
        let reset = reset.ok().flatten();
        let instruction = if reset.is_some() {
            format!(
                "{reason} Reset deletes all conversations; keeps accounts, models and folder trust."
            )
        } else {
            reason.to_owned()
        };
        let choices = if reset.is_some() {
            &[(b'1', "Retry"), (b'2', "Reset history"), (b'0', "Exit")][..]
        } else {
            &[(b'1', "Retry"), (b'0', "Exit")][..]
        };
        match choose(terminal, 1, "History recovery", &instruction, choices).await? {
            Some(b'1') => {}
            Some(b'2') => {
                if review_warning(
                    terminal,
                    "Reset history?",
                    "Reset deletes all saved conversations and Provider evidence in this state directory. Saved accounts, model choices and folder trust are kept. This cannot be undone.",
                    "Delete all saved conversations?",
                ).await? {
                    reset_attempted = true;
                    uncertain = reset.ok_or("history reset unavailable")?.apply().is_err();
                }
            }
            _ => return Ok(HistoryAdmission::Exit),
        }
    }
}

pub(super) async fn saved_defaults(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
) -> Result<SavedDefaults, SetupError> {
    if let Some(defaults) = super::models::last_saved_defaults(workspace)? {
        let disconnected = if defaults.provider.as_deref().and_then(ProviderKind::parse)
            == Some(ProviderKind::ChatGpt)
        {
            match chatgpt::selected_account_id() {
                Ok(_) => false,
                Err(chatgpt::AuthorizationError::NoSelectedAccount) => true,
                Err(error) => return Err(SetupError::recoverable(error)),
            }
        } else {
            false
        };
        if !disconnected {
            return Ok(SavedDefaults::Selected(defaults));
        }
    }
    let path = StateRoot::account_path().map_err(SetupError::recoverable)?;
    let (native, chatgpt) = native::saved_account_presence(&path)?;
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
            Some(b'1') => native::saved_native_defaults(terminal).await,
            Some(b'2') => selected_chatgpt_defaults(),
            Some(b'0') | None => Ok(SavedDefaults::Cancelled),
            _ => Err("invalid saved access choice".into()),
        },
        (true, false) => native::saved_native_defaults(terminal).await,
        (false, true) => selected_chatgpt_defaults(),
        (false, false) => Ok(SavedDefaults::Unconfigured),
    }
}

pub(super) async fn authorize_saved_access(
    terminal: &mut AttachedTerminal,
    defaults: &SessionDefaults,
) -> Result<bool, SetupError> {
    let target = selected_keyring_slot(defaults).await?;
    let Some(slot) = target else {
        return Ok(true);
    };
    let expected = defaults
        .account_id
        .zip(defaults.provider.clone())
        .filter(|(_, provider)| {
            ProviderKind::parse(provider).is_some_and(ProviderKind::is_native_api)
        });
    let Some(result) = await_operation(
        terminal,
        AUTHORIZE_TITLE,
        AUTHORIZE_PROMPT,
        credentials::authorize_keyring_slot(slot.clone(), expected),
    )
    .await?
    else {
        return Ok(false);
    };
    if !result.map_err(SetupError::recoverable)? {
        return Err(
            "Saved sign-in is missing from the OS password store; use /setup to reconnect".into(),
        );
    }
    if selected_keyring_slot(defaults).await? != Some(slot) {
        return Err("Saved account changed during authorization; restart Arany".into());
    }
    Ok(true)
}

async fn selected_keyring_slot(defaults: &SessionDefaults) -> Result<Option<String>, SetupError> {
    let Some(id) = defaults.account_id else {
        return Ok(None);
    };
    match defaults.provider.as_deref().and_then(ProviderKind::parse) {
        Some(ProviderKind::ChatGpt) => {
            chatgpt::selected_keyring_slot(id).map_err(SetupError::recoverable)
        }
        Some(provider @ (ProviderKind::OpenAi | ProviderKind::Anthropic)) => {
            credentials::selected_keyring_slot(
                id,
                provider.native_name().expect("native Provider name"),
            )
            .await
            .map_err(SetupError::recoverable)
        }
        Some(ProviderKind::Custom) | None => Ok(None),
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

pub(super) async fn resolve(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    workspace: &Path,
    journal: &mut SetupJournal,
    current: &SessionDefaults,
) -> Result<Option<SetupSelection>, SetupError> {
    if super::models::preferences_need_reset(workspace)?
        && !native::reset_saved_setup(terminal, workspace, credentials::SetupData::Models, journal)
            .await?
    {
        return Ok(None);
    }
    let Some(access) = choose(
        terminal,
        1,
        "Provider setup",
        "Manage access, custom routes, role routing and roster presets",
        &[
            (b'1', "API accounts"),
            (b'2', "Custom routes"),
            (b'3', "Role routing"),
            (b'4', "Roster profiles"),
        ],
    )
    .await?
    else {
        return Ok(None);
    };
    match access {
        b'1' => accounts_setup(terminal, workspace, journal).await,
        b'2' => custom_routes_setup(terminal, state_dir, journal).await,
        b'3' => {
            routing::manage(terminal, state_dir, journal, current).await?;
            Ok(None)
        }
        b'4' => {
            roster::manage(terminal, state_dir, journal, current).await?;
            Ok(None)
        }
        _ => Err("invalid Provider setup choice".into()),
    }
}

async fn accounts_setup(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<Option<SetupSelection>, SetupError> {
    loop {
        let Some(action) = choose(
            terminal,
            1,
            "API accounts",
            "Credentials belong to this OS user",
            &[
                (b'1', "Add or replace native API account"),
                (b'2', "Select saved native API account"),
                (b'3', "Manage ChatGPT account"),
                (b'4', "Inspect saved native API account"),
                (b'5', "Remove saved access"),
                (b'6', "Back"),
            ],
        )
        .await?
        else {
            return Ok(None);
        };
        match action {
            b'1' => return native::native_setup(terminal, workspace, journal).await,
            b'2' => return native::select_native_account(terminal).await,
            b'3' => return chatgpt_setup(terminal, workspace, journal).await,
            b'4' => native::inspect_native_account(terminal).await?,
            b'5' => remove_saved_access(terminal, workspace, journal).await?,
            b'6' => return Ok(None),
            _ => return Err("invalid account action".into()),
        }
    }
}

async fn remove_saved_access(
    terminal: &mut AttachedTerminal,
    workspace: &Path,
    journal: &mut SetupJournal,
) -> Result<(), SetupError> {
    let Some(kind) = choose(
        terminal,
        1,
        "Remove saved access",
        "Local removal does not revoke a remote token",
        &[(b'1', "Native API account"), (b'2', "ChatGPT account")],
    )
    .await?
    else {
        return Ok(());
    };
    match kind {
        b'1' => native::remove_native_saved_access(terminal, workspace, journal).await?,
        b'2' => remove_chatgpt_saved_access(terminal, workspace, journal).await?,
        _ => return Err("invalid account removal choice".into()),
    }
    Ok(())
}

async fn custom_routes_setup(
    terminal: &mut AttachedTerminal,
    state_dir: &Path,
    journal: &mut SetupJournal,
) -> Result<Option<SetupSelection>, SetupError> {
    loop {
        let state = StateRoot::open_existing(state_dir)
            .or_else(|_| StateRoot::admit(state_dir))
            .map_err(|_| "state directory unavailable or unsafe".to_owned())?;
        let (expected, profiles) =
            CustomProfile::load_all_with_snapshot(&state).map_err(SetupError::recoverable)?;
        let statuses = CustomProfile::check_statuses(&state, &profiles)
            .await
            .map_err(SetupError::recoverable)?;
        let mut context = CustomRouteContext {
            state_dir,
            state: &state,
            expected: expected.as_deref(),
            profiles: &profiles,
            journal,
        };
        match custom_route_selection(terminal, &profiles, &statuses).await? {
            None => return Ok(None),
            Some(choice) => {
                match apply_custom_route_choice(terminal, &mut context, &statuses, choice).await? {
                    CustomRouteResult::Continue => {}
                    CustomRouteResult::Leave => return Ok(None),
                    CustomRouteResult::Select(selection) => return Ok(Some(selection)),
                }
            }
        }
    }
}

enum CustomRouteResult {
    Continue,
    Leave,
    Select(SetupSelection),
}

struct CustomRouteContext<'a> {
    state_dir: &'a Path,
    state: &'a StateRoot,
    expected: Option<&'a [u8]>,
    profiles: &'a [CustomProfile],
    journal: &'a mut SetupJournal,
}

async fn custom_route_selection(
    terminal: &mut AttachedTerminal,
    profiles: &[CustomProfile],
    statuses: &[(String, CustomCheckStatus)],
) -> Result<Option<u8>, SetupError> {
    let choices = custom_profile_choices(profiles, statuses);
    let rendered = choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    choose(
        terminal,
        1,
        "Custom routes",
        "Declarations are saved privately; Select and Check are separate actions",
        &rendered,
    )
    .await
}

async fn apply_custom_route_choice(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
    statuses: &[(String, CustomCheckStatus)],
    choice: u8,
) -> Result<CustomRouteResult, SetupError> {
    if choice == b'a' {
        add_custom_route(terminal, context).await?;
        return Ok(CustomRouteResult::Continue);
    }
    if choice == b'b' {
        return Ok(CustomRouteResult::Leave);
    }
    let index = usize::from(choice.saturating_sub(1));
    let profile = context
        .profiles
        .get(index)
        .ok_or_else(|| SetupError::Recoverable("invalid custom route selection".into()))?;
    let Some(action) = custom_route_action(terminal, profile).await? else {
        return Ok(CustomRouteResult::Continue);
    };
    match action {
        b'1' => {
            let status = statuses
                .iter()
                .find(|(name, _)| name == profile.name())
                .map(|(_, status)| *status);
            inspect_custom_route(terminal, profile, status).await?;
        }
        b'2' => return Ok(CustomRouteResult::Select(select_custom_profile(profile))),
        b'3' => {
            if check_custom_route(terminal, context.state, profile).await? {
                return Ok(CustomRouteResult::Leave);
            }
        }
        b'4' => edit_custom_route(terminal, context, index, profile).await?,
        b'5' => remove_custom_route(terminal, context, index, profile).await?,
        b'6' => {}
        _ => return Err("invalid custom route action".into()),
    }
    Ok(CustomRouteResult::Continue)
}

async fn custom_route_action(
    terminal: &mut AttachedTerminal,
    profile: &CustomProfile,
) -> Result<Option<u8>, SetupError> {
    choose(
        terminal,
        1,
        &format!("Custom route {}", escape_terminal(profile.name())),
        &format!(
            "Credential: {} — not checked by listing",
            escape_terminal(profile.credential_env())
        ),
        &[
            (b'1', "Inspect"),
            (b'2', "Select"),
            (b'3', "Check"),
            (b'4', "Edit"),
            (b'5', "Remove"),
            (b'6', "Back"),
        ],
    )
    .await
}

fn select_custom_profile(profile: &CustomProfile) -> SetupSelection {
    let effort = profile
        .efforts()
        .iter()
        .copied()
        .find(|effort| *effort == Effort::Low);
    SetupSelection::Custom {
        defaults: SessionDefaults {
            provider: Some(ProviderKind::custom_profile(profile.name())),
            model: Some(profile.model().to_owned()),
            effort,
            account_id: None,
            policy: CollaborationPolicy::default(),
        },
        notice: "Custom route selected; the next Run rechecks exact admission".into(),
    }
}

async fn edit_custom_route(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
    index: usize,
    profile: &CustomProfile,
) -> Result<(), SetupError> {
    let Some(draft) = custom_profile_form(terminal, Some(&profile.draft())).await? else {
        return Ok(());
    };
    if draft.name != profile.name() {
        await_setup_ack(terminal, "Edit custom route", "Route names cannot change in place; add the new route, then remove the old one after updating references").await?;
        return Ok(());
    }
    let mut drafts = context
        .profiles
        .iter()
        .map(CustomProfile::draft)
        .collect::<Vec<_>>();
    drafts[index] = draft.clone();
    CustomProfile::validate_drafts(&drafts)
        .map_err(|_| SetupError::Recoverable("custom route fields are invalid".into()))?;
    let summary = format!(
        "Name: {}\nDestination: {}\nModel: {}\nChanging this declaration clears its check evidence.",
        escape_terminal(&draft.name),
        escape_terminal(&draft.endpoint),
        escape_terminal(&draft.model),
    );
    if review_warning(
        terminal,
        "Save custom route?",
        &summary,
        "Save this declaration?",
    )
    .await?
    {
        write_custom_profiles(context, &drafts, &draft.name).await?;
        await_setup_ack(terminal, "Custom route", "Saved; check it again before use").await?;
    }
    Ok(())
}

async fn remove_custom_route(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
    index: usize,
    profile: &CustomProfile,
) -> Result<(), SetupError> {
    if custom_route_references(context.state_dir, profile.name())? {
        await_setup_ack(terminal, "Remove custom route", "This route is referenced by local routing or roster configuration; update those records first").await?;
        return Ok(());
    }
    let summary = format!(
        "Remove {} from this State directory. Its environment variable is unchanged; its check evidence will be cleared.",
        escape_terminal(profile.name()),
    );
    if review_warning(
        terminal,
        "Remove custom route?",
        &summary,
        "Remove this route?",
    )
    .await?
    {
        let mut drafts = context
            .profiles
            .iter()
            .map(CustomProfile::draft)
            .collect::<Vec<_>>();
        drafts.remove(index);
        write_custom_profiles(context, &drafts, profile.name()).await?;
        await_setup_ack(terminal, "Custom route", "Removed").await?;
    }
    Ok(())
}

async fn add_custom_route(
    terminal: &mut AttachedTerminal,
    context: &mut CustomRouteContext<'_>,
) -> Result<(), SetupError> {
    let Some(draft) = custom_profile_form(terminal, None).await? else {
        return Ok(());
    };
    if context
        .profiles
        .iter()
        .any(|profile| profile.name() == draft.name)
    {
        await_setup_ack(
            terminal,
            "Custom route",
            "A route with that name already exists",
        )
        .await?;
        return Ok(());
    }
    let mut drafts = context
        .profiles
        .iter()
        .map(CustomProfile::draft)
        .collect::<Vec<_>>();
    drafts.push(draft.clone());
    CustomProfile::validate_drafts(&drafts)
        .map_err(|_| SetupError::Recoverable("custom route fields are invalid".into()))?;
    let summary = format!(
        "Name: {}\nDestination: {}\nModel: {}\nCredential reference: {}\nSaving does not call the Provider or verify credentials.",
        escape_terminal(&draft.name),
        escape_terminal(&draft.endpoint),
        escape_terminal(&draft.model),
        escape_terminal(&draft.credential_env),
    );
    if review_warning(
        terminal,
        "Save custom route?",
        &summary,
        "Save this declaration?",
    )
    .await?
    {
        write_custom_profiles(context, &drafts, &draft.name).await?;
        await_setup_ack(
            terminal,
            "Custom route",
            "Saved; run Check before using this route",
        )
        .await?;
    }
    Ok(())
}

async fn write_custom_profiles(
    context: &mut CustomRouteContext<'_>,
    drafts: &[CustomProfileDraft],
    identity: &str,
) -> Result<(), SetupError> {
    let operation = context
        .journal
        .start(
            ProviderSetupTargetKind::CustomProfile,
            identity.to_owned(),
            context.expected.map(|bytes| Sha256::digest(bytes).into()),
        )
        .await?;
    match CustomProfile::write_profiles(context.state, context.expected, drafts).await {
        Ok(bytes) => {
            context
                .journal
                .finish(
                    operation,
                    Some(Sha256::digest(bytes).into()),
                    ProviderSetupDisposition::Applied,
                )
                .await
        }
        Err(error) => {
            let disposition = if matches!(error, CustomProfileError::PublicationUncertain) {
                ProviderSetupDisposition::Uncertain
            } else {
                ProviderSetupDisposition::Failed
            };
            context.journal.finish(operation, None, disposition).await?;
            Err(SetupError::custom_write(error))
        }
    }
}

fn custom_profile_choices(
    profiles: &[CustomProfile],
    statuses: &[(String, CustomCheckStatus)],
) -> Vec<(u8, String)> {
    let mut choices: Vec<_> = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let status = custom_status_label(statuses, profile.name());
            (
                u8::try_from(index + 1).unwrap_or(u8::MAX),
                format!("{} · {status}", escape_terminal(profile.name())),
            )
        })
        .collect();
    choices.push((b'a', "Add custom route".into()));
    choices.push((b'b', "Back".into()));
    choices
}

fn custom_status_label(statuses: &[(String, CustomCheckStatus)], name: &str) -> String {
    statuses
        .iter()
        .find(|(candidate, _)| candidate == name)
        .map_or("unchecked".to_owned(), |(_, status)| match status {
            CustomCheckStatus::Unchecked => "unchecked".into(),
            CustomCheckStatus::Stale => "stale".into(),
            CustomCheckStatus::VerifiedUntil(expires) => format_verified_until(*expires),
        })
}

fn format_verified_until(expires_at_ms: i64) -> String {
    let now = i64::try_from(arany::now_ms()).unwrap_or(i64::MAX);
    let remaining_ms = u64::try_from(expires_at_ms.saturating_sub(now)).unwrap_or(0);
    format!("verified; {} left", format_duration(remaining_ms))
}

async fn inspect_custom_route(
    terminal: &mut AttachedTerminal,
    profile: &CustomProfile,
    status: Option<CustomCheckStatus>,
) -> Result<(), SetupError> {
    let status = match status.unwrap_or(CustomCheckStatus::Unchecked) {
        CustomCheckStatus::Unchecked => "not checked".to_owned(),
        CustomCheckStatus::Stale => "stale; check again".to_owned(),
        CustomCheckStatus::VerifiedUntil(expires) => format_verified_until(expires),
    };
    let details = format!(
        "Protocol: {}\nDestination: {}\nModel: {}\nCredential: {} — not checked by listing\nCheck: {status}",
        match profile.draft().protocol {
            CustomProtocol::OpenaiResponses => "OpenAI Responses",
            CustomProtocol::AnthropicMessages => "Anthropic Messages",
            CustomProtocol::OpenaiChatCompletions => "OpenAI Chat Completions",
        },
        escape_terminal(&profile.check_destination()),
        escape_terminal(profile.model()),
        escape_terminal(profile.credential_env()),
    );
    await_setup_ack(terminal, "Custom route", &details).await?;
    Ok(())
}

async fn check_custom_route(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    profile: &CustomProfile,
) -> Result<bool, SetupError> {
    let (calls, duration) = profile.check_limits();
    let duration_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    if !review_warning(
        terminal,
        "Check custom route?",
        &format!(
            "Destination: {}\nModel: {}\nAt most {calls} synthetic calls in {}; the Provider may bill for them. No chat or Workspace content is sent.",
            escape_terminal(&profile.check_destination()),
            escape_terminal(profile.model()),
            format_duration(duration_ms),
        ),
        "Authorize this conformance check?",
    )
    .await?
    {
        return Ok(false);
    }
    let report = await_custom_check(terminal, state, profile).await?;
    let (status, uncertain) = custom_check_status(report, profile);
    await_setup_ack(terminal, "Custom route check", &status).await?;
    Ok(uncertain)
}

async fn await_custom_check(
    terminal: &mut AttachedTerminal,
    state: &StateRoot,
    profile: &CustomProfile,
) -> Result<Result<arany::CustomProfileCheck, CustomProfileError>, SetupError> {
    terminal.draw_setup(
        1,
        "Checking custom route",
        "Contacting the exact destination with synthetic data",
        0,
        None,
    )?;
    let cancellation = RunCancellation::new();
    let check = arany::check_custom_profile_value_with_cancellation(
        StateRoot::open_existing(state.path())
            .map_err(|_| "state directory unavailable or unsafe")?,
        profile.clone(),
        cancellation.clone(),
    );
    tokio::pin!(check);
    let report = loop {
        tokio::select! {
            result = &mut check => break result,
            input = terminal.next_input() => match input {
                Ok(TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput) => {
                    cancellation.cancel();
                    break check.await;
                }
                Ok(TerminalInput::Shutdown(signal)) => {
                    cancellation.cancel();
                    let _ = check.await;
                    return Err(SetupError::Shutdown(signal));
                }
                Ok(_) => {}
                Err(error) => {
                    cancellation.cancel();
                    let _ = check.await;
                    return Err(SetupError::from(error));
                }
            }
        }
    };
    Ok(report)
}

fn custom_check_status(
    report: Result<arany::CustomProfileCheck, CustomProfileError>,
    profile: &CustomProfile,
) -> (String, bool) {
    let uncertain = matches!(
        &report,
        Err(CustomProfileError::Check { receipt, .. })
            if receipt.disposition.is_none_or(|value| value == arany::CheckDisposition::Uncertain)
    ) || matches!(
        &report,
        Err(CustomProfileError::Journal(error)) if error.receipt.is_some()
    );
    let status = match report {
        Ok(report) => format!(
            "Check: {}\nDisposition: {}\nStatus: {}",
            report.view().id,
            report.view().replay_disposition(),
            format_verified_until(report.expires_at_ms()),
        ),
        Err(CustomProfileError::Check { source, receipt }) => {
            let reason = if matches!(*source, CustomProfileError::CredentialNotSet) {
                format!(
                    "Credential: {} — not set\nExport it in the launching shell, then restart Arany",
                    escape_terminal(profile.credential_env())
                )
            } else {
                format!(
                    "Check could not be completed: {}",
                    safe_custom_check_error(&source)
                )
            };
            format!(
                "{reason}\nCheck: {}\nDisposition: {}",
                receipt.id,
                receipt
                    .disposition
                    .map_or("unconfirmed".into(), |value| value.to_string())
            )
        }
        Err(error) => format!("Check unavailable: {}", safe_custom_check_error(&error)),
    };
    (status, uncertain)
}

async fn await_setup_ack(
    terminal: &mut AttachedTerminal,
    title: &str,
    message: &str,
) -> Result<bool, SetupError> {
    let pages = setup_message_pages(message);
    let mut page = 0usize;
    let mut notice = None;
    loop {
        let text = format!("Page {}/{}\n{}", page + 1, pages.len(), pages[page]);
        terminal.draw_setup(0, title, &text, 0, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit if page + 1 < pages.len() => page += 1,
            TerminalInput::Down | TerminalInput::PageDown if page + 1 < pages.len() => page += 1,
            TerminalInput::Up | TerminalInput::PageUp => page = page.saturating_sub(1),
            TerminalInput::Submit => return Ok(true),
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                return Ok(false);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            TerminalInput::Resize => {
                page = 0;
                notice = Some("Terminal resized; reread all pages");
            }
            _ => {}
        }
    }
}

fn setup_message_pages(message: &str) -> Vec<String> {
    let pages: Vec<_> = message
        .lines()
        .flat_map(|line| {
            let chars: Vec<_> = line.chars().collect();
            if chars.is_empty() {
                return vec![String::new()];
            }
            chars
                .chunks(SETUP_MESSAGE_PAGE_CHARS)
                .map(|chunk| chunk.iter().collect())
                .collect()
        })
        .collect();
    if pages.is_empty() {
        vec![String::new()]
    } else {
        pages
    }
}

fn safe_custom_check_error(error: &CustomProfileError) -> &'static str {
    match error {
        CustomProfileError::CredentialNotSet => "selected credential is not set",
        CustomProfileError::CredentialUnavailable => {
            "selected credential is unavailable or invalid"
        }
        CustomProfileError::CheckBusy => "another Provider check is active",
        CustomProfileError::PublicationUncertain => {
            "record publication is uncertain; inspect the saved route before continuing"
        }
        CustomProfileError::Changed => {
            "route changed during the operation; inspect it before continuing"
        }
        CustomProfileError::ConformanceFailed => "synthetic conformance failed",
        CustomProfileError::Journal(_) | CustomProfileError::EvidenceUnavailable => {
            "check result could not be confirmed"
        }
        _ => "destination or route was refused",
    }
}

fn custom_route_references(state_dir: &Path, name: &str) -> Result<bool, SetupError> {
    Ok(
        crate::cli::provider::routing::references_custom_profile(state_dir, name)
            .map_err(SetupError::recoverable)?
            || crate::cli::provider::roster::references_custom_profile(state_dir, name)
                .map_err(SetupError::recoverable)?,
    )
}

async fn custom_profile_form(
    terminal: &mut AttachedTerminal,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<CustomProfileDraft>, SetupError> {
    let Some(protocol) = choose_custom_protocol(terminal, initial).await? else {
        return Ok(None);
    };
    let Some((name, endpoint, model, credential_env)) =
        read_custom_profile_fields(terminal, protocol, initial).await?
    else {
        return Ok(None);
    };
    let Some(efforts) = read_custom_efforts(terminal, initial).await? else {
        return Ok(None);
    };
    let messages_auth = if protocol == CustomProtocol::AnthropicMessages {
        let Some(choice) = choose_focused(
            terminal,
            1,
            "Messages authentication",
            "Choose the one declared credential header",
            &[(b'1', "Bearer"), (b'2', "x-api-key")],
            initial.map(|draft| match draft.messages_auth {
                MessagesAuth::Bearer => b'1',
                MessagesAuth::ApiKey => b'2',
            }),
        )
        .await?
        else {
            return Ok(None);
        };
        if choice == b'1' {
            MessagesAuth::Bearer
        } else {
            MessagesAuth::ApiKey
        }
    } else if protocol == CustomProtocol::OpenaiChatCompletions {
        MessagesAuth::Bearer
    } else {
        MessagesAuth::ApiKey
    };
    Ok(Some(CustomProfileDraft {
        name,
        protocol,
        endpoint,
        model,
        credential_env,
        messages_auth,
        efforts,
    }))
}

async fn choose_custom_protocol(
    terminal: &mut AttachedTerminal,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<CustomProtocol>, SetupError> {
    let selected = choose_focused(
        terminal,
        1,
        "Custom route protocol",
        "Choose one closed request format",
        &[
            (b'1', "OpenAI Responses"),
            (b'2', "Anthropic Messages"),
            (b'3', "OpenAI Chat Completions"),
        ],
        initial.map(|draft| match draft.protocol {
            CustomProtocol::OpenaiResponses => b'1',
            CustomProtocol::AnthropicMessages => b'2',
            CustomProtocol::OpenaiChatCompletions => b'3',
        }),
    )
    .await?;
    Ok(selected.map(|choice| {
        if choice == b'1' {
            CustomProtocol::OpenaiResponses
        } else if choice == b'2' {
            CustomProtocol::AnthropicMessages
        } else {
            CustomProtocol::OpenaiChatCompletions
        }
    }))
}

async fn read_custom_profile_fields(
    terminal: &mut AttachedTerminal,
    protocol: CustomProtocol,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<(String, String, String, String)>, SetupError> {
    let Some(name) = read_setup_value(
        terminal,
        "Custom route name",
        &format!("Route name; at most {MAX_CUSTOM_PROFILE_NAME_BYTES} bytes"),
        initial.map_or("", |draft| &draft.name),
        MAX_CUSTOM_PROFILE_NAME_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let endpoint_prompt = match protocol {
        CustomProtocol::OpenaiResponses => {
            "Exact canonical /responses URL; HTTPS, or loopback HTTP for local fixtures"
        }
        CustomProtocol::AnthropicMessages => {
            "Canonical HTTPS base URL, or loopback HTTP; Arany adds /v1/messages"
        }
        CustomProtocol::OpenaiChatCompletions => {
            "Canonical HTTPS base URL, or loopback HTTP; Arany adds /chat/completions"
        }
    };
    let Some(endpoint) = read_setup_value(
        terminal,
        "Custom route destination",
        endpoint_prompt,
        initial
            .filter(|draft| draft.protocol == protocol)
            .map_or("", |draft| &draft.endpoint),
        MAX_CUSTOM_PROFILE_ENDPOINT_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(model) = read_setup_value(
        terminal,
        "Custom route model",
        &format!("Model identifier; at most {MAX_CUSTOM_PROFILE_MODEL_BYTES} bytes"),
        initial.map_or("", |draft| &draft.model),
        MAX_CUSTOM_PROFILE_MODEL_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(credential_env) = read_setup_value(
        terminal,
        "Credential environment name",
        &format!(
            "Credential reference only; at most {MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES} bytes"
        ),
        initial.map_or("", |draft| &draft.credential_env),
        MAX_CUSTOM_PROFILE_CREDENTIAL_ENV_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some((name, endpoint, model, credential_env)))
}

async fn read_custom_efforts(
    terminal: &mut AttachedTerminal,
    initial: Option<&CustomProfileDraft>,
) -> Result<Option<Vec<Effort>>, SetupError> {
    let initial = initial.map_or_else(String::new, |draft| {
        draft
            .efforts
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    });
    let Some(value) = read_optional_setup_value(
        terminal,
        "Declared efforts",
        "Comma-separated effort names, or empty for no effort override",
        &initial,
        MAX_CUSTOM_PROFILE_EFFORTS_BYTES,
    )
    .await?
    else {
        return Ok(None);
    };
    parse_efforts(&value)
        .map(Some)
        .ok_or_else(|| SetupError::Recoverable("invalid declared effort list".into()))
}

fn parse_efforts(value: &str) -> Option<Vec<Effort>> {
    if value.trim().is_empty() {
        return Some(Vec::new());
    }
    let requested: Vec<_> = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<Effort>)
        .collect::<Result<_, _>>()
        .ok()?;
    if requested.len() > Effort::ALL.len() {
        return None;
    }
    let efforts: Vec<_> = Effort::ALL
        .iter()
        .copied()
        .filter(|effort| requested.contains(effort))
        .collect();
    (efforts.len() == requested.len()).then_some(efforts)
}

async fn read_setup_value(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    initial: &str,
    limit: usize,
) -> Result<Option<String>, SetupError> {
    read_setup_value_inner(
        terminal,
        SetupValueInput {
            title,
            instruction,
            initial,
            limit,
            allow_empty: false,
        },
    )
    .await
}

async fn read_optional_setup_value(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
    initial: &str,
    limit: usize,
) -> Result<Option<String>, SetupError> {
    read_setup_value_inner(
        terminal,
        SetupValueInput {
            title,
            instruction,
            initial,
            limit,
            allow_empty: true,
        },
    )
    .await
}

struct SetupValueInput<'a> {
    title: &'a str,
    instruction: &'a str,
    initial: &'a str,
    limit: usize,
    allow_empty: bool,
}

async fn read_setup_value_inner(
    terminal: &mut AttachedTerminal,
    input: SetupValueInput<'_>,
) -> Result<Option<String>, SetupError> {
    let SetupValueInput {
        title,
        instruction,
        initial,
        limit,
        allow_empty,
    } = input;
    if !initial.is_empty()
        && !initial.starts_with("re-enter ")
        && !await_setup_ack(
            terminal,
            title,
            &format!(
                "Current value: {initial}\nEnter a replacement; empty Enter {}.",
                if allow_empty { "clears it" } else { "keeps it" }
            ),
        )
        .await?
    {
        return Ok(None);
    }
    let mut composer = Composer::default();
    let prompt = if allow_empty {
        format!("{instruction}; empty Enter clears this field")
    } else if !initial.is_empty() && !initial.starts_with("re-enter ") {
        format!("{instruction}; Enter replaces it, empty Enter keeps it")
    } else {
        instruction.to_owned()
    };
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_text_input(title, &prompt, &composer, notice.take())?;
        let input = terminal.next_input().await?;
        match input {
            TerminalInput::Character(character) if !character.is_control() => {
                composer.apply(TerminalInput::Character(character));
            }
            TerminalInput::Paste => match terminal.take_paste() {
                Ok(text)
                    if !text.chars().any(char::is_control)
                        && composer.text().len().saturating_add(text.len()) <= limit =>
                {
                    composer
                        .insert_paste(&text)
                        .map_err(|_| "setup field is too long")?;
                }
                _ => notice = Some("Field paste is invalid or over the size limit"),
            },
            TerminalInput::Submit if !composer.text().trim().is_empty() => {
                let value = composer.text().trim().to_owned();
                if value.len() <= limit && !value.chars().any(char::is_control) {
                    return Ok(Some(value));
                }
                notice = Some("Field is invalid or over the size limit");
            }
            TerminalInput::Submit if allow_empty => return Ok(Some(String::new())),
            TerminalInput::Submit if !initial.is_empty() => return Ok(Some(initial.to_owned())),
            TerminalInput::Submit => notice = Some("This field cannot be empty"),
            TerminalInput::Backspace
            | TerminalInput::Left
            | TerminalInput::Right
            | TerminalInput::Delete => {
                composer.apply(input);
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Suspend => terminal.suspend_and_resume(
                crate::terminal::MAX_DRAFT_BYTES.saturating_sub(composer.text().len()),
            )?,
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
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

async fn settle_chatgpt_authorization(
    journal: &SetupJournal,
    operation: uuid::Uuid,
    result: Result<Option<ChatGptSetupAccount>, SetupError>,
) -> Result<Option<ChatGptSetupAccount>, SetupError> {
    let (digest, disposition) = match &result {
        Ok(Some(ChatGptSetupAccount::PermissionDisabled {
            local_cleared: false,
            ..
        }))
        | Err(SetupError::Uncertain(_)) => (None, ProviderSetupDisposition::Uncertain),
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
    terminal: &mut AttachedTerminal,
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
        begin_chatgpt_authorization(terminal, workspace, target.as_ref(), acknowledgment).await?
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
    credentials: chatgpt::VerifiedSignIn,
    acknowledgment: RiskAcknowledgment,
) -> Result<ChatGptSetupAccount, SetupError> {
    match credentials {
        chatgpt::VerifiedSignIn::PlanEnabled(credentials) => chatgpt::save_verified(
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
        }),
        chatgpt::VerifiedSignIn::PlanDisabled(identity) => chatgpt::save_without_plan_permission(
            workspace.to_path_buf(),
            identity,
            acknowledgment,
            expected,
        )
        .await
        .map(|outcome| ChatGptSetupAccount::PermissionDisabled {
            id: outcome.id,
            local_cleared: outcome.local_cleared,
        }),
    }
    .map_err(SetupError::chatgpt_authorization)
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
            context_window_tokens: None,
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

pub(super) async fn review_warning(
    terminal: &mut AttachedTerminal,
    title: &str,
    warning: &str,
    instruction: &str,
) -> Result<bool, SetupError> {
    if terminal.is_linear() {
        return review_linear_warning(terminal, title, warning, instruction).await;
    }
    let mut page = 0usize;
    let mut accept_selected = false;
    let mut notice = None::<&str>;
    loop {
        let has_more =
            terminal.draw_setup_warning(title, warning, page, accept_selected, notice.take())?;
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
            TerminalInput::Up if !accept_selected && page > 0 => page -= 1,
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
                return Ok(true);
            }
            TerminalInput::Submit => {
                terminal.discard_draft_input();
                return Ok(false);
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
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                page = 0;
                accept_selected = false;
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(false);
            }
            TerminalInput::Shutdown(signal) => {
                return Err(SetupError::Shutdown(signal));
            }
            _ => {}
        }
    }
}

async fn review_linear_warning(
    terminal: &mut AttachedTerminal,
    title: &str,
    warning: &str,
    instruction: &str,
) -> Result<bool, SetupError> {
    let pages = setup_message_pages(warning);
    let mut page = 0usize;
    let mut notice = None;
    loop {
        let text = format!("Page {}/{}\n{}", page + 1, pages.len(), pages[page]);
        terminal.draw_setup(0, title, &text, 0, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit if page + 1 < pages.len() => page += 1,
            TerminalInput::Down | TerminalInput::PageDown if page + 1 < pages.len() => page += 1,
            TerminalInput::Submit => {
                if let Some(accepted) =
                    review_linear_confirmation(terminal, title, instruction).await?
                {
                    return Ok(accepted);
                }
                page = 0;
            }
            TerminalInput::Up | TerminalInput::PageUp => page = page.saturating_sub(1),
            TerminalInput::Character(_) => {
                notice = Some("Read every page before accepting");
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(false);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            TerminalInput::Resize => {
                page = 0;
                notice = Some("Resize: reread");
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                page = 0;
            }
            _ => {}
        }
    }
}

async fn review_linear_confirmation(
    terminal: &mut AttachedTerminal,
    title: &str,
    instruction: &str,
) -> Result<Option<bool>, SetupError> {
    let choices = [(b'1', "Back"), (b'2', "Accept")];
    let linear_choices = linear_rendered_choices(&choices);
    let rendered = linear_choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let mut input = String::new();
    let mut rejected = 0u16;
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(2, title, instruction, &rendered, 0, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Submit => {
                let accepted = if rejected == 0 && input.trim().is_empty() {
                    Some(choices[0].0)
                } else if rejected == 0 {
                    linear_choice_value(input.trim(), &choices)
                } else {
                    None
                };
                terminal.discard_draft_input();
                if let Some(value) = accepted {
                    return Ok(Some(value == b'2'));
                }
                input.clear();
                rejected = 0;
                notice = Some("Type a listed choice name; Ctrl+C cancels");
            }
            TerminalInput::Character(character) => {
                if !append_linear_choice_character(&mut input, &mut rejected, character) {
                    notice = Some("Invalid choice input; use Backspace to correct");
                }
            }
            TerminalInput::Backspace => {
                if rejected > 0 {
                    rejected -= 1;
                } else {
                    input.pop();
                }
            }
            TerminalInput::LineRejected | TerminalInput::LineContinued => {
                input.clear();
                rejected = 0;
                terminal.discard_draft_input();
                notice = Some("Invalid choice line; Ctrl+C cancels");
            }
            TerminalInput::Resize => {
                terminal.discard_approval_typeahead().await?;
                return Ok(None);
            }
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
                terminal.discard_approval_typeahead().await?;
                return Ok(None);
            }
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(Some(false));
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
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
        .map_err(SetupError::chatgpt_authorization)
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
                TerminalInput::Suspend => terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?,
                TerminalInput::LineRejected | TerminalInput::LineContinued => terminal.discard_draft_input(),
                _ => {}
            },
        }
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
    if terminal.is_linear() {
        let linear_choices = linear_rendered_choices(choices);
        let rendered = linear_choices
            .iter()
            .map(|(value, label)| (*value, label.as_str()))
            .collect::<Vec<_>>();
        let listing = linear_choices
            .iter()
            .map(|(_, label)| label.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if crate::terminal::setup_choices_need_paging(instruction, &rendered)
            && !await_setup_ack(terminal, title, &format!("{instruction}\n{listing}")).await?
        {
            return Ok(None);
        }
    }
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
    let selected = choices
        .iter()
        .position(|(value, _)| Some(*value) == current)
        .unwrap_or(0);
    if terminal.is_linear() {
        choose_linear(terminal, step, title, instruction, choices, selected).await
    } else {
        choose_inline(terminal, step, title, instruction, choices, selected).await
    }
}

async fn choose_linear(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    mut selected: usize,
) -> Result<Option<u8>, SetupError> {
    let linear_choices = linear_rendered_choices(choices);
    let linear_rendered = linear_choices
        .iter()
        .map(|(value, label)| (*value, label.as_str()))
        .collect::<Vec<_>>();
    let mut input = String::new();
    let mut rejected = 0u16;
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(
            step,
            title,
            instruction,
            &linear_rendered,
            selected,
            notice.take(),
        )?;
        match terminal.next_input().await? {
            TerminalInput::Character(character) => {
                if !append_linear_choice_character(&mut input, &mut rejected, character) {
                    notice = Some("Invalid choice input; use Backspace to correct");
                }
            }
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
                if rejected == 0 && input.trim().is_empty() {
                    return Ok(Some(choices[selected].0));
                }
                if rejected == 0
                    && let Some(value) = linear_choice_value(input.trim(), choices)
                {
                    return Ok(Some(value));
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
            TerminalInput::Suspend => terminal
                .suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES.saturating_sub(input.len()))?,
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            _ => {}
        }
    }
}

fn append_linear_choice_character(input: &mut String, rejected: &mut u16, character: char) -> bool {
    if *rejected == 0
        && (character.is_ascii_graphic() || character == ' ')
        && input.len() < MAX_SETUP_CHOICE_INPUT_BYTES
    {
        input.push(character);
        true
    } else {
        *rejected = rejected.saturating_add(1);
        false
    }
}

fn linear_rendered_choices(choices: &[(u8, &str)]) -> Vec<(u8, String)> {
    choices
        .iter()
        .enumerate()
        .map(|(index, (value, label))| (*value, format!("{}. {label}", index.saturating_add(1))))
        .collect()
}

fn linear_choice_value(input: &str, choices: &[(u8, &str)]) -> Option<u8> {
    if let Ok(index) = input.parse::<usize>()
        && let Some((value, _)) = index.checked_sub(1).and_then(|index| choices.get(index))
    {
        return Some(*value);
    }
    choices
        .iter()
        .find(|(_, label)| label.eq_ignore_ascii_case(input))
        .map(|(value, _)| *value)
}

async fn choose_inline(
    terminal: &mut AttachedTerminal,
    step: u8,
    title: &str,
    instruction: &str,
    choices: &[(u8, &str)],
    mut selected: usize,
) -> Result<Option<u8>, SetupError> {
    let mut notice = None::<&str>;
    loop {
        terminal.draw_setup_choices(step, title, instruction, choices, selected, notice.take())?;
        match terminal.next_input().await? {
            TerminalInput::Character(_) => notice = Some("Use Up/Down and Enter to select"),
            TerminalInput::Up => selected = selected.saturating_sub(1),
            TerminalInput::Down | TerminalInput::Tab => {
                selected = (selected + 1).min(choices.len() - 1);
            }
            TerminalInput::Home => selected = 0,
            TerminalInput::End => selected = choices.len() - 1,
            TerminalInput::Submit => return Ok(Some(choices[selected].0)),
            TerminalInput::Escape | TerminalInput::Interrupt | TerminalInput::EndOfInput => {
                terminal.discard_draft_input();
                return Ok(None);
            }
            TerminalInput::Shutdown(signal) => return Err(SetupError::Shutdown(signal)),
            TerminalInput::Suspend => {
                terminal.suspend_and_resume(crate::terminal::MAX_DRAFT_BYTES)?;
            }
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
pub(super) mod tests {
    use super::*;
    use crate::accounts::credentials::CredentialError;

    pub(crate) enum SetupPublication {
        NativeSave,
        NativeRemoval,
        ChatGpt,
    }

    pub(crate) async fn assert_uncertain_publication(
        publication: SetupPublication,
        publish: impl FnOnce() -> chatgpt::AuthorizationError,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let path = temp.path().join("state");
        let mut journal = SetupJournal::new(&path, &workspace, None);
        let target = match publication {
            SetupPublication::NativeSave | SetupPublication::NativeRemoval => {
                ProviderSetupTargetKind::NativeAccount
            }
            SetupPublication::ChatGpt => ProviderSetupTargetKind::ChatGptAccount,
        };
        let identity = if target == ProviderSetupTargetKind::NativeAccount {
            "native"
        } else {
            "new-chatgpt-account"
        };
        let operation = journal
            .start(target, identity.into(), None)
            .await
            .unwrap_or_else(|_| panic!("setup intent"));
        let error = publish();
        if target == ProviderSetupTargetKind::NativeAccount {
            let chatgpt::AuthorizationError::CredentialStore(error) = error else {
                panic!("native credential failure required");
            };
            let result = match publication {
                SetupPublication::NativeSave => {
                    native::settle_native_save(
                        &journal,
                        operation,
                        uuid::Uuid::now_v7(),
                        Err(error),
                    )
                    .await
                }
                SetupPublication::NativeRemoval => {
                    native::settle_native_removal(&journal, operation, Err(error)).await
                }
                SetupPublication::ChatGpt => unreachable!("native target"),
            };
            let expected = match publication {
                SetupPublication::NativeSave => {
                    "Native account storage is uncertain; restart setup and inspect saved access before continuing"
                }
                SetupPublication::NativeRemoval => {
                    "Native account removal is uncertain; restart setup and inspect saved access before continuing."
                }
                SetupPublication::ChatGpt => unreachable!("native target"),
            };
            assert!(
                matches!(result, Err(SetupError::Presentation(message)) if message == expected)
            );
        } else {
            let result = settle_chatgpt_authorization(
                &journal,
                operation,
                Err(SetupError::chatgpt_authorization(error)),
            )
            .await;
            assert!(matches!(result, Err(SetupError::Uncertain(_))));
        }
        let id = journal.session_id().unwrap();
        let store = Store::open(StateRoot::open_existing(&path).unwrap()).unwrap();
        let events = store.load_session(id).await.unwrap();
        assert!(matches!(&events.last().unwrap().event,
            Event::ProviderSetupEditFinished {
                operation_id, disposition: ProviderSetupDisposition::Uncertain, ..
            } if *operation_id == operation));
        let view = store.load_view(id).await.unwrap().unwrap();
        assert!(view.provider_setup_blocks_continuation());
        assert!(view.pending_provider_setup_edit.is_none());
        store.close().await.unwrap();
        assert!(
            arany::resume_session(
                StateRoot::open_existing(&path).unwrap(),
                workspace.clone(),
                id,
            )
            .await
            .is_err()
        );
        assert!(
            arany::set_session_defaults(
                StateRoot::open_existing(&path).unwrap(),
                workspace,
                id,
                SessionDefaults::default(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn authorization_exits_settle_and_reopen_without_pending_edits() {
        for (result, disposition) in [
            (
                Err(SetupError::from("system browser launch failed")),
                ProviderSetupDisposition::Failed,
            ),
            (Ok(None), ProviderSetupDisposition::Cancelled),
            (
                Err(SetupError::chatgpt_authorization(
                    chatgpt::AuthorizationError::CredentialStore(
                        CredentialError::WriteOutcomeUnknown,
                    ),
                )),
                ProviderSetupDisposition::Uncertain,
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let path = temp.path().join("state");
            let mut journal = SetupJournal::new(&path, &workspace, None);
            let operation = journal
                .start(
                    ProviderSetupTargetKind::ChatGptAccount,
                    "new-chatgpt-account".into(),
                    None,
                )
                .await
                .unwrap_or_else(|_| panic!("setup intent"));
            let _result = settle_chatgpt_authorization(&journal, operation, result).await;
            let id = journal.session_id().unwrap();
            let admissions = arany::test_support::admissions::recorded(id);
            assert!(matches!(
                admissions.as_slice(),
                [(
                    1,
                    2,
                    arany::test_support::admissions::AdmissionKind::Operation,
                    0
                )]
            ));
            let store = Store::open(StateRoot::open_existing(&path).unwrap()).unwrap();
            let events = store.load_session(id).await.unwrap();
            assert!(matches!(&events.last().unwrap().event,
                Event::ProviderSetupEditFinished { operation_id, disposition: actual, .. }
                    if *operation_id == operation && *actual == disposition));
            let view = store.load_view(id).await.unwrap().unwrap();
            assert_eq!(view.pending_provider_setup_edit, None);
            assert_eq!(
                view.provider_setup_uncertain,
                disposition == ProviderSetupDisposition::Uncertain
            );
            store.close().await.unwrap();
            let resumed = arany::resume_session(
                StateRoot::open_existing(&path).unwrap(),
                workspace.clone(),
                id,
            )
            .await;
            assert_eq!(
                resumed.is_err(),
                disposition == ProviderSetupDisposition::Uncertain
            );
            let continuation = arany::set_session_defaults(
                StateRoot::open_existing(&path).unwrap(),
                workspace,
                id,
                SessionDefaults::default(),
            )
            .await;
            assert_eq!(
                continuation.is_err(),
                disposition == ProviderSetupDisposition::Uncertain
            );
        }
    }

    #[test]
    fn setup_pages_show_complete_content_at_bounded_width() {
        let message = format!("Destination: {}\nDisposition: Uncertain", "x".repeat(160));
        let pages = setup_message_pages(&message);
        assert!(
            pages
                .iter()
                .all(|page| page.chars().count() <= SETUP_MESSAGE_PAGE_CHARS)
        );
        assert_eq!(pages.concat(), message.replace('\n', ""));
    }

    #[test]
    fn empty_efforts_create_and_clear_the_no_override_state() {
        assert_eq!(parse_efforts(""), Some(Vec::new()));
        assert_eq!(
            parse_efforts("low,high"),
            Some(vec![Effort::Low, Effort::High])
        );
        assert_eq!(parse_efforts("unknown"), None);
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
