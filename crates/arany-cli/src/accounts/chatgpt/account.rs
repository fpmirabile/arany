use super::{
    AuthorizationError, RiskAcknowledgment, VerifiedCredentials, consent::ConsentReceipt,
    registration, valid_client_id,
};
use crate::accounts::credentials::{
    AccountStorage, delete_chatgpt_keyring_record, read_chatgpt_keyring_record,
    write_chatgpt_keyring_record,
};
use arany::{Effort, StateRoot, validate_native_model_id};
use arany::{RecordFormat, RecordKind};
use aws_lc_rs::hmac;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::{Uuid, Variant, Version};

const MAX_ACCOUNTS: usize = 8;
const REFRESH_EARLY_SECONDS: u64 = 300;
pub(crate) const MAX_TOKEN_RECORD_BYTES: usize = 64 * 1024;

fn account_keyring_slot(client_id: &str, id: Uuid) -> String {
    let digest = Sha256::new()
        .chain_update(client_id.as_bytes())
        .chain_update([0])
        .chain_update(id.as_bytes())
        .finalize();
    format!("chatgpt-{}", URL_SAFE_NO_PAD.encode(digest))
}

fn decode_keyring_record(bytes: &[u8]) -> Option<TokenRecord> {
    if bytes.len() > MAX_TOKEN_RECORD_BYTES {
        return None;
    }
    serde_json::from_slice::<TokenRecord>(bytes).ok()
}

pub(crate) fn valid_keyring_record(slot: &str, bytes: &[u8]) -> bool {
    decode_keyring_record(bytes).is_some_and(|record| {
        record.valid_for(AccountStorage::Keyring) && record.slot.as_deref() == Some(slot)
    })
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TokenRecord {
    #[serde(deserialize_with = "Option::deserialize")]
    slot: Option<String>,
    credentials: VerifiedCredentials,
    consent: ConsentReceipt,
}

#[derive(Clone)]
pub(super) struct SelectedAccount {
    pub(super) id: Uuid,
    pub(super) credentials: VerifiedCredentials,
    pub(super) consent: ConsentReceipt,
    pub(super) storage: AccountStorage,
}

#[derive(Clone, Copy)]
struct SignedIn<'a> {
    client_id: &'a str,
    subject: &'a str,
    host_id: Uuid,
    account_id: &'a str,
}

impl SignedIn<'_> {
    fn unchanged_in(self, entry: &AccountEntry, storage: AccountStorage) -> bool {
        entry.subject == self.subject
            && entry.host_id == self.host_id
            && entry.storage == storage
            && entry.chatgpt_account_id == self.account_id
    }
}

pub(crate) struct ReauthorizationTarget {
    pub(crate) id: Uuid,
    pub(crate) host_id: Uuid,
    pub(crate) client_id: String,
    pub(crate) subject: String,
    pub(crate) storage: AccountStorage,
    pub(crate) chatgpt_account_id: String,
}

pub(crate) struct AccountInspection {
    pub(crate) id: Uuid,
    pub(crate) storage: AccountStorage,
    pub(crate) access: AccountAccess,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum AccountAccess {
    Connected,
    Disconnected,
    RenewalPending,
    SignOutPending,
}

pub(crate) struct SignOutOutcome {
    pub(crate) remote_confirmed: bool,
    pub(crate) local_cleared: bool,
}

struct SignOutTarget<'a> {
    account_id: Option<Uuid>,
    expected_index: Option<&'a [u8]>,
}

struct SignOutActions<R, V, D> {
    read_keyring: R,
    revoke: Option<V>,
    delete_keyring: D,
}

impl SelectedAccount {
    fn from_token(id: Uuid, storage: AccountStorage, token: TokenRecord) -> Self {
        Self {
            id,
            credentials: token.credentials,
            consent: token.consent,
            storage,
        }
    }
}

impl TokenRecord {
    fn content_valid(&self, storage: AccountStorage) -> bool {
        self.credentials.valid_saved() && self.consent.matches(&self.credentials, storage)
    }

