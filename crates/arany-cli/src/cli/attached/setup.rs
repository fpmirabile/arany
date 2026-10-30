#[path = "setup/chatgpt.rs"]
mod chatgpt_flow;
mod custom;
mod error;
mod native;
mod prompts;
mod roster;
mod routes;
mod routing;

use chatgpt_flow::{chatgpt_setup, remove_chatgpt_saved_access, selected_chatgpt_defaults};
pub(super) use error::SetupError;
pub(super) use prompts::{await_operation, choose_focused, read_setup_value, review_warning};
use prompts::{await_setup_ack, choose};

use crate::accounts::chatgpt;
use crate::accounts::credentials;
use crate::terminal::AttachedTerminal;
#[cfg(all(test, unix, debug_assertions))]
use arany::{CollaborationPolicy, RecordKind};
use arany::{
    Event, HistoryRecovery, ModelEntry, ProviderKind, ProviderSetupDisposition,
    ProviderSetupTarget, ProviderSetupTargetKind, SessionDefaults, SessionId, StateRoot, Store,
    StoreError, create_session,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const AUTHORIZE_TITLE: &str = "Authorize saved access";
const AUTHORIZE_PROMPT: &str =
    "Approve this Arany credential in your OS password store; Esc or Ctrl+C exits";

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
        let root = match arany::StateRoot::open_optional(state_dir) {
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
}
