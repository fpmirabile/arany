use arany::{
    Effort, NativeApiCredentials, StateRoot, StoreError, resolve_native_effort,
    validate_native_model_id,
};
#[cfg(test)]
use keyring::Entry;
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::{Uuid, Version};

mod keyring_helper;

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn native_test_output(
    command: &mut std::process::Command,
    input: Option<&[u8]>,
) -> std::process::Output {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::process::{Child, Output, Stdio};
    use std::time::{Duration, Instant};

    struct OwnedChild(Option<Child>);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    let root = tempfile::tempdir().expect("private native observation");
    let stdin = if let Some(input) = input {
        assert!(input.len() <= 16 * 1024, "native input bound");
        let mut file = tempfile::tempfile_in(root.path()).expect("private native input");
        file.write_all(input).expect("synthetic native input");
        file.rewind().expect("input rewind");
        Stdio::from(file)
    } else {
        Stdio::null()
    };
    let mut stdout = tempfile::tempfile_in(root.path()).expect("private native stdout");
    let mut stderr = tempfile::tempfile_in(root.path()).expect("private native stderr");
    let mut child = OwnedChild(Some(
        command
            .stdin(stdin)
            .stdout(stdout.try_clone().expect("stdout handle"))
            .stderr(stderr.try_clone().expect("stderr handle"))
            .spawn()
            .expect("native process"),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child
            .0
            .as_mut()
            .expect("native child")
            .try_wait()
            .expect("native status")
        {
            break status;
        }
        assert!(Instant::now() < deadline, "native parent deadline");
        assert!(
            stdout.metadata().expect("stdout size").len() <= 64 * 1024
                && stderr.metadata().expect("stderr size").len() <= 64 * 1024,
            "native output bound"
        );
        std::thread::yield_now();
    };
    child
        .0
        .take()
        .expect("native child")
        .wait()
        .expect("native child reaped");
    let mut output = Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    for (file, bytes) in [
        (&mut stdout, &mut output.stdout),
        (&mut stderr, &mut output.stderr),
    ] {
        file.seek(SeekFrom::Start(0)).expect("output rewind");
        file.take(64 * 1024 + 1)
            .read_to_end(bytes)
            .expect("bounded native output");
        assert!(bytes.len() <= 64 * 1024, "native observation bound");
    }
    output
}

pub(crate) async fn probe_chatgpt_keyring() -> Result<(), CredentialError> {
    tokio::task::spawn_blocking(keyring_helper::probe_chatgpt_backend)
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
    workspace: &Path,
    id: Uuid,
    provider: &str,
) -> Result<Option<String>, CredentialError> {
    migrate_legacy(workspace).await?;
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
const MIGRATED_ACCOUNT: &[u8] = b"{\"schema\":0,\"migrated\":\"os_user\"}";

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
    schema: u8,
    storage: AccountStorage,
    account: Option<SavedAccount>,
}

impl StoredAccountFile {
    fn parse(record: &[u8]) -> Result<Self, CredentialError> {
        let stored: Self =
            serde_json::from_slice(record).map_err(|_| CredentialError::InvalidAccount)?;
        if stored.schema != 1
            || matches!(
                (&stored.storage, &stored.account),
                (AccountStorage::Keyring, Some(_)) | (AccountStorage::PrivateFile, None)
            )
        {
            return Err(CredentialError::InvalidAccount);
        }
        if let Some(account) = &stored.account {
            account.validate()?;
        }
        Ok(stored)
    }

    fn encoded(&self) -> Result<Vec<u8>, CredentialError> {
        let record = serde_json::to_vec(self).map_err(|_| CredentialError::InvalidAccount)?;
        if record.len() > StateRoot::MAX_ACCOUNT_RECORD_BYTES {
            return Err(CredentialError::InvalidAccount);
        }
        Ok(record)
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub(crate) enum CredentialError {
    #[error("invalid saved account")]
    InvalidAccount,
    #[error("OS credential store unavailable")]
    Unavailable,
    #[error("OS credential store did not respond before its deadline")]
    TimedOut,
    #[error(
        "OS credential store write timed out; the saved account may have changed. Check it before retrying"
    )]
    WriteOutcomeUnknown,
    #[error("OS credential store deletion timed out; the ChatGPT token may remain stored")]
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
    #[error("saved accounts in different state roots conflict; resolve them before continuing")]
    AccountConflict,
    #[error("private account state unavailable")]
    StateUnavailable,
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
    schema: u8,
    pub id: Uuid,
    pub provider: String,
    pub model: String,
    pub effort: Option<Effort>,
    api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
            schema: 1,
            id: Uuid::now_v7(),
            provider,
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
        self.schema = if workspace_id.is_some() { 2 } else { 1 };
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
            &self.provider,
            self.api_key.clone(),
            self.anthropic_workspace_id.clone(),
        )
        .map_err(|_| CredentialError::InvalidAccount)
    }

    pub(crate) fn into_credentials(self) -> Result<NativeApiCredentials, CredentialError> {
        self.validate()?;
        NativeApiCredentials::new(&self.provider, self.api_key, self.anthropic_workspace_id)
            .map_err(|_| CredentialError::InvalidAccount)
    }

    fn validate(&self) -> Result<(), CredentialError> {
        if !matches!(
            (self.schema, self.anthropic_workspace_id.is_some()),
            (1, false) | (2, true)
        ) || self.id.get_version() != Some(Version::SortRand)
            || !matches!(self.provider.as_str(), "openai" | "anthropic")
            || (resolve_native_effort(&self.provider, &self.model, self.effort).is_err()
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
) -> Result<(), CredentialError> {
    migrate_legacy(workspace).await?;
    let account_root = StateRoot::account_path().map_err(|_| CredentialError::StateUnavailable)?;
    match storage {
        AccountStorage::Keyring => {
            save_at(DEFAULT_ACCOUNT, &account_root, workspace, account).await
        }
        AccountStorage::PrivateFile => save_file_at(&account_root, workspace, account).await,
    }
}

async fn save_at(
    slot: &str,
    state_dir: &Path,
    workspace: &Path,
    account: SavedAccount,
) -> Result<(), CredentialError> {
    account.validate()?;
    admit_transport()?;
    let record = serde_json::to_string(&account).map_err(|_| CredentialError::InvalidAccount)?;
    if record.len() > StateRoot::MAX_ACCOUNT_RECORD_BYTES {
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
                    .read_saved_account_record()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                if existing
                    .as_deref()
                    .map(StoredAccountFile::parse)
                    .transpose()?
                    .is_some_and(|stored| stored.storage == AccountStorage::PrivateFile)
                {
                    return Err(CredentialError::InvalidAccount);
                }
                let marker = StoredAccountFile {
                    schema: 1,
                    storage: AccountStorage::Keyring,
                    account: None,
                }
                .encoded()?;
                state
                    .replace_saved_account_record(&marker)
                    .map_err(|_| CredentialError::StateUnavailable)?;
                keyring_helper::write(&slot, &record)
            })
            .map_err(|error| match error {
                StoreError::AccountBusy => CredentialError::AccountBusy,
                _ => CredentialError::StateUnavailable,
            })?
    })
    .await
    .map_err(|_| CredentialError::Unavailable)?
}