    fn valid_for(&self, storage: AccountStorage) -> bool {
        self.content_valid(storage) && self.slot.is_some() == (storage == AccountStorage::Keyring)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AccountEntry {
    id: Uuid,
    host_id: Uuid,
    client_id: String,
    subject: String,
    chatgpt_account_id: String,
    #[serde(deserialize_with = "Option::deserialize")]
    keyring_slot: Option<String>,
    storage: AccountStorage,
    renewal_pending: bool,
    signout_pending: bool,
    disconnected: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    token: Option<TokenRecord>,
}

impl AccountEntry {
    fn ensure_usable(&self) -> Result<(), AuthorizationError> {
        if self.disconnected {
            return Err(AuthorizationError::NoSelectedAccount);
        }
        if self.signout_pending {
            return Err(AuthorizationError::SignOutPending);
        }
        if self.renewal_pending {
            return Err(AuthorizationError::RenewalStorageUncertain);
        }
        Ok(())
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.client_id == other.client_id
            && self.subject == other.subject
            && self.chatgpt_account_id == other.chatgpt_account_id
    }

    fn slot_valid(&self) -> bool {
        match (self.storage, &self.keyring_slot) {
            (AccountStorage::Keyring, Some(slot)) => {
                *slot == account_keyring_slot(&self.client_id, self.id)
            }
            (AccountStorage::PrivateFile, None) => true,
            _ => false,
        }
    }

    fn valid(&self) -> bool {
        self.id.get_version() == Some(Version::SortRand)
            && self.host_id.get_version() == Some(Version::Random)
            && self.host_id.get_variant() == Variant::RFC4122
            && valid_client_id(&self.client_id)
            && !self.subject.is_empty()
            && self.subject.len() <= 512
            && self.subject.bytes().all(|byte| byte.is_ascii_graphic())
            && arany::ChatGptProvider::valid_account_id(&self.chatgpt_account_id)
            && self.slot_valid()
            && !(self.signout_pending && self.renewal_pending)
            && match (&self.storage, &self.token) {
                (AccountStorage::PrivateFile, Some(token)) => {
                    token.slot.is_none()
                        && token.credentials.valid_saved()
                        && token.credentials.client_id == self.client_id
                        && token.credentials.host_id == self.host_id
                        && token.credentials.subject == self.subject
                        && token.credentials.chatgpt_account_id.as_ref()
                            == Some(&self.chatgpt_account_id)
                }
                (AccountStorage::PrivateFile, None) => self.disconnected,
                (AccountStorage::Keyring, None) => true,
                _ => false,
            }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AccountIndex {
    #[serde(deserialize_with = "Option::deserialize")]
    selected: Option<Uuid>,
    accounts: Vec<AccountEntry>,
}

pub(crate) const CHATGPT_ACCOUNTS: RecordFormat = RecordFormat {
    kind: RecordKind::ChatgptAccounts,
    accepts: |bytes| AccountIndex::decode(bytes).is_ok(),
};

impl AccountIndex {
    fn account_position(&self, id: Uuid) -> Result<usize, AuthorizationError> {
        self.accounts
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(AuthorizationError::InvalidIdentity)
    }

    fn signed_in_position(
        &self,
        who: SignedIn<'_>,
        storage: AccountStorage,
        expected_selected: Option<Uuid>,
    ) -> Result<Option<usize>, AuthorizationError> {
        let position = match expected_selected {
            Some(id) => {
                if self.selected != Some(id) {
                    return Err(AuthorizationError::SelectedAccountChanged);
                }
                let position = self.account_position(id)?;
                if self.accounts[position].client_id != who.client_id {
                    return Err(AuthorizationError::SelectedAccountChanged);
                }
                Some(position)
            }
            None => self.accounts.iter().position(|entry| {
                entry.client_id == who.client_id
                    && entry.subject == who.subject
                    && entry.chatgpt_account_id == who.account_id
            }),
        };
        let Some(position) = position else {
            return Ok(None);
        };
        let entry = &self.accounts[position];
        if !who.unchanged_in(entry, storage)
            || self.accounts.iter().any(|other| {
                other.id != entry.id
                    && other.client_id == who.client_id
                    && other.subject == who.subject
                    && other.chatgpt_account_id == who.account_id
            })
        {
            return Err(AuthorizationError::RegistrationConflict);
        }
        Ok(Some(position))
    }

    fn ensure_account_capacity(&self) -> Result<(), AuthorizationError> {
        if self.accounts.len() == MAX_ACCOUNTS {
            return Err(AuthorizationError::Unavailable);
        }
        Ok(())
    }

    fn push_account(&mut self, entry: AccountEntry) -> Uuid {
        let id = entry.id;
        self.accounts.push(entry);
        id
    }

    fn select_account(&mut self, id: Uuid) {
        self.selected = Some(id);
    }

    fn empty() -> Self {
        Self {
            selected: None,
            accounts: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), AuthorizationError> {
        if self.accounts.len() > MAX_ACCOUNTS
            || self
                .selected
                .is_some_and(|id| !self.accounts.iter().any(|entry| entry.id == id))
            || self.accounts.iter().any(|entry| !entry.valid())
            || self
                .accounts
                .iter()
                .any(|entry| entry.host_id != self.accounts[0].host_id)
        {
            return Err(AuthorizationError::InvalidIdentity);
        }
        for (index, account) in self.accounts.iter().enumerate() {
            if self.accounts[..index].iter().any(|prior| {
                prior.id == account.id
                    || prior.same_identity(account)
                    || (prior.keyring_slot.is_some() && prior.keyring_slot == account.keyring_slot)
            }) {
                return Err(AuthorizationError::InvalidIdentity);
            }
        }
        Ok(())
    }

    fn read(state: &StateRoot) -> Result<Self, AuthorizationError> {
        let Some(bytes) = state
            .record(CHATGPT_ACCOUNTS.kind)
            .read()
            .map_err(|_| AuthorizationError::Unavailable)?
        else {
            return Ok(Self::empty());
        };
        Self::decode(&bytes)
    }

    fn decode(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        let index: Self =
            serde_json::from_slice(bytes).map_err(|_| AuthorizationError::InvalidSavedState)?;
        index
            .validate()
            .map_err(|_| AuthorizationError::InvalidSavedState)?;
        Ok(index)
    }

    fn write(&self, state: &StateRoot) -> Result<(), AuthorizationError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| AuthorizationError::Unavailable)?;
        state
            .record(RecordKind::ChatgptAccounts)
            .replace(&bytes)
            .map_err(super::publication_error)
    }
}

fn account_fingerprint(
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
) -> Result<[u8; 32], AuthorizationError> {
    validate_native_model_id(model).map_err(|_| AuthorizationError::InvalidSelection)?;
    if !selected
        .consent
        .matches(&selected.credentials, selected.storage)
    {
        return Err(AuthorizationError::ConsentRequired);
    }
    let mut message = Vec::with_capacity(256);
    for field in [
        arany::ChatGptProvider::RESPONSES_ENDPOINT,
        "codex-backend-v1;store=false;stream=true",
        &selected.credentials.client_id,
        &selected.credentials.subject,
        selected
            .credentials
            .chatgpt_account_id
            .as_deref()
            .ok_or(AuthorizationError::InvalidIdentity)?,
        model,
        effort.as_str(),
    ] {
        let length = u64::try_from(field.len()).map_err(|_| AuthorizationError::InvalidIdentity)?;
        message.extend_from_slice(&length.to_be_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    let consent = selected.consent.fingerprint_bytes()?;
    let length = u64::try_from(consent.len()).map_err(|_| AuthorizationError::InvalidIdentity)?;
    message.extend_from_slice(&length.to_be_bytes());
    message.extend_from_slice(&consent);
    message.extend_from_slice(selected.id.as_bytes());
    message.extend_from_slice(selected.credentials.host_id.as_bytes());
    let key = hmac::Key::new(hmac::HMAC_SHA256, selected.credentials.id_token.as_bytes());
    let tag = hmac::sign(&key, &message);
    tag.as_ref()
        .try_into()
        .map_err(|_| AuthorizationError::Unavailable)
}

fn load_selected_snapshot(
    index: &AccountIndex,
    selected: &SelectedAccount,
) -> Result<TokenRecord, AuthorizationError> {
    if index.selected != Some(selected.id) {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let entry = index
        .accounts
        .iter()
        .find(|entry| entry.id == selected.id)
        .ok_or(AuthorizationError::InvalidIdentity)?;
    entry.ensure_usable()?;
    if entry.storage != selected.storage
        || entry.host_id != selected.credentials.host_id
        || entry.client_id != selected.credentials.client_id
        || selected.credentials.chatgpt_account_id.as_ref() != Some(&entry.chatgpt_account_id)
        || entry.subject != selected.credentials.subject
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let token = match entry.storage {
        AccountStorage::Keyring => read_keyring_token(entry)?,
        AccountStorage::PrivateFile => entry
            .token
            .clone()
            .ok_or(AuthorizationError::InvalidIdentity)?,
    };
    if !token.valid_for(entry.storage) {
        return Err(AuthorizationError::ConsentRequired);
    }
    if token.credentials.id_token != selected.credentials.id_token
        || token.consent != selected.consent
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    Ok(token)
}

fn verify_selected_snapshot(
    index: &AccountIndex,
    selected: &SelectedAccount,
) -> Result<(), AuthorizationError> {
    load_selected_snapshot(index, selected).map(|_| ())
}

pub(super) struct AdmittedModel {
    pub(super) selected: SelectedAccount,
    pub(super) fingerprint: [u8; 32],
}

pub(super) async fn admitted_selected_for_run(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
    model: String,
    effort: Effort,
) -> Result<AdmittedModel, AuthorizationError> {
    let selected = selected_for_use(workspace.clone(), expected_account_id).await?;
    let admitted = selected.clone();
    let fingerprint = tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        admitted_model_at(&state, &workspace, &admitted, (&model, effort))
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)??;
    Ok(AdmittedModel {
        selected,
        fingerprint,
    })
}

fn admitted_model_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    (slug, effort): (&str, Effort),
) -> Result<[u8; 32], AuthorizationError> {
    let fingerprint = account_fingerprint(selected, slug, effort)?;
    state
        .with_account_replacement_lock(workspace, || {
            let index = AccountIndex::read(state)?;
            verify_selected_snapshot(&index, selected)?;
            Ok(fingerprint)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) fn saved_host_at(state: &StateRoot) -> Result<Option<Uuid>, AuthorizationError> {
    Ok(AccountIndex::read(state)?
        .accounts
        .first()
        .map(|entry| entry.host_id))
}

pub(crate) async fn save_verified(
    workspace: PathBuf,
    credentials: VerifiedCredentials,
    acknowledgment: RiskAcknowledgment,
    expected_selected: Option<Uuid>,
) -> Result<Uuid, AuthorizationError> {
    if credentials.client_id != super::CLIENT_ID || credentials.chatgpt_account_id.is_none() {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let storage = acknowledgment.storage();
    let consent = acknowledgment.bind(&credentials)?;
    tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::admit(&path).map_err(|_| AuthorizationError::Unavailable)?;
        match storage {
            AccountStorage::Keyring => {
                save_keyring_at(&state, &workspace, credentials, consent, expected_selected)
            }
            AccountStorage::PrivateFile => {
                save_private_file_at(&state, &workspace, credentials, consent, expected_selected)
            }
        }
    })
    .await
    .map_err(|_| {
        AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown,
        )
    })?
}

pub(super) fn save_private_file_at(
    state: &StateRoot,
    workspace: &Path,
    credentials: VerifiedCredentials,
    consent: ConsentReceipt,
    expected_selected: Option<Uuid>,
) -> Result<Uuid, AuthorizationError> {
    let token = TokenRecord {
        slot: None,
        credentials,
        consent,
    };
    if !token.valid_for(AccountStorage::PrivateFile) {
        return Err(AuthorizationError::ConsentRequired);
    }
    let host_id = token.credentials.host_id;
    let account_id = token
        .credentials
        .chatgpt_account_id
        .clone()
        .ok_or(AuthorizationError::InvalidIdentity)?;
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            registration::verify_host_at(state, host_id)?;
            if index
                .accounts
                .first()
                .is_some_and(|entry| entry.host_id != host_id)
            {
                return Err(AuthorizationError::RegistrationConflict);
            }
            let client_id = &token.credentials.client_id;
            let subject = &token.credentials.subject;
            let position = index.signed_in_position(
                SignedIn {
                    client_id,
                    subject,
                    host_id,
                    account_id: &account_id,
                },
                AccountStorage::PrivateFile,
                expected_selected,
            )?;
            let id = if let Some(position) = position {
                let entry = &mut index.accounts[position];
                entry.token = Some(token);
                entry.renewal_pending = false;
                entry.signout_pending = false;
                entry.disconnected = false;
                entry.id
            } else {
                index.ensure_account_capacity()?;
                index.push_account(AccountEntry {
                    id: Uuid::now_v7(),
                    host_id,
                    client_id: client_id.clone(),
                    subject: subject.clone(),
                    chatgpt_account_id: account_id.clone(),
                    keyring_slot: None,
                    storage: AccountStorage::PrivateFile,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: false,
                    token: Some(token),
                })
            };
            index.select_account(id);
            index.write(state)?;
            Ok(id)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) fn save_keyring_at(
    state: &StateRoot,
    workspace: &Path,
    credentials: VerifiedCredentials,
    consent: ConsentReceipt,
    expected_selected: Option<Uuid>,
) -> Result<Uuid, AuthorizationError> {
    save_keyring_at_with(
        state,
        workspace,
        credentials,
        consent,
        expected_selected,
        |slot, record| {
            write_chatgpt_keyring_record(slot, record).map_err(AuthorizationError::CredentialStore)
        },
    )
}

fn save_keyring_at_with(
    state: &StateRoot,
    workspace: &Path,
    credentials: VerifiedCredentials,
    consent: ConsentReceipt,
    expected_selected: Option<Uuid>,
    write: impl FnOnce(&str, &str) -> Result<(), AuthorizationError>,
) -> Result<Uuid, AuthorizationError> {
    let mut token = TokenRecord {
        slot: None,
        credentials,
        consent,
    };
    if !token.content_valid(AccountStorage::Keyring) {
        return Err(AuthorizationError::ConsentRequired);
    }
    let account_id = token
        .credentials
        .chatgpt_account_id
        .clone()
        .ok_or(AuthorizationError::InvalidIdentity)?;
    state
        .with_account_replacement_lock(workspace, || {
            publish_keyring_account(state, &mut token, &account_id, expected_selected, write)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

// Runs under the account replacement lock.
fn publish_keyring_account(
    state: &StateRoot,
    token: &mut TokenRecord,
    account_id: &str,
    expected_selected: Option<Uuid>,
    write: impl FnOnce(&str, &str) -> Result<(), AuthorizationError>,
) -> Result<Uuid, AuthorizationError> {
    let host_id = token.credentials.host_id;
    let mut index = AccountIndex::read(state)?;
    registration::verify_host_at(state, host_id)?;
    if index
        .accounts
        .first()
        .is_some_and(|entry| entry.host_id != host_id)
    {
        return Err(AuthorizationError::RegistrationConflict);
    }
    let client_id = token.credentials.client_id.clone();
    let subject = token.credentials.subject.clone();
    let position = index.signed_in_position(
        SignedIn {
            client_id: &client_id,
            subject: &subject,
            host_id,
            account_id,
        },
        AccountStorage::Keyring,
        expected_selected,
    )?;
    let (id, slot, record) = prepare_keyring_record(&index, position, token)?;
    if let Some(position) = position {
        index.accounts[position].signout_pending = false;
        index.accounts[position].renewal_pending = true;
        index.write(state)?;
    } else {
        index.push_account(AccountEntry {
            id,
            host_id,
            client_id,
            subject,
            chatgpt_account_id: account_id.to_owned(),
            keyring_slot: Some(slot.clone()),
            storage: AccountStorage::Keyring,
            renewal_pending: false,
            signout_pending: false,
            disconnected: false,
            token: None,
        });
    }
    write(&slot, &record).map_err(|error| {
        if position.is_some() {
            uncertain_account_publication(error)
        } else {
            error
        }
    })?;
    let entry = index
        .accounts
        .iter_mut()
        .find(|entry| entry.id == id)
        .ok_or_else(|| uncertain_account_publication(AuthorizationError::InvalidIdentity))?;
    entry.renewal_pending = false;
    entry.signout_pending = false;
    entry.disconnected = false;
    index.select_account(id);
    index.write(state).map_err(uncertain_account_publication)?;
    Ok(id)
}

fn prepare_keyring_record(
    index: &AccountIndex,
    position: Option<usize>,
    token: &mut TokenRecord,
) -> Result<(Uuid, String, String), AuthorizationError> {
    let (id, slot) = match position {
        Some(position) => {
            let entry = &index.accounts[position];
            let slot = entry
                .keyring_slot
                .clone()
                .ok_or(AuthorizationError::InvalidIdentity)?;
            (entry.id, slot)
        }
        None => {
            index.ensure_account_capacity()?;
            let id = Uuid::now_v7();
            (id, account_keyring_slot(&token.credentials.client_id, id))
        }
    };
    token.slot = Some(slot.clone());
    let record = serde_json::to_string(token).map_err(|_| AuthorizationError::Unavailable)?;
    if record.len() > MAX_TOKEN_RECORD_BYTES {
        return Err(AuthorizationError::InvalidIdentity);
    }
    Ok((id, slot, record))
}

fn uncertain_account_publication(_: AuthorizationError) -> AuthorizationError {
    AuthorizationError::CredentialStore(
        crate::accounts::credentials::CredentialError::WriteOutcomeUnknown,
    )
}

pub(super) fn load_selected_private_file_at(
    state: &StateRoot,
    id: Uuid,
) -> Result<VerifiedCredentials, AuthorizationError> {
    let index = AccountIndex::read(state)?;
    if index.selected != Some(id) {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let entry = index
        .accounts
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or(AuthorizationError::InvalidIdentity)?;
    if entry.storage != AccountStorage::PrivateFile {
        return Err(AuthorizationError::InvalidIdentity);
    }
    entry.ensure_usable()?;
    let token = entry.token.ok_or(AuthorizationError::InvalidIdentity)?;
    if !token.valid_for(AccountStorage::PrivateFile) {
        return Err(AuthorizationError::ConsentRequired);
    }
    Ok(token.credentials)
}

pub(super) fn load_selected_keyring_at(
    state: &StateRoot,
    id: Uuid,
) -> Result<VerifiedCredentials, AuthorizationError> {
    let index = AccountIndex::read(state)?;
    if index.selected != Some(id) {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let entry = index
        .accounts
        .into_iter()
        .find(|entry| entry.id == id)
        .ok_or(AuthorizationError::InvalidIdentity)?;
    if entry.storage != AccountStorage::Keyring {
        return Err(AuthorizationError::InvalidIdentity);
    }
    entry.ensure_usable()?;
    Ok(read_keyring_token(&entry)?.credentials)
}

fn read_keyring_token(entry: &AccountEntry) -> Result<TokenRecord, AuthorizationError> {
    let slot = entry
        .keyring_slot
        .as_deref()
        .ok_or(AuthorizationError::InvalidIdentity)?;
    let bytes = read_chatgpt_keyring_record(slot)
        .map_err(AuthorizationError::CredentialStore)?
        .ok_or(AuthorizationError::CredentialMissing)?;
    admit_keyring_record(entry, &bytes)
}

fn admit_keyring_record(
    entry: &AccountEntry,
    bytes: &[u8],
) -> Result<TokenRecord, AuthorizationError> {
    let token = decode_keyring_record(bytes).ok_or(AuthorizationError::InvalidIdentity)?;
    if entry.keyring_slot.is_none()
        || token.slot != entry.keyring_slot
        || !token.credentials.valid_saved()
        || token.credentials.client_id != entry.client_id
        || token.credentials.chatgpt_account_id.as_ref() != Some(&entry.chatgpt_account_id)
        || token.credentials.subject != entry.subject
        || token.credentials.host_id != entry.host_id
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    if !token
        .consent
        .matches(&token.credentials, AccountStorage::Keyring)
    {
        return Err(AuthorizationError::ConsentRequired);
    }
    Ok(token)
}

pub(super) fn selected_keyring_slot(
    expected_id: Uuid,
) -> Result<Option<String>, AuthorizationError> {
    let state = open_account_state()?;
    let index = AccountIndex::read(&state)?;
    if index.selected != Some(expected_id) {
        return Err(AuthorizationError::SelectedAccountChanged);
    }
    let entry = &index.accounts[index.account_position(expected_id)?];
    entry.ensure_usable()?;
    Ok(entry.keyring_slot.clone())
}

pub(super) async fn selected_for_use(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
) -> Result<SelectedAccount, AuthorizationError> {
    let state = open_account_state()?;
    let id = match expected_account_id {
        Some(id) => id,
        None => AccountIndex::read(&state)?
            .selected
            .ok_or(AuthorizationError::NoSelectedAccount)?,
    };
    admit_backend_account_at(&state, &workspace, id)?;
    refresh_selected_at(state, workspace, id).await
}

fn admit_backend_account_at(
    state: &StateRoot,
    workspace: &Path,
    expected_id: Uuid,
) -> Result<(), AuthorizationError> {
    state
        .with_account_replacement_lock(workspace, || {
            let index = AccountIndex::read(state)?;
            if index.selected != Some(expected_id) {
                return Err(AuthorizationError::SelectedAccountChanged);
            }
            let entry = &index.accounts[index.account_position(expected_id)?];
            entry.ensure_usable()?;
            if entry.client_id != super::CLIENT_ID {
                return Err(AuthorizationError::InvalidClient);
            }
            Ok(())
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) fn authenticate(expected_account_id: Option<Uuid>) -> Result<(), AuthorizationError> {
    let state = open_account_state()?;
    let id = selected_id_at(&state)?;
    if expected_account_id.is_some_and(|expected| expected != id) {
        return Err(AuthorizationError::SelectedAccountChanged);
    }
    let credentials = if selected_keyring_slot(id)?.is_some() {
        load_selected_keyring_at(&state, id)?
    } else {
        load_selected_private_file_at(&state, id)?
    };
    drop(credentials);
    Ok(())
}

pub(super) fn selected_id() -> Result<Uuid, AuthorizationError> {
    selected_id_at(&open_account_state()?)
}

pub(super) fn selected_registration() -> Result<(Uuid, bool), AuthorizationError> {
    selected_registration_at(&open_account_state()?)
}

fn selected_registration_at(state: &StateRoot) -> Result<(Uuid, bool), AuthorizationError> {
    let index = AccountIndex::read(state)?;
    let id = index
        .selected
        .ok_or(AuthorizationError::NoSelectedAccount)?;
    let entry = index
        .accounts
        .iter()
        .find(|entry| entry.id == id)
        .ok_or(AuthorizationError::InvalidIdentity)?;
    Ok((id, !entry.disconnected && !entry.signout_pending))
}

pub(super) fn account_inspections()
-> Result<(Option<Uuid>, Vec<AccountInspection>), AuthorizationError> {
    account_inspections_at(&open_account_state()?)
}

fn account_inspections_at(
    state: &StateRoot,
) -> Result<(Option<Uuid>, Vec<AccountInspection>), AuthorizationError> {
    let index = AccountIndex::read(state)?;
    let mut accounts = index
        .accounts
        .into_iter()
        .map(|entry| AccountInspection {
            id: entry.id,
            storage: entry.storage,
            access: if entry.signout_pending {
                AccountAccess::SignOutPending
            } else if entry.renewal_pending {
                AccountAccess::RenewalPending
            } else if entry.disconnected {
                AccountAccess::Disconnected
            } else {
                AccountAccess::Connected
            },
        })
        .collect::<Vec<_>>();
    accounts.sort_by_key(|account| Some(account.id) != index.selected);
    Ok((index.selected, accounts))
}

pub(super) fn saved_ids() -> Result<(Uuid, Vec<Uuid>), AuthorizationError> {
    saved_ids_at(&open_account_state()?)
}

pub(super) fn saved_account_index(
    workspace: &Path,
) -> Result<(Uuid, Vec<Uuid>, Vec<u8>), AuthorizationError> {
    let state = open_account_state()?;
    state
        .with_account_replacement_lock(workspace, || {
            let bytes = state
                .record(CHATGPT_ACCOUNTS.kind)
                .read()
                .map_err(|_| AuthorizationError::InvalidSavedState)?
                .ok_or(AuthorizationError::NoSelectedAccount)?;
            let index = AccountIndex::decode(&bytes)?;
            let selected = index
                .selected
                .ok_or(AuthorizationError::NoSelectedAccount)?;
            let mut ids = index
                .accounts
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>();
            ids.sort_by_key(|id| *id != selected);
            Ok((selected, ids, bytes))
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn saved_ids_at(state: &StateRoot) -> Result<(Uuid, Vec<Uuid>), AuthorizationError> {
    let index = AccountIndex::read(state)?;
    let selected = index
        .selected
        .ok_or(AuthorizationError::NoSelectedAccount)?;
    let mut ids = vec![selected];
    ids.extend(
        index
            .accounts
            .iter()
            .filter(|entry| entry.id != selected && entry.ensure_usable().is_ok())
            .map(|entry| entry.id),
    );
    Ok((selected, ids))
}

pub(super) async fn select_saved(
    workspace: PathBuf,
    expected_selected: Uuid,
    target: Uuid,
) -> Result<(), AuthorizationError> {
    tokio::task::spawn_blocking(move || {
        let state = open_account_state()?;
        select_saved_at(&state, &workspace, expected_selected, target)
    })
    .await
    .map_err(|_| {
        AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown,
        )
    })?
}

pub(crate) async fn sign_out_selected(
    workspace: PathBuf,
) -> Result<SignOutOutcome, AuthorizationError> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let state = open_account_state()?;
        sign_out_at_with(
            &state,
            &workspace,
            read_keyring_token,
            |credentials| handle.block_on(credentials.revoke_remote()),
            |slot| delete_chatgpt_keyring_record(slot).map_err(|_| AuthorizationError::Unavailable),
        )
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

pub(crate) async fn remove_account_locally(
    workspace: PathBuf,
    account_id: Uuid,
    expected_index: Vec<u8>,
) -> Result<SignOutOutcome, AuthorizationError> {
    tokio::task::spawn_blocking(move || {
        let state = open_account_state()?;
        sign_out_target_at_with(
            &state,
            &workspace,
            SignOutTarget {
                account_id: Some(account_id),
                expected_index: Some(&expected_index),
            },
            SignOutActions {
                read_keyring: read_keyring_token,
                revoke: None::<fn(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
                delete_keyring: |slot: &str| {
                    delete_chatgpt_keyring_record(slot).map_err(|_| AuthorizationError::Unavailable)
                },
            },
        )
    })
    .await
    .map_err(|_| {
        AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown,
        )
    })?
}

fn sign_out_at_with(
    state: &StateRoot,
    workspace: &Path,
    read_keyring: impl FnOnce(&AccountEntry) -> Result<TokenRecord, AuthorizationError>,
    revoke: impl FnOnce(&VerifiedCredentials) -> Result<(), AuthorizationError>,
    delete_keyring: impl FnOnce(&str) -> Result<(), AuthorizationError>,
) -> Result<SignOutOutcome, AuthorizationError> {
    sign_out_target_at_with(
        state,
        workspace,
        SignOutTarget {
            account_id: None,
            expected_index: None,
        },
        SignOutActions {
            read_keyring,
            revoke: Some(revoke),
            delete_keyring,
        },
    )
}

fn sign_out_target_at_with(
    state: &StateRoot,
    workspace: &Path,
    target: SignOutTarget<'_>,
    actions: SignOutActions<
        impl FnOnce(&AccountEntry) -> Result<TokenRecord, AuthorizationError>,
        impl FnOnce(&VerifiedCredentials) -> Result<(), AuthorizationError>,
        impl FnOnce(&str) -> Result<(), AuthorizationError>,
    >,
) -> Result<SignOutOutcome, AuthorizationError> {
    state
        .with_account_replacement_lock(workspace, || {
            if let Some(expected) = target.expected_index {
                let current = state
                    .record(RecordKind::ChatgptAccounts)
                    .read()
                    .map_err(|_| AuthorizationError::InvalidSavedState)?;
                if current.as_deref() != Some(expected) {
                    return Err(AuthorizationError::SetupChanged);
                }
            }
            let mut index = AccountIndex::read(state)?;
            let id = target
                .account_id
                .or(index.selected)
                .ok_or(AuthorizationError::NoSelectedAccount)?;
            let position = index.account_position(id)?;
            let entry = &index.accounts[position];
            let slot = entry.keyring_slot.clone();
            if entry.disconnected {
                let remote_confirmed =
                    revoke_stored_token(entry, actions.read_keyring, actions.revoke);
                index.accounts[position].token = None;
                index.write(state)?;
                let local_cleared = slot
                    .as_deref()
                    .is_none_or(|slot| (actions.delete_keyring)(slot).is_ok());
                if !local_cleared {
                    return Err(AuthorizationError::SignOutStorageUncertain);
                }
                index.accounts[position].signout_pending = false;
                index
                    .write(state)
                    .map_err(|_| AuthorizationError::SignOutStorageUncertain)?;
                forget_removed_account(state, &mut index, target.account_id)?;
                return Ok(SignOutOutcome {
                    remote_confirmed,
                    local_cleared,
                });
            }
            if !entry.signout_pending {
                index.accounts[position].renewal_pending = false;
                index.accounts[position].signout_pending = true;
                index.write(state)?;
            }
            let entry = &index.accounts[position];
            let remote_confirmed = revoke_stored_token(entry, actions.read_keyring, actions.revoke);
            index.accounts[position].token = None;
            index.accounts[position].signout_pending = false;
            index.accounts[position].disconnected = true;
            index
                .write(state)
                .map_err(|_| AuthorizationError::SignOutStorageUncertain)?;
            let local_cleared = slot
                .as_deref()
                .is_none_or(|slot| (actions.delete_keyring)(slot).is_ok());
            if !local_cleared {
                return Err(AuthorizationError::SignOutStorageUncertain);
            }
            forget_removed_account(state, &mut index, target.account_id)?;
            Ok(SignOutOutcome {
                remote_confirmed,
                local_cleared,
            })
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn revoke_stored_token(
    entry: &AccountEntry,
    read_keyring: impl FnOnce(&AccountEntry) -> Result<TokenRecord, AuthorizationError>,
    revoke: Option<impl FnOnce(&VerifiedCredentials) -> Result<(), AuthorizationError>>,
) -> bool {
    let Some(revoke) = revoke else {
        return false;
    };
    let token = match entry.storage {
        AccountStorage::Keyring => read_keyring(entry).ok(),
        AccountStorage::PrivateFile => entry.token.clone(),
    };
    token
        .filter(|token| token.valid_for(entry.storage))
        .is_some_and(|token| revoke(&token.credentials).is_ok())
}

fn forget_removed_account(
    state: &StateRoot,
    index: &mut AccountIndex,
    target: Option<Uuid>,
) -> Result<(), AuthorizationError> {
    let Some(id) = target else {
        return Ok(());
    };
    index.accounts.retain(|entry| entry.id != id);
    if index.selected == Some(id) {
        index.selected = index
            .accounts
            .iter()
            .find(|entry| entry.ensure_usable().is_ok())
            .or_else(|| index.accounts.first())
            .map(|entry| entry.id);
    }
    index
        .write(state)
        .map_err(|_| AuthorizationError::SignOutStorageUncertain)
}

fn select_saved_at(
    state: &StateRoot,
    workspace: &Path,
    expected_selected: Uuid,
    target: Uuid,
) -> Result<(), AuthorizationError> {
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            if index.selected != Some(expected_selected) {
                return Err(AuthorizationError::SelectedAccountChanged);
            }
            let entry = index
                .accounts
                .iter()
                .find(|entry| entry.id == target)
                .ok_or(AuthorizationError::SelectedAccountChanged)?;
            if entry.disconnected {
                return Err(AuthorizationError::NoSelectedAccount);
            }
            if entry.signout_pending {
                return Err(AuthorizationError::SignOutPending);
            }
            if entry.renewal_pending {
                return Err(AuthorizationError::RenewalStorageUncertain);
            }
            let token = match entry.storage {
                AccountStorage::Keyring => read_keyring_token(entry)?,
                AccountStorage::PrivateFile => entry
                    .token
                    .clone()
                    .ok_or(AuthorizationError::InvalidIdentity)?,
            };
            if !token.valid_for(entry.storage) {
                return Err(AuthorizationError::ConsentRequired);
            }
            if target != expected_selected {
                index.selected = Some(target);
                index.write(state)?;
            }
            Ok(())
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) fn selected_reauthorization_target(
    expected_id: Uuid,
) -> Result<ReauthorizationTarget, AuthorizationError> {
    selected_reauthorization_target_at(&open_account_state()?, expected_id)
}

fn selected_reauthorization_target_at(
    state: &StateRoot,
    expected_id: Uuid,
) -> Result<ReauthorizationTarget, AuthorizationError> {
    let index = AccountIndex::read(state)?;
    if index.selected != Some(expected_id) {
        return Err(AuthorizationError::SelectedAccountChanged);
    }
    let entry = index
        .accounts
        .into_iter()
        .find(|entry| entry.id == expected_id)
        .ok_or(AuthorizationError::InvalidIdentity)?;
    Ok(ReauthorizationTarget {
        id: entry.id,
        host_id: entry.host_id,
        client_id: entry.client_id,
        subject: entry.subject,
        storage: entry.storage,
        chatgpt_account_id: entry.chatgpt_account_id,
    })
}

fn selected_id_at(state: &StateRoot) -> Result<Uuid, AuthorizationError> {
    let (id, connected) = selected_registration_at(state)?;
    if connected {
        Ok(id)
    } else {
        Err(AuthorizationError::NoSelectedAccount)
    }
}

fn open_account_state() -> Result<StateRoot, AuthorizationError> {
    let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
    arany::StateRoot::open_optional(&path)
        .map_err(|_| AuthorizationError::Unavailable)?
        .ok_or(AuthorizationError::NoSelectedAccount)
}

async fn refresh_selected_at(
    state: StateRoot,
    workspace: PathBuf,
    id: Uuid,
) -> Result<SelectedAccount, AuthorizationError> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthorizationError::Unavailable)?
            .as_secs();
        refresh_selected_with(&state, &workspace, id, now, |credentials| {
            handle.block_on(credentials.refresh_uncommitted())
        })
    })
    .await
    .map_err(|_| AuthorizationError::RenewalStorageUncertain)?
}

fn refresh_selected_with(
    state: &StateRoot,
    workspace: &Path,
    id: Uuid,
    now: u64,
    refresh: impl FnOnce(&VerifiedCredentials) -> Result<VerifiedCredentials, AuthorizationError>,
) -> Result<SelectedAccount, AuthorizationError> {
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            let (position, storage, token) = selected_token(&index, id)?;
            if token.credentials.access_expires_at_unix > now.saturating_add(REFRESH_EARLY_SECONDS)
            {
                return Ok(SelectedAccount::from_token(id, storage, token));
            }
            index.accounts[position].renewal_pending = true;
            index.write(state)?;
            let replacement = match refresh(&token.credentials) {
                Ok(replacement) => replacement,
                Err(error) => {
                    settle_failed_refresh(state, &mut index, position, &error)?;
                    return Err(error);
                }
            };
            check_replacement(&replacement, &token.credentials, now)?;
            let updated = TokenRecord {
                slot: token.slot.clone(),
                credentials: replacement,
                consent: token.consent,
            };
            if !updated.valid_for(storage) {
                return Err(AuthorizationError::InvalidIdentity);
            }
            store_refreshed(state, &mut index, position, id, updated)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn selected_token(
    index: &AccountIndex,
    id: Uuid,
) -> Result<(usize, AccountStorage, TokenRecord), AuthorizationError> {
    if index.selected != Some(id) {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let position = index.account_position(id)?;
    let entry = &index.accounts[position];
    entry.ensure_usable()?;
    let storage = entry.storage;
    let token = match storage {
        AccountStorage::Keyring => read_keyring_token(entry)?,
        AccountStorage::PrivateFile => entry
            .token
            .as_ref()
            .cloned()
            .ok_or(AuthorizationError::InvalidIdentity)?,
    };
    if token.credentials.client_id != entry.client_id
        || token.credentials.host_id != entry.host_id
        || token.credentials.subject != entry.subject
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    if !token.valid_for(storage) {
        return Err(AuthorizationError::ConsentRequired);
    }
    Ok((position, storage, token))
}

// An unavailable refresh clears the pending mark; an unusable refresh token disconnects the account.
fn settle_failed_refresh(
    state: &StateRoot,
    index: &mut AccountIndex,
    position: usize,
    error: &AuthorizationError,
) -> Result<(), AuthorizationError> {
    if matches!(error, AuthorizationError::Unavailable) {
        index.accounts[position].renewal_pending = false;
        index
            .write(state)
            .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
    } else if matches!(error, AuthorizationError::RefreshTokenUnusable) {
        disconnect_unusable_refresh_with(state, index, position, |slot| {
            delete_chatgpt_keyring_record(slot).map_err(|_| AuthorizationError::Unavailable)
        })?;
    }
    Ok(())
}

fn check_replacement(
    replacement: &VerifiedCredentials,
    current: &VerifiedCredentials,
    now: u64,
) -> Result<(), AuthorizationError> {
    if !replacement.valid_saved()
        || replacement.client_id != current.client_id
        || replacement.subject != current.subject
        || replacement.host_id != current.host_id
        || replacement.id_token != current.id_token
        || replacement.access_token == current.access_token
        || replacement.chatgpt_account_id != current.chatgpt_account_id
        || replacement.access_expires_at_unix <= now
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    Ok(())
}

fn store_refreshed(
    state: &StateRoot,
    index: &mut AccountIndex,
    position: usize,
    id: Uuid,
    updated: TokenRecord,
) -> Result<SelectedAccount, AuthorizationError> {
    let storage = index.accounts[position].storage;
    match storage {
        AccountStorage::Keyring => {
            let record = serde_json::to_string(&updated)
                .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
            let slot = index.accounts[position]
                .keyring_slot
                .as_deref()
                .ok_or(AuthorizationError::RenewalStorageUncertain)?;
            write_chatgpt_keyring_record(slot, &record)
                .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
            index.accounts[position].renewal_pending = false;
            index
                .write(state)
                .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
            Ok(SelectedAccount::from_token(id, storage, updated))
        }
        AccountStorage::PrivateFile => {
            index.accounts[position].token = Some(updated);
            index.accounts[position].renewal_pending = false;
            index
                .write(state)
                .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
            Ok(SelectedAccount::from_token(
                id,
                storage,
                index.accounts[position]
                    .token
                    .take()
                    .ok_or(AuthorizationError::RenewalStorageUncertain)?,
            ))
        }
    }
}

fn disconnect_unusable_refresh_with(
    state: &StateRoot,
    index: &mut AccountIndex,
    position: usize,
    delete_keyring: impl FnOnce(&str) -> Result<(), AuthorizationError>,
) -> Result<(), AuthorizationError> {
    let entry = &mut index.accounts[position];
    let slot = entry.keyring_slot.clone();
    entry.token = None;
    entry.renewal_pending = false;
    entry.disconnected = true;
    index
        .write(state)
        .map_err(|_| AuthorizationError::RefreshCleanupUncertain)?;
    if let Some(slot) = slot {
        delete_keyring(&slot).map_err(|_| AuthorizationError::RefreshCleanupUncertain)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod public_client_tests;
