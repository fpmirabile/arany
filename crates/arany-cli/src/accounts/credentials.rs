use arany::{
    Effort, NativeApiCredentials, ProviderRecord, StateRoot, StoreError, resolve_native_effort,
    validate_native_model_id,
};
use arany::{RecordFormat, RecordKind};
#[cfg(all(test, not(target_os = "macos")))]
use keyring::Entry;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::path::Path;
use uuid::{Uuid, Version};

mod keyring_helper;
#[cfg(test)]
pub(crate) use keyring_helper::TEST_HELPER;

#[cfg(all(test, target_os = "macos", feature = "test-support"))]
mod macos_tests;

pub(crate) async fn probe_keyring() -> Result<(), CredentialError> {
    tokio::task::spawn_blocking(keyring_helper::probe_backend)
        .await
        .map_err(|_| CredentialError::Unavailable)?
}

pub(crate) async fn authorize_keyring_slot(
    slot: String,
    expected: Option<(Uuid, String)>,
) -> Result<bool, CredentialError> {
    keyring_helper::authorize(slot, expected).await
}

pub(crate) async fn selected_keyring_slot(
    id: Uuid,
    provider: &str,
) -> Result<Option<String>, CredentialError> {
    let stored = tokio::task::spawn_blocking(load_file)
        .await
        .map_err(|_| CredentialError::StateUnavailable)??
        .ok_or(CredentialError::InvalidAccount)?;
    match stored.storage {
        AccountStorage::Keyring => Ok(Some(DEFAULT_ACCOUNT.into())),
        AccountStorage::PrivateFile => {
            let account = stored.account.ok_or(CredentialError::InvalidAccount)?;
            selected_credentials(&account, id, provider)?;
            Ok(None)
        }
    }
}

pub(crate) fn read_chatgpt_keyring_record(slot: &str) -> Result<Option<Vec<u8>>, CredentialError> {
    keyring_helper::read(slot)
}

pub(crate) fn write_chatgpt_keyring_record(
    slot: &str,
    record: &str,
) -> Result<(), CredentialError> {
    keyring_helper::write(slot, record)
}

pub(crate) fn delete_chatgpt_keyring_record(slot: &str) -> Result<(), CredentialError> {
    keyring_helper::delete(slot)
}

pub(super) const SERVICE: &str = "io.github.fpmirabile.arany";
const DEFAULT_ACCOUNT: &str = "default-native-api-account";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountStorage {
    Keyring,
    PrivateFile,
}

pub(crate) struct InspectedAccount {
    pub account: Option<SavedAccount>,
    pub storage: AccountStorage,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAccountFile {
    storage: AccountStorage,
    #[serde(deserialize_with = "Option::deserialize")]
    account: Option<SavedAccount>,
}

pub(crate) const SAVED_ACCOUNT: RecordFormat = RecordFormat {
    kind: RecordKind::SavedAccount,
    accepts: |bytes| StoredAccountFile::parse(bytes).is_ok(),
};

impl StoredAccountFile {
    fn parse(record: &[u8]) -> Result<Self, CredentialError> {
        let stored: Self =
            serde_json::from_slice(record).map_err(|_| CredentialError::InvalidAccount)?;
        if matches!(
            (&stored.storage, &stored.account),
            (AccountStorage::Keyring, Some(_)) | (AccountStorage::PrivateFile, None)
        ) {
            return Err(CredentialError::InvalidAccount);
        }
        if let Some(account) = &stored.account {
            account.validate()?;
        }
        Ok(stored)
    }

    fn encoded(&self) -> Result<Vec<u8>, CredentialError> {
        let record = serde_json::to_vec(self).map_err(|_| CredentialError::InvalidAccount)?;
        if record.len() > RecordKind::SavedAccount.limit() {
            return Err(CredentialError::InvalidAccount);
        }
        Ok(record)
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub(crate) enum CredentialError {
    #[error("invalid saved account; use arany --setup to choose or reset access")]
    InvalidAccount,
    #[error("OS credential store unavailable")]
    Unavailable,
    #[error("OS credential store did not respond before its deadline")]
    TimedOut,
    #[error(
        "saved credential write could not be confirmed; the saved account may have changed. Inspect it before retrying"
    )]
    WriteOutcomeUnknown,
    #[error(
        "saved credential deletion could not be confirmed; the credential may remain stored. Inspect it before retrying"
    )]
    DeleteOutcomeUnknown,
    #[error("OS credential store is locked; unlock it before using this account")]
    Locked,
    #[error("saved account keyring unavailable; restore it before replacing the account")]
    PinnedStoreUnavailable,
    #[cfg(target_os = "linux")]
    #[error("unsafe or unsupported D-Bus session address for the OS credential store")]
    UnsafeTransport,
    #[error("another process is replacing the saved account")]
    AccountBusy,
    #[error("private account state unavailable")]
    StateUnavailable,
    #[error("saved setup changed while reset was being confirmed; reopen /setup")]
    SetupChanged,
}