async fn save_file_at(
    account_root: &Path,
    workspace: &Path,
    account: SavedAccount,
) -> Result<(), CredentialError> {
    account.validate()?;
    let record = StoredAccountFile {
        schema: 1,
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
                    .read_saved_account_record()
                    .map_err(|_| CredentialError::StateUnavailable)?;
                if existing
                    .as_deref()
                    .map(StoredAccountFile::parse)
                    .transpose()?
                    .is_some_and(|stored| stored.storage == AccountStorage::Keyring)
                {
                    return Err(CredentialError::InvalidAccount);
                }
                state
                    .replace_saved_account_record(&record)
                    .map_err(|_| CredentialError::StateUnavailable)
            })
            .map_err(|error| match error {
                StoreError::AccountBusy => CredentialError::AccountBusy,
                _ => CredentialError::StateUnavailable,
            })?
    })
    .await
    .map_err(|_| CredentialError::StateUnavailable)?
}

async fn migrate_legacy(workspace: &Path) -> Result<(), CredentialError> {
    let account_root = StateRoot::account_path().map_err(|_| CredentialError::StateUnavailable)?;
    let old_root = StateRoot::default_path().map_err(|_| CredentialError::StateUnavailable)?;
    let workspace = workspace.to_path_buf();
    tokio::task::spawn_blocking(move || migrate_legacy_at(&account_root, &old_root, &workspace))
        .await
        .map_err(|_| CredentialError::StateUnavailable)?
}

