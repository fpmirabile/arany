#[path = "setup/chatgpt.rs"]
mod chatgpt_flow;
mod custom;
mod error;
mod native;
mod roster;
mod routes;
mod routing;

use chatgpt_flow::{chatgpt_setup, remove_chatgpt_saved_access, selected_chatgpt_defaults};
pub(super) use error::SetupError;

use crate::accounts::chatgpt;
use crate::accounts::credentials;
use crate::terminal::{AttachedTerminal, Composer, TerminalInput};
#[cfg(all(test, unix, debug_assertions))]
use arany::{CollaborationPolicy, RecordKind};
use arany::{
    Event, HistoryRecovery, MAX_ANTHROPIC_WORKSPACE_ID_BYTES, MAX_NATIVE_API_KEY_BYTES, ModelEntry,
    NativeApiCredentials, ProviderKind, ProviderSetupDisposition, ProviderSetupTarget,
    ProviderSetupTargetKind, SessionDefaults, SessionId, StateRoot, Store, StoreError,
    create_session, validate_native_api_key,
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
        b'2' => routes::custom_routes_setup(terminal, state_dir, journal).await,
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
    use super::chatgpt_flow::settle_chatgpt_authorization;
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
}