impl CredentialError {
    pub(crate) fn outcome_unknown(&self) -> bool {
        matches!(self, Self::WriteOutcomeUnknown | Self::DeleteOutcomeUnknown)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum SetupData {
    Native,
    ChatGpt,
}

pub(crate) struct SetupReset {
    state: StateRoot,
    data: SetupData,
    records: [Option<Vec<u8>>; 3],
}

pub(crate) struct NativeRemoval {
    state: StateRoot,
    record: Vec<u8>,
    models: Option<Vec<u8>>,
    keyring_identity: Option<[u8; 32]>,
}

impl SetupReset {
    pub(crate) fn prepare(workspace: &Path, data: SetupData) -> Result<Self, CredentialError> {
        let path = StateRoot::account_path().map_err(|_| CredentialError::StateUnavailable)?;
        let state = StateRoot::admit(&path).map_err(|_| CredentialError::StateUnavailable)?;
        Self::at(state, workspace, data)
    }

    fn at(state: StateRoot, workspace: &Path, data: SetupData) -> Result<Self, CredentialError> {
        let records = state
            .with_account_replacement_lock(workspace, || Self::read(&state, data))
            .map_err(|_| CredentialError::StateUnavailable)??;
        Ok(Self {
            state,
            data,
            records,
        })
    }

    fn read(state: &StateRoot, data: SetupData) -> Result<[Option<Vec<u8>>; 3], CredentialError> {
        let record = match data {
            SetupData::Native => state.record(RecordKind::SavedAccount).read(),
            SetupData::ChatGpt => state.record(RecordKind::ChatgptAccounts).read(),
        }
        .map_err(|_| CredentialError::StateUnavailable)?;
        let registration = if matches!(data, SetupData::ChatGpt) {
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .map_err(|_| CredentialError::StateUnavailable)?
        } else {
            None
        };
        let models = state
            .record(RecordKind::ModelPreferences)
            .read()
            .map_err(|_| CredentialError::StateUnavailable)?;
        Ok([record, registration, models])
    }

    pub(crate) fn apply(self, workspace: &Path) -> Result<(), CredentialError> {
        self.state
            .with_account_replacement_lock(workspace, || {
                if Self::read(&self.state, self.data)? != self.records {
                    return Err(CredentialError::SetupChanged);
                }
                let records: &[(RecordKind, usize)] = match self.data {
                    SetupData::Native => &[
                        (RecordKind::SavedAccount, 0),
                        (RecordKind::ModelPreferences, 2),
                    ],
                    SetupData::ChatGpt => &[
                        (RecordKind::ChatgptAccounts, 0),
                        (RecordKind::ChatgptRegistration, 1),
                        (RecordKind::ModelPreferences, 2),
                    ],
                };
                let mut changed = false;
                for &(kind, index) in records {
                    self.state.record(kind).remove().map_err(|error| {
                        if changed {
                            CredentialError::WriteOutcomeUnknown
                        } else {
                            account_record_error(error)
                        }
                    })?;
                    changed |= self.records[index].is_some();
                }
                Ok(())
            })
            .map_err(|_| CredentialError::StateUnavailable)?
    }
}

impl NativeRemoval {
    pub(crate) fn prepare(workspace: &Path) -> Result<Option<Self>, CredentialError> {
        let path = StateRoot::account_path().map_err(|_| CredentialError::StateUnavailable)?;
        let state = StateRoot::admit(&path).map_err(|_| CredentialError::StateUnavailable)?;
        Self::prepare_at(state, workspace, || keyring_helper::read(DEFAULT_ACCOUNT))
    }

    fn prepare_at(
        state: StateRoot,
        workspace: &Path,
        read_keyring: impl FnOnce() -> Result<Option<Vec<u8>>, CredentialError>,
    ) -> Result<Option<Self>, CredentialError> {
        let records = state
            .with_account_replacement_lock(workspace, || {
                let record = state
                    .record(SAVED_ACCOUNT.kind)
                    .read()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                let models = state
                    .record(RecordKind::ModelPreferences)
                    .read()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                let keyring_identity = if record
                    .as_deref()
                    .map(StoredAccountFile::parse)
                    .transpose()?
                    .is_some_and(|stored| stored.storage == AccountStorage::Keyring)
                {
                    Some(keyring_identity(
                        &read_keyring()?.ok_or(CredentialError::InvalidAccount)?,
                    ))
                } else {
                    None
                };
                Ok::<_, CredentialError>((record, models, keyring_identity))
            })
            .map_err(|_| CredentialError::StateUnavailable)??;
        let Some(record) = records.0 else {
            return Ok(None);
        };
        Ok(Some(Self {
            state,
            record,
            models: records.1,
            keyring_identity: records.2,
        }))
    }

    pub(crate) fn apply(self, workspace: &Path) -> Result<(), CredentialError> {
        self.apply_with(
            workspace,
            || keyring_helper::read(DEFAULT_ACCOUNT),
            keyring_helper::delete,
        )
    }

    fn apply_with(
        self,
        workspace: &Path,
        read_keyring: impl FnOnce() -> Result<Option<Vec<u8>>, CredentialError>,
        delete_keyring: impl FnOnce(&str) -> Result<(), CredentialError>,
    ) -> Result<(), CredentialError> {
        self.state
            .with_account_replacement_lock(workspace, || {
                let record = self
                    .state
                    .record(RecordKind::SavedAccount)
                    .read()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                let models = self
                    .state
                    .record(RecordKind::ModelPreferences)
                    .read()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                if record.as_deref() != Some(&self.record) || models != self.models {
                    return Err(CredentialError::SetupChanged);
                }
                let stored = StoredAccountFile::parse(&self.record)?;
                let keyring_slot = if stored.storage == AccountStorage::Keyring {
                    let credential = read_keyring()
                        .map_err(|_| CredentialError::SetupChanged)?
                        .ok_or(CredentialError::SetupChanged)?;
                    if !keyring_identity_matches(self.keyring_identity, &credential) {
                        return Err(CredentialError::SetupChanged);
                    }
                    Some(DEFAULT_ACCOUNT)
                } else {
                    None
                };
                self.state
                    .record(RecordKind::SavedAccount)
                    .remove()
                    .map_err(account_record_error)?;
                self.state
                    .record(RecordKind::ModelPreferences)
                    .remove()
                    .map_err(|_| CredentialError::WriteOutcomeUnknown)?;
                if let Some(slot) = keyring_slot {
                    delete_keyring(slot).map_err(|_| CredentialError::DeleteOutcomeUnknown)?;
                }
                Ok(())
            })
            .map_err(|_| CredentialError::StateUnavailable)?
    }
}

fn keyring_identity(credential: &[u8]) -> [u8; 32] {
    sha2::Sha256::digest(credential).into()
}

fn keyring_identity_matches(expected: Option<[u8; 32]>, credential: &[u8]) -> bool {
    expected == Some(keyring_identity(credential))
}

pub(super) fn account_record_error(error: StoreError) -> CredentialError {
    if matches!(error, StoreError::ReplaceUnconfirmed) {
        CredentialError::WriteOutcomeUnknown
    } else {
        CredentialError::StateUnavailable
    }
}

#[cfg(target_os = "linux")]
fn safe_unix_value(value: &str, absolute: bool) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
        && (!absolute || Path::new(value).is_absolute())
        && value
            .split('/')
            .all(|component| !matches!(component, "." | ".."))
}

#[cfg(target_os = "linux")]
fn safe_bus_address(value: &str) -> bool {
    let Some(parameters) = value.strip_prefix("unix:") else {
        return false;
    };
    let mut parts = parameters.split(',');
    let Some(socket) = parts.next() else {
        return false;
    };
    let valid_socket = socket
        .strip_prefix("path=")
        .is_some_and(|path| safe_unix_value(path, true))
        || socket
            .strip_prefix("abstract=")
            .is_some_and(|name| safe_unix_value(name, false));
    let valid_guid = parts.next().is_none_or(|part| {
        part.strip_prefix("guid=").is_some_and(|guid| {
            guid.len() == 32 && guid.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    });
    valid_socket && valid_guid && parts.next().is_none()
}

pub(crate) fn admit_transport() -> Result<(), CredentialError> {
    #[cfg(target_os = "linux")]
    {
        if let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
            let address = address
                .into_string()
                .map_err(|_| CredentialError::UnsafeTransport)?;
            if !safe_bus_address(&address) {
                return Err(CredentialError::UnsafeTransport);
            }
        } else if let Some(directory) = std::env::var_os("XDG_RUNTIME_DIR") {
            let directory = directory
                .into_string()
                .map_err(|_| CredentialError::UnsafeTransport)?;
            if !safe_unix_value(&directory, true) {
                return Err(CredentialError::UnsafeTransport);
            }
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedAccount {
    pub id: Uuid,
    pub provider: ProviderRecord,
    pub model: String,
    #[serde(deserialize_with = "Option::deserialize")]
    pub effort: Option<Effort>,
    api_key: String,
    #[serde(deserialize_with = "Option::deserialize")]
    anthropic_workspace_id: Option<String>,
}

impl SavedAccount {
    pub(crate) fn new(
        provider: String,
        model: String,
        effort: Option<Effort>,
        api_key: String,
    ) -> Result<Self, CredentialError> {
        let account = Self {
            id: Uuid::now_v7(),
            provider: ProviderRecord::parse(&provider).ok_or(CredentialError::InvalidAccount)?,
            model,
            effort,
            api_key,
            anthropic_workspace_id: None,
        };
        account.validate()?;
        Ok(account)
    }

    pub(crate) fn with_anthropic_workspace(
        mut self,
        workspace_id: Option<String>,
    ) -> Result<Self, CredentialError> {
        self.anthropic_workspace_id = workspace_id;
        self.validate()?;
        Ok(self)
    }

    #[cfg(test)]
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    pub(crate) fn credentials(&self) -> Result<NativeApiCredentials, CredentialError> {
        NativeApiCredentials::new(
            &self.provider.profile(),
            self.api_key.clone(),
            self.anthropic_workspace_id.clone(),
        )
        .map_err(|_| CredentialError::InvalidAccount)
    }

    pub(crate) fn into_credentials(self) -> Result<NativeApiCredentials, CredentialError> {
        self.validate()?;
        NativeApiCredentials::new(
            &self.provider.profile(),
            self.api_key,
            self.anthropic_workspace_id,
        )
        .map_err(|_| CredentialError::InvalidAccount)
    }

    pub(crate) fn snapshot_digest(&self) -> Result<[u8; 32], CredentialError> {
        let bytes = serde_json::to_vec(self).map_err(|_| CredentialError::InvalidAccount)?;
        Ok(sha2::Sha256::digest(bytes).into())
    }

    fn validate(&self) -> Result<(), CredentialError> {
        if self.id.get_version() != Some(Version::SortRand)
            || !self.provider.kind().is_native_api()
            || (resolve_native_effort(&self.provider.profile(), &self.model, self.effort).is_err()
                && (self.effort.is_none() || validate_native_model_id(&self.model).is_err()))
            || self.credentials().is_err()
        {
            return Err(CredentialError::InvalidAccount);
        }
        Ok(())
    }
}

pub(crate) async fn save(
    workspace: &Path,
    account: SavedAccount,
    storage: AccountStorage,
    expected: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    let account_root = StateRoot::account_path().map_err(|_| CredentialError::StateUnavailable)?;
    match storage {
        AccountStorage::Keyring => {
            save_at(DEFAULT_ACCOUNT, &account_root, workspace, account, expected).await
        }
        AccountStorage::PrivateFile => {
            save_file_at(&account_root, workspace, account, expected).await
        }
    }
}

async fn save_at(
    slot: &str,
    state_dir: &Path,
    workspace: &Path,
    account: SavedAccount,
    expected: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    account.validate()?;
    admit_transport()?;
    let record = serde_json::to_string(&account).map_err(|_| CredentialError::InvalidAccount)?;
    if record.len() > RecordKind::SavedAccount.limit() {
        return Err(CredentialError::InvalidAccount);
    }
    let slot = slot.to_owned();
    let state_dir = state_dir.to_path_buf();
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let state = StateRoot::admit(&state_dir).map_err(|_| CredentialError::StateUnavailable)?;
        state
            .with_account_replacement_lock(&workspace, || {
                let existing = state
                    .record(SAVED_ACCOUNT.kind)
                    .read()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                if existing
                    .as_deref()
                    .map(StoredAccountFile::parse)
                    .transpose()?
                    .is_some_and(|stored| stored.storage == AccountStorage::PrivateFile)
                {
                    return Err(CredentialError::InvalidAccount);
                }
                let current = if existing.is_some() {
                    parse_account(keyring_helper::read(&slot)?)?
                } else {
                    None
                };
                require_account_snapshot(current.as_ref(), expected)?;
                write_marked_keyring_account(&state, &slot, &record, keyring_helper::write)
            })
            .map_err(|error| match error {
                StoreError::AccountBusy => CredentialError::AccountBusy,
                _ => CredentialError::StateUnavailable,
            })?
    })
    .await
    .map_err(|_| CredentialError::WriteOutcomeUnknown)?
}

fn write_marked_keyring_account(
    state: &StateRoot,
    slot: &str,
    record: &str,
    write: impl FnOnce(&str, &str) -> Result<(), CredentialError>,
) -> Result<(), CredentialError> {
    let marker = StoredAccountFile {
        storage: AccountStorage::Keyring,
        account: None,
    }
    .encoded()?;
    state
        .record(RecordKind::SavedAccount)
        .replace(&marker)
        .map_err(account_record_error)?;
    write(slot, record).map_err(|_| CredentialError::WriteOutcomeUnknown)
}

async fn save_file_at(
    account_root: &Path,
    workspace: &Path,
    account: SavedAccount,
    expected: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    account.validate()?;
    let record = StoredAccountFile {
        storage: AccountStorage::PrivateFile,
        account: Some(account),
    }
    .encoded()?;
    let account_root = account_root.to_path_buf();
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let state =
            StateRoot::admit(&account_root).map_err(|_| CredentialError::StateUnavailable)?;
        state
            .with_account_replacement_lock(&workspace, || {
                let existing = state
                    .record(SAVED_ACCOUNT.kind)
                    .read()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                if existing
                    .as_deref()
                    .map(StoredAccountFile::parse)
                    .transpose()?
                    .is_some_and(|stored| stored.storage == AccountStorage::Keyring)
                {
                    return Err(CredentialError::InvalidAccount);
                }
                let current = existing
                    .as_deref()
                    .map(StoredAccountFile::parse)
                    .transpose()?
                    .and_then(|stored| stored.account);
                require_account_snapshot(current.as_ref(), expected)?;
                state
                    .record(RecordKind::SavedAccount)
                    .replace(&record)
                    .map_err(account_record_error)
            })
            .map_err(|error| match error {
                StoreError::AccountBusy => CredentialError::AccountBusy,
                _ => CredentialError::StateUnavailable,
            })?
    })
    .await
    .map_err(|_| CredentialError::WriteOutcomeUnknown)?
}

fn require_account_snapshot(
    current: Option<&SavedAccount>,
    expected: Option<[u8; 32]>,
) -> Result<(), CredentialError> {
    if current.map(SavedAccount::snapshot_digest).transpose()? != expected {
        return Err(CredentialError::SetupChanged);
    }
    Ok(())
}

pub(crate) async fn inspect() -> Result<InspectedAccount, CredentialError> {
    inspect_with_interaction(false).await
}

pub(crate) async fn inspect_interactive() -> Result<InspectedAccount, CredentialError> {
    inspect_with_interaction(true).await
}

async fn inspect_with_interaction(interactive: bool) -> Result<InspectedAccount, CredentialError> {
    let file = tokio::task::spawn_blocking(load_file)
        .await
        .map_err(|_| CredentialError::StateUnavailable)??;
    let Some(stored) = file else {
        probe_keyring().await?;
        return Ok(InspectedAccount {
            account: None,
            storage: AccountStorage::Keyring,
        });
    };
    if stored.storage == AccountStorage::PrivateFile {
        return Ok(InspectedAccount {
            account: stored.account,
            storage: AccountStorage::PrivateFile,
        });
    }
    let account = if interactive {
        keyring_helper::read_interactive(DEFAULT_ACCOUNT)
            .await
            .and_then(parse_account)
    } else {
        load_at(DEFAULT_ACCOUNT).await
    }
    .map_err(|error| {
        if matches!(
            error,
            CredentialError::Unavailable | CredentialError::TimedOut
        ) {
            CredentialError::PinnedStoreUnavailable
        } else {
            error
        }
    })?;
    Ok(InspectedAccount {
        account,
        storage: AccountStorage::Keyring,
    })
}

fn load_file() -> Result<Option<StoredAccountFile>, CredentialError> {
    let path = StateRoot::account_path().map_err(|_| CredentialError::StateUnavailable)?;
    load_file_at(&path)
}

fn load_file_at(path: &Path) -> Result<Option<StoredAccountFile>, CredentialError> {
    let Some(state) =
        arany::StateRoot::open_optional(path).map_err(|_| CredentialError::StateUnavailable)?
    else {
        return Ok(None);
    };
    let Some(record) = state
        .record(SAVED_ACCOUNT.kind)
        .read()
        .map_err(|_| CredentialError::StateUnavailable)?
    else {
        return Ok(None);
    };
    Ok(Some(StoredAccountFile::parse(&record)?))
}

pub(crate) async fn load() -> Result<Option<SavedAccount>, CredentialError> {
    Ok(inspect().await?.account)
}

async fn load_at(slot: &str) -> Result<Option<SavedAccount>, CredentialError> {
    admit_transport()?;
    let slot = slot.to_owned();
    tokio::task::spawn_blocking(move || parse_account(keyring_helper::read(&slot)?))
        .await
        .map_err(|_| CredentialError::Unavailable)?
}

fn parse_account(record: Option<Vec<u8>>) -> Result<Option<SavedAccount>, CredentialError> {
    record.map(|record| decode_account(&record)).transpose()
}

fn decode_account(record: &[u8]) -> Result<SavedAccount, CredentialError> {
    if record.len() > RecordKind::SavedAccount.limit() {
        return Err(CredentialError::InvalidAccount);
    }
    let account: SavedAccount =
        serde_json::from_slice(record).map_err(|_| CredentialError::InvalidAccount)?;
    account.validate()?;
    Ok(account)
}

pub(crate) async fn load_selected(
    id: Uuid,
    provider: &str,
) -> Result<NativeApiCredentials, CredentialError> {
    let account = load().await?.ok_or(CredentialError::InvalidAccount)?;
    selected_credentials(&account, id, provider)
}

pub(crate) fn keyring_helper_main() -> std::process::ExitCode {
    keyring_helper::main()
}

fn selected_credentials(
    account: &SavedAccount,
    id: Uuid,
    provider: &str,
) -> Result<NativeApiCredentials, CredentialError> {
    if account.id != id || account.provider.profile() != provider {
        return Err(CredentialError::InvalidAccount);
    }
    account.validate()?;
    account.credentials()
}

#[cfg(test)]
fn selected_key(
    account: &SavedAccount,
    id: Uuid,
    provider: &str,
) -> Result<String, CredentialError> {
    selected_credentials(account, id, provider).map(NativeApiCredentials::into_api_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_directory_sync_failure_preserves_uncertainty() {
        for (data, kind, name) in [
            (
                SetupData::Native,
                RecordKind::SavedAccount,
                "account-credentials.json",
            ),
            (
                SetupData::ChatGpt,
                RecordKind::ChatgptAccounts,
                "chatgpt-accounts.json",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let state = StateRoot::admit(&temp.path().join("state")).unwrap();
            state
                .record(kind)
                .replace(b"damaged configuration")
                .unwrap();
            let reset = SetupReset::at(
                StateRoot::open_existing(state.path()).unwrap(),
                &workspace,
                data,
            )
            .unwrap();
            arany::FaultPlan::fail_record_sync(state.path().to_owned(), name);
            assert_eq!(
                reset.apply(&workspace),
                Err(CredentialError::WriteOutcomeUnknown)
            );
            assert!(state.record(kind).read().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn native_file_publication_fault_is_uncertain_and_stale_replacement_refuses() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let first = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-first-key".into(),
        )
        .unwrap();
        let expected = Some(first.snapshot_digest().unwrap());
        arany::FaultPlan::fail_record_sync(root.clone(), "account-credentials.json");
        assert_eq!(
            save_file_at(&root, &workspace, first, None).await,
            Err(CredentialError::WriteOutcomeUnknown)
        );
        let second = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-second-key".into(),
        )
        .unwrap();
        let current = Some(second.snapshot_digest().unwrap());
        save_file_at(&root, &workspace, second, expected)
            .await
            .unwrap();
        let rejected = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-rejected-key".into(),
        )
        .unwrap();
        assert_eq!(
            save_file_at(&root, &workspace, rejected, expected).await,
            Err(CredentialError::SetupChanged)
        );
        let reopened = load_file_at(&root).unwrap().unwrap().account.unwrap();
        assert_eq!(Some(reopened.snapshot_digest().unwrap()), current);
    }

    #[test]
    fn native_snapshot_detects_keyring_credential_change_with_the_same_account_id() {
        let mut account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-original-key".into(),
        )
        .unwrap();
        let expected = Some(account.snapshot_digest().unwrap());
        require_account_snapshot(Some(&account), expected).unwrap();
        account.api_key = "synthetic-changed-key".into();
        assert_eq!(
            require_account_snapshot(Some(&account), expected),
            Err(CredentialError::SetupChanged)
        );
    }

    #[test]
    fn keyring_identity_changes_when_selected_credential_changes() {
        assert_ne!(
            keyring_identity(b"first credential"),
            keyring_identity(b"replacement credential")
        );
        let snapshot = Some(keyring_identity(b"first credential"));
        assert!(keyring_identity_matches(snapshot, b"first credential"));
        assert!(!keyring_identity_matches(
            snapshot,
            b"replacement credential"
        ));
    }

    #[test]
    fn native_removal_reads_keyring_identity_inside_account_lock() {
        let temp = tempfile::tempdir().unwrap();
        let state = StateRoot::admit(&temp.path().join("state")).unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        state
            .record(RecordKind::SavedAccount)
            .replace(br#"{"storage":"keyring","account":null}"#)
            .unwrap();
        let probe_state = StateRoot::admit(state.path()).unwrap();
        let probe_workspace = workspace.clone();

        let removal = NativeRemoval::prepare_at(state, &workspace, || {
            assert!(matches!(
                probe_state.with_account_replacement_lock(&probe_workspace, || ()),
                Err(StoreError::AccountBusy)
            ));
            Ok(Some(b"selected credential".to_vec()))
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            removal.keyring_identity,
            Some(keyring_identity(b"selected credential"))
        );
    }

    #[tokio::test]
    async fn native_save_keyring_failures_after_marker_block_continuation() {
        for failure in [CredentialError::Locked, CredentialError::Unavailable] {
            let temp = tempfile::tempdir().unwrap();
            let state = StateRoot::admit(&temp.path().join("state")).unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            crate::cli::attached::assert_uncertain_publication(
                crate::cli::attached::SetupPublication::NativeSave,
                || {
                    let result = state
                        .with_account_replacement_lock(&workspace, || {
                            write_marked_keyring_account(
                                &state,
                                DEFAULT_ACCOUNT,
                                "synthetic credential",
                                |_, _| Err(failure),
                            )
                        })
                        .unwrap();
                    assert_eq!(result, Err(CredentialError::WriteOutcomeUnknown));
                    let reopened = StateRoot::open_existing(state.path()).unwrap();
                    let bytes = reopened
                        .record(RecordKind::SavedAccount)
                        .read()
                        .unwrap()
                        .unwrap();
                    let marker = StoredAccountFile::parse(&bytes).unwrap();
                    assert_eq!(marker.storage, AccountStorage::Keyring);
                    assert!(marker.account.is_none());
                    crate::accounts::chatgpt::AuthorizationError::CredentialStore(
                        result.unwrap_err(),
                    )
                },
            )
            .await;
        }
    }

    #[tokio::test]
    async fn partial_native_removal_is_uncertain_after_each_publication() {
        for failure in [
            CredentialError::Unavailable,
            CredentialError::DeleteOutcomeUnknown,
            CredentialError::StateUnavailable,
        ] {
            let temp = tempfile::tempdir().unwrap();
            let state = StateRoot::admit(&temp.path().join("state")).unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let record = br#"{"storage":"keyring","account":null}"#.to_vec();
            state
                .record(RecordKind::SavedAccount)
                .replace(&record)
                .unwrap();
            state
                .record(RecordKind::ModelPreferences)
                .replace(b"saved model preferences")
                .unwrap();
            let removal = NativeRemoval::prepare_at(
                StateRoot::open_existing(state.path()).unwrap(),
                &workspace,
                || Ok(Some(b"selected credential".to_vec())),
            )
            .unwrap()
            .unwrap();
            let preference_failure = failure == CredentialError::StateUnavailable;
            if preference_failure {
                arany::FaultPlan::fail_record_publication(
                    state.path().to_owned(),
                    "model-preferences.json",
                );
            }
            crate::cli::attached::assert_uncertain_publication(
                crate::cli::attached::SetupPublication::NativeRemoval,
                || {
                    let result = removal.apply_with(
                        &workspace,
                        || Ok(Some(b"selected credential".to_vec())),
                        |_| {
                            assert_ne!(
                                failure,
                                CredentialError::StateUnavailable,
                                "preference failure must prevent keyring dispatch"
                            );
                            Err(failure)
                        },
                    );
                    let expected = if preference_failure {
                        CredentialError::WriteOutcomeUnknown
                    } else {
                        CredentialError::DeleteOutcomeUnknown
                    };
                    let error = result.unwrap_err();
                    assert_eq!(error, expected);
                    assert!(
                        state
                            .record(RecordKind::SavedAccount)
                            .read()
                            .unwrap()
                            .is_none()
                    );
                    assert_eq!(
                        state
                            .record(RecordKind::ModelPreferences)
                            .read()
                            .unwrap()
                            .is_some(),
                        preference_failure
                    );
                    crate::accounts::chatgpt::AuthorizationError::CredentialStore(error)
                },
            )
            .await;
        }
    }
    #[cfg(target_os = "linux")]
    use std::process::Command;

    #[cfg(not(target_os = "macos"))]
    struct RemoveTestAccount(String);

    #[cfg(not(target_os = "macos"))]
    impl Drop for RemoveTestAccount {
        fn drop(&mut self) {
            if let Ok(entry) = Entry::new(SERVICE, &self.0) {
                let _ = entry.delete_credential();
            }
        }
    }

    #[cfg(all(target_os = "macos", feature = "test-support"))]
    #[tokio::test]
    #[cfg_attr(
        not(arany_ci),
        ignore = "isolated native macOS Keychain gate; requires ARANY_TEST_EXE pointing to fixture-enabled arany"
    )]
    #[allow(clippy::await_holding_lock)]
    async fn native_store_round_trips_an_isolated_synthetic_account() {
        let _gate = crate::test_process::NATIVE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        super::macos_tests::roundtrip().await;
    }

    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    #[ignore = "requires a native unlocked OS credential store"]
    async fn native_store_round_trips_an_isolated_synthetic_account() {
        let executable =
            std::env::var_os("ARANY_TEST_EXE").expect("set ARANY_TEST_EXE to built arany");
        assert!(std::path::Path::new(&executable).is_absolute());
        #[cfg(target_os = "linux")]
        if let Some(stage) = std::env::var_os("ARANY_NATIVE_ACCOUNT_STAGE") {
            let account_root = std::path::PathBuf::from(
                std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").expect("private account root"),
            );
            assert_eq!(StateRoot::account_path().unwrap(), account_root);
            let session_root = std::path::PathBuf::from(
                std::env::var_os("XDG_STATE_HOME").expect("private Session root"),
            );
            assert!(StateRoot::default_path().unwrap().starts_with(session_root));
            let workspace = std::path::PathBuf::from(
                std::env::var_os("ARANY_NATIVE_WORKSPACE").expect("private workspace"),
            );
            let slot = std::env::var("ARANY_NATIVE_ACCOUNT_SLOT").expect("random slot");
            let replacement = SavedAccount::new(
                "openai".into(),
                "gpt-5.4".into(),
                Some(Effort::Low),
                "synthetic-replacement-key".into(),
            )
            .expect("synthetic replacement");
            let expected = load_at(&slot)
                .await
                .unwrap()
                .as_ref()
                .map(SavedAccount::snapshot_digest)
                .transpose()
                .unwrap();
            let result = save_at(&slot, &account_root, &workspace, replacement, expected).await;
            match stage.to_str() {
                Some("busy") => assert_eq!(result, Err(CredentialError::AccountBusy)),
                Some("replace") => result.expect("replace after lock release"),
                _ => panic!("unknown native account test stage"),
            }
            return;
        }

        let slot = format!("native-test-{}", Uuid::now_v7());
        let _cleanup = RemoveTestAccount(slot.clone());
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state_dir = temp.path().join("account-root");
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-test-key".into(),
        )
        .expect("synthetic account");
        let id = account.id;
        save_at(&slot, &state_dir, &workspace, account, None)
            .await
            .expect("save isolated account");
        let loaded = load_at(&slot)
            .await
            .expect("load isolated account")
            .expect("saved account");
        assert_eq!(loaded.id, id);
        assert_eq!(loaded.api_key(), "synthetic-test-key");
        #[cfg(target_os = "linux")]
        {
            let state = StateRoot::open_existing(&state_dir).expect("private account root");
            let marker = state
                .record(RecordKind::SavedAccount)
                .read()
                .expect("backend marker")
                .expect("pinned keyring backend");
            assert_eq!(
                StoredAccountFile::parse(&marker).unwrap().storage,
                AccountStorage::Keyring
            );
            assert!(
                !marker
                    .windows(b"synthetic-test-key".len())
                    .any(|part| { part == b"synthetic-test-key" })
            );

            let child = |stage: &str, session_root: &str| {
                let mut command = Command::new("/usr/bin/timeout");
                command
                    .args(["-k", "1s", "8s"])
                    .arg(std::env::current_exe().expect("test executable"))
                    .args([
                        "--ignored",
                        "--exact",
                        "accounts::credentials::tests::native_store_round_trips_an_isolated_synthetic_account",
                    ])
                    .env_clear()
                    .env("ARANY_TEST_EXE", &executable)
                    .env("ARANY_NATIVE_ACCOUNT_STAGE", stage)
                    .env("ARANY_NATIVE_ACCOUNT_SLOT", &slot)
                    .env("ARANY_NATIVE_WORKSPACE", &workspace)
                    .env("ARANY_TEST_ACCOUNT_ROOT", &state_dir)
                    .env("XDG_STATE_HOME", temp.path().join(session_root))
                    .current_dir(temp.path());
                for name in [
                    "DBUS_SESSION_BUS_ADDRESS",
                    "XDG_RUNTIME_DIR",
                    "HOME",
                    "USER",
                    "LOGNAME",
                ] {
                    if let Some(value) = std::env::var_os(name) {
                        command.env(name, value);
                    }
                }
                let output = crate::test_process::file_output(&mut command, None);
                assert!(
                    output.status.success(),
                    "native account child {stage} failed"
                );
                assert!(
                    output
                        .stdout
                        .windows(b"test result: ok. 1 passed; 0 failed; 0 ignored;".len())
                        .any(|part| part == b"test result: ok. 1 passed; 0 failed; 0 ignored;"),
                    "native account child {stage} did not complete exactly one test"
                );
                assert!(
                    output
                        .stdout
                        .windows(b"running 1 test".len())
                        .any(|part| { part == b"running 1 test" }),
                    "native account child {stage} did not run its filtered test"
                );
                for secret in [
                    &b"synthetic-test-key"[..],
                    &b"synthetic-replacement-key"[..],
                ] {
                    assert!(
                        !output
                            .stdout
                            .windows(secret.len())
                            .any(|part| part == secret)
                    );
                    assert!(
                        !output
                            .stderr
                            .windows(secret.len())
                            .any(|part| part == secret)
                    );
                }
            };
            state
                .with_account_replacement_lock(&workspace, || child("busy", "session-a"))
                .expect("hold shared account lock");
            assert_eq!(
                state.record(RecordKind::SavedAccount).read().unwrap(),
                Some(marker.clone()),
                "rejected replacement changed the backend marker"
            );
            let unchanged = load_at(&slot)
                .await
                .unwrap()
                .expect("original keyring item");
            assert_eq!(unchanged.id, id);
            assert_eq!(unchanged.api_key(), "synthetic-test-key");

            child("replace", "session-b");
            let replaced = load_at(&slot)
                .await
                .unwrap()
                .expect("replacement keyring item");
            assert_ne!(replaced.id, id);
            assert_eq!(replaced.api_key(), "synthetic-replacement-key");
            assert_eq!(
                selected_key(&replaced, id, "openai"),
                Err(CredentialError::InvalidAccount)
            );
            assert_eq!(
                selected_key(&replaced, replaced.id, "openai"),
                Ok("synthetic-replacement-key".into())
            );
            assert_eq!(
                state.record(RecordKind::SavedAccount).read().unwrap(),
                Some(marker)
            );
        }
        assert_eq!(
            selected_key(&loaded, Uuid::now_v7(), "openai"),
            Err(CredentialError::InvalidAccount)
        );
        Entry::new(SERVICE, &slot)
            .expect("test credential entry")
            .delete_credential()
            .expect("remove isolated account");
        assert!(load_at(&slot).await.expect("check removal").is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "native Linux Secret Service same-user client isolation release gate"]
    async fn native_store_denies_another_same_user_client() {
        let slot = format!("native-client-test-{}", Uuid::now_v7());
        let _cleanup = RemoveTestAccount(slot.clone());
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-client-test-key".into(),
        )
        .expect("synthetic account");
        let expected = serde_json::to_vec(&account).expect("synthetic record");
        save_at(&slot, &temp.path().join("state"), &workspace, account, None)
            .await
            .expect("save isolated account");

        let mut client = Command::new("/usr/bin/timeout");
        client
            .args([
                "-k",
                "1s",
                "5s",
                "/usr/bin/secret-tool",
                "lookup",
                "service",
                SERVICE,
                "username",
            ])
            .arg(&slot)
            .current_dir(&workspace)
            .env_clear();
        for name in ["DBUS_SESSION_BUS_ADDRESS", "XDG_RUNTIME_DIR"] {
            if let Some(value) = std::env::var_os(name) {
                client.env(name, value);
            }
        }
        let output = crate::test_process::file_output(&mut client, None);
        let retrieved = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
        let exposed = output.status.success() && retrieved == expected;

        Entry::new(SERVICE, &slot)
            .expect("test credential entry")
            .delete_credential()
            .expect("remove isolated account");
        assert!(load_at(&slot).await.expect("check removal").is_none());
        assert!(
            matches!(output.status.code(), Some(0 | 1)) && output.stderr.is_empty(),
            "external Secret Service lookup was inconclusive"
        );
        assert!(
            !exposed,
            "another same-user client retrieved Arany's saved account record"
        );
    }