fn migrate_legacy_at(
    account_path: &Path,
    old_path: &Path,
    workspace: &Path,
) -> Result<(), CredentialError> {
    if account_path == old_path {
        return Ok(());
    }
    let Some(old) =
        super::open_optional_state(old_path).map_err(|_| CredentialError::StateUnavailable)?
    else {
        return Ok(());
    };
    old.with_account_replacement_lock(workspace, || {
        let record = old
            .read_saved_account_record()
            .map_err(|_| CredentialError::StateUnavailable)?;
        let Some(record) = record else {
            return Ok(());
        };
        if record == MIGRATED_ACCOUNT {
            return Ok(());
        }
        StoredAccountFile::parse(&record)?;
        let current =
            StateRoot::admit(account_path).map_err(|_| CredentialError::StateUnavailable)?;
        current
            .with_account_replacement_lock(workspace, || {
                match current
                    .read_saved_account_record()
                    .map_err(|_| CredentialError::StateUnavailable)?
                {
                    Some(existing) if existing != record => {
                        return Err(CredentialError::AccountConflict);
                    }
                    Some(_) => {}
                    None => current
                        .replace_saved_account_record(&record)
                        .map_err(|_| CredentialError::StateUnavailable)?,
                }
                old.replace_saved_account_record(MIGRATED_ACCOUNT)
                    .map_err(|_| CredentialError::StateUnavailable)
            })
            .map_err(account_lock_error)?
    })
    .map_err(account_lock_error)?
}

fn account_lock_error(error: StoreError) -> CredentialError {
    match error {
        StoreError::AccountBusy => CredentialError::AccountBusy,
        _ => CredentialError::StateUnavailable,
    }
}

pub(crate) async fn inspect(workspace: &Path) -> Result<InspectedAccount, CredentialError> {
    inspect_with_interaction(workspace, false).await
}

pub(crate) async fn inspect_interactive(
    workspace: &Path,
) -> Result<InspectedAccount, CredentialError> {
    inspect_with_interaction(workspace, true).await
}

async fn inspect_with_interaction(
    workspace: &Path,
    interactive: bool,
) -> Result<InspectedAccount, CredentialError> {
    migrate_legacy(workspace).await?;
    let file = tokio::task::spawn_blocking(load_file)
        .await
        .map_err(|_| CredentialError::StateUnavailable)??;
    let pinned_keyring = file
        .as_ref()
        .is_some_and(|stored| stored.storage == AccountStorage::Keyring);
    if let Some(stored) = file {
        match stored.storage {
            AccountStorage::PrivateFile => {
                return Ok(InspectedAccount {
                    account: stored.account,
                    storage: AccountStorage::PrivateFile,
                });
            }
            AccountStorage::Keyring => {}
        }
    }
    let account = if interactive {
        keyring_helper::read_interactive(DEFAULT_ACCOUNT)
            .await
            .and_then(parse_account)
    } else {
        load_at(DEFAULT_ACCOUNT).await
    }
    .map_err(|error| {
        if pinned_keyring
            && matches!(
                error,
                CredentialError::Unavailable | CredentialError::TimedOut
            )
        {
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
        super::open_optional_state(path).map_err(|_| CredentialError::StateUnavailable)?
    else {
        return Ok(None);
    };
    let Some(record) = state
        .read_saved_account_record()
        .map_err(|_| CredentialError::StateUnavailable)?
    else {
        return Ok(None);
    };
    Ok(Some(StoredAccountFile::parse(&record)?))
}

pub(crate) async fn load(workspace: &Path) -> Result<Option<SavedAccount>, CredentialError> {
    Ok(inspect(workspace).await?.account)
}

async fn load_at(slot: &str) -> Result<Option<SavedAccount>, CredentialError> {
    admit_transport()?;
    let slot = slot.to_owned();
    tokio::task::spawn_blocking(move || parse_account(keyring_helper::read(&slot)?))
        .await
        .map_err(|_| CredentialError::Unavailable)?
}

fn parse_account(record: Option<Vec<u8>>) -> Result<Option<SavedAccount>, CredentialError> {
    record
        .map(|record| {
            let account: SavedAccount =
                serde_json::from_slice(&record).map_err(|_| CredentialError::InvalidAccount)?;
            account.validate()?;
            Ok(account)
        })
        .transpose()
}

pub(crate) async fn load_selected(
    workspace: &Path,
    id: Uuid,
    provider: &str,
) -> Result<NativeApiCredentials, CredentialError> {
    let account = load(workspace)
        .await?
        .ok_or(CredentialError::InvalidAccount)?;
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
    if account.id != id || account.provider != provider {
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
    #[cfg(target_os = "linux")]
    use std::process::Command;

    struct RemoveTestAccount(String);

    impl Drop for RemoveTestAccount {
        fn drop(&mut self) {
            if let Ok(entry) = Entry::new(SERVICE, &self.0) {
                let _ = entry.delete_credential();
            }
        }
    }

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
            let result = save_at(&slot, &account_root, &workspace, replacement).await;
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
        save_at(&slot, &state_dir, &workspace, account)
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
                .read_saved_account_record()
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
                        "cli::credentials::tests::native_store_round_trips_an_isolated_synthetic_account",
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
                let output = native_test_output(&mut command, None);
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
                state.read_saved_account_record().unwrap(),
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
            assert_eq!(state.read_saved_account_record().unwrap(), Some(marker));
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
        save_at(&slot, &temp.path().join("state"), &workspace, account)
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
        let output = native_test_output(&mut client, None);
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
        assert_eq!(record["schema"], 1);
        assert!(record.get("anthropic_workspace_id").is_none());
        record["schema"] = 2.into();
        let malformed: SavedAccount = serde_json::from_value(record).expect("parse schema");
        assert_eq!(malformed.validate(), Err(CredentialError::InvalidAccount));
        let mut scoped = serde_json::json!({
            "schema": 2,
            "id": Uuid::now_v7(),
            "provider": "anthropic",
            "model": "claude-sonnet-5",
            "effort": null,
            "api_key": "synthetic-key",
            "anthropic_workspace_id": "wrkspc_Test123"
        });
        assert!(
            serde_json::from_value::<SavedAccount>(scoped.clone())
                .is_ok_and(|account| account.validate().is_ok()),
            "a valid versioned workspace account must be admitted"
        );
        for (field, value) in [
            ("schema", serde_json::json!(1)),
            ("schema", serde_json::json!(3)),
            ("anthropic_workspace_id", serde_json::Value::Null),
            ("anthropic_workspace_id", serde_json::json!("")),
            ("anthropic_workspace_id", serde_json::json!("wrkspc_")),
            (
                "anthropic_workspace_id",
                serde_json::json!("wrkspc_wrong-id"),
            ),
            ("anthropic_workspace_id", serde_json::json!("wrkspc_\n")),
            ("anthropic_workspace_id", serde_json::json!("w".repeat(129))),
            ("provider", serde_json::json!("openai")),
        ] {
            let original = scoped[field].clone();
            scoped[field] = value;
            assert!(
                !serde_json::from_value::<SavedAccount>(scoped.clone())
                    .is_ok_and(|account| account.validate().is_ok()),
                "invalid workspace/version/provider combination was admitted"
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
                schema: 1,
                storage: AccountStorage::PrivateFile,
                account: Some(account),
            }
            .encoded();
            if fits {
                assert!(encoded.is_ok_and(|record| {
                    record.len() <= StateRoot::MAX_ACCOUNT_RECORD_BYTES
                        && StoredAccountFile::parse(&record).is_ok()
                }));
            } else {
                assert!(matches!(encoded, Err(CredentialError::InvalidAccount)));
            }
        }
        assert_eq!(
            CredentialError::InvalidAccount.to_string(),
            "invalid saved account"
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
        save_file_at(&account_root, &workspace, account)
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
        assert_eq!(
            save_at("isolated-never-used", &account_root, &workspace, loaded).await,
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
        save_file_at(&account_root, &workspace, scoped)
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
            schema: 1,
            storage: AccountStorage::Keyring,
            account: None,
        }
        .encoded()
        .expect("keyring marker");
        state
            .replace_saved_account_record(&marker)
            .expect("pin keyring backend");
        let replacement = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            None,
            "synthetic-replacement".into(),
        )
        .expect("replacement account");
        assert_eq!(
            save_file_at(&account_root, &workspace, replacement).await,
            Err(CredentialError::InvalidAccount)
        );
        let stored = load_file_at(&account_root)
            .expect("pinned backend")
            .expect("marker");
        assert_eq!(stored.storage, AccountStorage::Keyring);
        assert!(stored.account.is_none());
    }

    #[tokio::test]
    async fn legacy_account_migration_preserves_the_selected_record_and_rejects_conflicts() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let legacy_path = temp.path().join("legacy");
        let account_path = temp.path().join("user-account");
        let account = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-migration-key".into(),
        )
        .expect("synthetic account");
        let id = account.id;
        save_file_at(&legacy_path, &workspace, account)
            .await
            .expect("legacy account");
        migrate_legacy_at(&account_path, &legacy_path, &workspace).expect("migrate account");
        let migrated = load_file_at(&account_path)
            .expect("admitted account root")
            .expect("account record")
            .account
            .expect("file account");
        assert_eq!(migrated.id, id);
        assert_eq!(
            selected_key(&migrated, id, "openai"),
            Ok("synthetic-migration-key".into())
        );
        let legacy = StateRoot::open_existing(&legacy_path).expect("legacy root");
        assert_eq!(
            legacy.read_saved_account_record().expect("legacy marker"),
            Some(MIGRATED_ACCOUNT.to_vec())
        );

        let replacement = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            Some(Effort::Low),
            "synthetic-replacement-key".into(),
        )
        .expect("replacement");
        let replacement_id = replacement.id;
        save_file_at(&account_path, &workspace, replacement)
            .await
            .expect("replace selected account");
        migrate_legacy_at(&account_path, &legacy_path, &workspace).expect("old marker is inert");
        let selected = load_file_at(&account_path)
            .expect("current account")
            .expect("record")
            .account
            .expect("file account");
        assert_eq!(selected.id, replacement_id);

        let conflicting_path = temp.path().join("conflicting-legacy");
        let other = SavedAccount::new(
            "openai".into(),
            "gpt-5.4".into(),
            None,
            "synthetic-other-key".into(),
        )
        .expect("other account");
        save_file_at(&conflicting_path, &workspace, other)
            .await
            .expect("conflicting root");
        assert_eq!(
            migrate_legacy_at(&account_path, &conflicting_path, &workspace),
            Err(CredentialError::AccountConflict)
        );
        assert!(
            load_file_at(&conflicting_path)
                .expect("old account")
                .is_some()
        );
        assert_eq!(
            load_file_at(&account_path)
                .expect("selected account")
                .expect("record")
                .account
                .expect("file account")
                .id,
            replacement_id
        );
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