    #[test]
    fn record_validation_is_profile_and_model_specific() {
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-key".into(),
        )
        .expect("reviewed account");
        assert_eq!(account.api_key(), "synthetic-key");
        assert!(SavedAccount::new("other".into(), "gpt-5.4".into(), None, "key".into()).is_err());
        assert!(
            SavedAccount::new("openai".into(), "unreviewed".into(), None, "key".into()).is_err()
        );
        assert!(
            SavedAccount::new(
                "openai".into(),
                "unreviewed".into(),
                Some(Effort::High),
                "key".into(),
            )
            .is_ok()
        );
        assert!(
            SavedAccount::new(
                "openai".into(),
                "bad model".into(),
                Some(Effort::High),
                "key".into(),
            )
            .is_err()
        );
        assert!(
            SavedAccount::new("openai".into(), "gpt-5.4".into(), None, "bad\nkey".into()).is_err()
        );
    }

    #[test]
    fn malformed_records_and_diagnostics_do_not_reveal_secret() {
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            None,
            "synthetic-key".into(),
        )
        .expect("reviewed account");
        let mut record = serde_json::to_value(account).expect("serialize account");
        assert!(record["anthropic_workspace_id"].is_null());
        for field in ["effort", "anthropic_workspace_id"] {
            let mut missing = record.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<SavedAccount>(missing).is_err());
        }
        record["schema"] = 3.into();
        assert!(serde_json::from_value::<SavedAccount>(record).is_err());
        let mut scoped = serde_json::json!({
            "id": Uuid::now_v7(),
            "provider": {"kind":"anthropic","custom_name":null},
            "model": "claude-sonnet-5",
            "effort": null,
            "api_key": "synthetic-key",
            "anthropic_workspace_id": "wrkspc_Test123"
        });
        assert!(
            serde_json::from_value::<SavedAccount>(scoped.clone())
                .is_ok_and(|account| account.validate().is_ok()),
            "a valid workspace account must be admitted"
        );
        let mut previous = scoped.clone();
        previous["provider"] = "anthropic".into();
        assert!(matches!(
            decode_account(&serde_json::to_vec(&previous).unwrap()),
            Err(CredentialError::InvalidAccount)
        ));
        for (field, value) in [
            (
                "provider",
                serde_json::json!({"kind":"anthropic","custom_name":"other"}),
            ),
            (
                "provider",
                serde_json::json!({"kind":"custom","custom_name":null}),
            ),
            (
                "provider",
                serde_json::json!({"kind":"custom","custom_name":"other"}),
            ),
            ("anthropic_workspace_id", serde_json::json!("")),
            ("anthropic_workspace_id", serde_json::json!("wrkspc_")),
            (
                "anthropic_workspace_id",
                serde_json::json!("wrkspc_wrong-id"),
            ),
            ("anthropic_workspace_id", serde_json::json!("wrkspc_\n")),
            ("anthropic_workspace_id", serde_json::json!("w".repeat(129))),
            (
                "provider",
                serde_json::json!({"kind":"openai","custom_name":null}),
            ),
        ] {
            let original = scoped[field].clone();
            scoped[field] = value;
            assert!(
                !serde_json::from_value::<SavedAccount>(scoped.clone())
                    .is_ok_and(|account| account.validate().is_ok()),
                "invalid workspace/provider combination was admitted"
            );
            scoped[field] = original;
        }
        for (key, fits) in [("K".repeat(512), true), ("\\".repeat(512), false)] {
            let account =
                SavedAccount::new("anthropic".into(), "m".repeat(128), Some(Effort::High), key)
                    .and_then(|account| {
                        account
                            .with_anthropic_workspace(Some(format!("wrkspc_{}", "W".repeat(121))))
                    })
                    .expect("bounded fields");
            let encoded = StoredAccountFile {
                storage: AccountStorage::PrivateFile,
                account: Some(account),
            }
            .encoded();
            if fits {
                assert!(encoded.is_ok_and(|record| {
                    record.len() <= RecordKind::SavedAccount.limit()
                        && StoredAccountFile::parse(&record).is_ok()
                }));
            } else {
                assert!(matches!(encoded, Err(CredentialError::InvalidAccount)));
            }
        }
        assert_eq!(
            CredentialError::InvalidAccount.to_string(),
            "invalid saved account; use arany --setup to choose or reset access"
        );
        assert_eq!(
            CredentialError::Unavailable.to_string(),
            "OS credential store unavailable"
        );
    }

    #[test]
    fn saved_account_rejects_a_changed_identity_or_provider() {
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            None,
            "synthetic-key".into(),
        )
        .expect("reviewed account");
        assert_eq!(
            selected_key(&account, Uuid::now_v7(), "openai"),
            Err(CredentialError::InvalidAccount)
        );
        assert_eq!(
            selected_key(&account, account.id, "anthropic"),
            Err(CredentialError::InvalidAccount)
        );
        assert_eq!(
            selected_key(&account, account.id, "openai"),
            Ok("synthetic-key".into())
        );
        for workspace_id in [None, Some("wrkspc_Test123".to_owned())] {
            let scoped = SavedAccount::new(
                "anthropic".into(),
                "claude-sonnet-5".into(),
                None,
                "synthetic-key".into(),
            )
            .and_then(|account| account.with_anthropic_workspace(workspace_id.clone()))
            .expect("explicit account scope");
            let selected = selected_credentials(&scoped, scoped.id, "anthropic")
                .expect("matching selected account");
            assert!(selected.api_key() == "synthetic-key");
            assert_eq!(selected.anthropic_workspace_id(), workspace_id.as_deref());
            assert!(matches!(
                selected_credentials(&scoped, Uuid::now_v7(), "anthropic"),
                Err(CredentialError::InvalidAccount)
            ));
            assert!(matches!(
                selected_credentials(&scoped, scoped.id, "openai"),
                Err(CredentialError::InvalidAccount)
            ));
        }
    }

    #[tokio::test]
    async fn private_file_account_is_bounded_and_pinned_without_keyring_access() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let account_root = temp.path().join("user-root");
        assert!(
            load_file_at(&account_root)
                .expect("empty user root")
                .is_none()
        );
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-file-key".into(),
        )
        .expect("synthetic account");
        let id = account.id;
        save_file_at(&account_root, &workspace, account, None)
            .await
            .expect("save private account");
        let loaded = load_file_at(&account_root)
            .expect("read private account")
            .expect("saved account")
            .account
            .expect("file-backed account");
        assert_eq!(loaded.id, id);
        assert_eq!(
            selected_key(&loaded, id, "openai"),
            Ok("synthetic-file-key".into())
        );
        assert_eq!(
            selected_key(&loaded, Uuid::now_v7(), "openai"),
            Err(CredentialError::InvalidAccount)
        );
        let expected = Some(loaded.snapshot_digest().unwrap());
        assert_eq!(
            save_at(
                "isolated-never-used",
                &account_root,
                &workspace,
                loaded,
                expected
            )
            .await,
            Err(CredentialError::InvalidAccount)
        );

        let scoped = SavedAccount::new(
            "anthropic".into(),
            "claude-sonnet-5".into(),
            Some(Effort::High),
            "synthetic-scoped-key".into(),
        )
        .and_then(|account| account.with_anthropic_workspace(Some("wrkspc_Test123".into())))
        .expect("scoped account");
        let scoped_id = scoped.id;
        save_file_at(&account_root, &workspace, scoped, expected)
            .await
            .expect("save scoped replacement");
        let reopened = load_file_at(&account_root)
            .expect("reopen scoped account")
            .expect("saved scoped record")
            .account
            .expect("file-backed scope");
        let selected = selected_credentials(&reopened, scoped_id, "anthropic")
            .expect("reopened selected scope");
        assert!(selected.api_key() == "synthetic-scoped-key");
        assert_eq!(selected.anthropic_workspace_id(), Some("wrkspc_Test123"));
        assert!(matches!(
            selected_credentials(&reopened, id, "openai"),
            Err(CredentialError::InvalidAccount)
        ));

        let state = StateRoot::open_existing(&account_root).expect("private account root");
        let marker = StoredAccountFile {
            storage: AccountStorage::Keyring,
            account: None,
        }
        .encoded()
        .expect("keyring marker");
        state
            .record(RecordKind::SavedAccount)
            .replace(&marker)
            .expect("pin keyring backend");
        let replacement = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            None,
            "synthetic-replacement".into(),
        )
        .expect("replacement account");
        assert_eq!(
            save_file_at(&account_root, &workspace, replacement, None).await,
            Err(CredentialError::InvalidAccount)
        );
        let stored = load_file_at(&account_root)
            .expect("pinned backend")
            .expect("marker");
        assert_eq!(stored.storage, AccountStorage::Keyring);
        assert!(stored.account.is_none());

        for data in [SetupData::Native, SetupData::ChatGpt] {
            state
                .record(RecordKind::SavedAccount)
                .replace(b"unsupported-api-record")
                .unwrap();
            state
                .record(RecordKind::ChatgptAccounts)
                .replace(b"unsupported-chatgpt-record")
                .unwrap();
            state
                .record(RecordKind::ChatgptRegistration)
                .replace(b"unsupported-registration-record")
                .unwrap();
            state
                .record(RecordKind::ModelPreferences)
                .replace(b"unsupported-preferences")
                .unwrap();
            let snapshot = SetupReset::read(&state, data).unwrap();
            let reset = SetupReset::at(
                StateRoot::open_existing(&account_root).unwrap(),
                &workspace,
                data,
            )
            .unwrap();
            drop(reset);
            assert!(
                SetupReset::read(&state, data).unwrap() == snapshot,
                "cancellation preserves setup"
            );
            let reset = SetupReset::at(
                StateRoot::open_existing(&account_root).unwrap(),
                &workspace,
                data,
            )
            .unwrap();
            state
                .record(RecordKind::ModelPreferences)
                .replace(b"changed-preferences")
                .unwrap();
            let changed = SetupReset::read(&state, data).unwrap();
            assert_eq!(reset.apply(&workspace), Err(CredentialError::SetupChanged));
            assert!(
                SetupReset::read(&state, data).unwrap() == changed,
                "concurrent change preserves every record"
            );
            SetupReset::at(
                StateRoot::open_existing(&account_root).unwrap(),
                &workspace,
                data,
            )
            .unwrap()
            .apply(&workspace)
            .unwrap();
            assert!(
                state
                    .record(RecordKind::ModelPreferences)
                    .read()
                    .unwrap()
                    .is_none()
            );
            assert!(
                state.record(RecordKind::SavedAccount).read().unwrap()
                    == if matches!(data, SetupData::Native) {
                        None
                    } else {
                        Some(b"unsupported-api-record".to_vec())
                    }
            );
            assert!(
                state.record(RecordKind::ChatgptAccounts).read().unwrap()
                    == if matches!(data, SetupData::ChatGpt) {
                        None
                    } else {
                        Some(b"unsupported-chatgpt-record".to_vec())
                    }
            );
            assert!(
                state
                    .record(RecordKind::ChatgptRegistration)
                    .read()
                    .unwrap()
                    == if matches!(data, SetupData::ChatGpt) {
                        None
                    } else {
                        Some(b"unsupported-registration-record".to_vec())
                    }
            );
            if matches!(data, SetupData::Native) {
                let account = SavedAccount::new(
                    "openai".into(),
                    "gpt-5.4".into(),
                    None,
                    "synthetic-new-key".into(),
                )
                .unwrap();
                let id = account.id;
                save_file_at(&account_root, &workspace, account, None)
                    .await
                    .unwrap();
                assert_eq!(
                    load_file_at(&account_root)
                        .unwrap()
                        .unwrap()
                        .account
                        .unwrap()
                        .id,
                    id
                );
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn secret_service_transport_rejects_process_and_fallback_addresses() {
        for address in [
            "unixexec:path=/usr/bin/touch,argv1=/tmp/marker",
            "autolaunch:",
            "tcp:host=127.0.0.1,port=5555",
            "unix:path=/run/user/1000/bus;unixexec:path=/usr/bin/touch",
            "unix:path=/run/user/1000/bus,foo=bar",
            "unix:path=/run/user/../bus",
        ] {
            assert!(!safe_bus_address(address), "{address}");
        }
        for address in [
            "unix:path=/run/user/1000/bus",
            "unix:abstract=/tmp/dbus-123,guid=0123456789abcdef0123456789abcdef",
        ] {
            assert!(safe_bus_address(address), "{address}");
        }
    }
}
