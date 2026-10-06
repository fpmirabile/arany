use super::{
    AuthorizationError, RiskAcknowledgment, VerifiedCredentials, VerifiedIdentity,
    consent::ConsentReceipt, registration, valid_client_id,
};
use crate::cli::credentials::{
    AccountStorage, delete_chatgpt_keyring_record, read_chatgpt_keyring_record,
    write_chatgpt_keyring_record,
};
use arany::{Effort, StateRoot, validate_native_model_id};
use aws_lc_rs::hmac;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::{Uuid, Variant, Version};

const MAX_ACCOUNTS: usize = 8;
const MAX_MODEL_CHECKS: usize = 64;
const MODEL_CHECK_AGE_SECONDS: u64 = 24 * 60 * 60;
const MODEL_CHECK_VERSION: &str = "chatgpt-strict-stream-conformance-v7";
const ACCOUNT_ADMISSION_VERSION: &str = "chatgpt-consented-account-admission-v9";
const REFRESH_EARLY_SECONDS: u64 = 300;
pub(crate) const MAX_TOKEN_RECORD_BYTES: usize = 64 * 1024;

fn keyring_slot(client_id: &str) -> String {
    format!(
        "chatgpt-{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(client_id.as_bytes()))
    )
}

pub(crate) fn valid_keyring_record(slot: &str, bytes: &[u8]) -> bool {
    if bytes.len() > MAX_TOKEN_RECORD_BYTES {
        return false;
    }
    let Ok(record) = serde_json::from_slice::<TokenRecord>(bytes) else {
        return false;
    };
    record.valid_for(AccountStorage::Keyring) && keyring_slot(&record.credentials.client_id) == slot
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TokenRecord {
    schema: u8,
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

pub(crate) struct ReauthorizationTarget {
    pub(crate) id: Uuid,
    pub(crate) host_id: Uuid,
    pub(crate) client_id: String,
    pub(crate) subject: String,
    pub(crate) storage: AccountStorage,
    pub(crate) plan_permission_missing: bool,
}

pub(crate) struct DisabledSignInOutcome {
    pub(crate) id: Uuid,
    pub(crate) local_cleared: bool,
}

pub(crate) struct SignOutOutcome {
    pub(crate) remote_confirmed: bool,
    pub(crate) local_cleared: bool,
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
    fn valid_for(&self, storage: AccountStorage) -> bool {
        self.schema == 1
            && self.credentials.valid_saved()
            && self.consent.matches(&self.credentials, storage)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AccountEntry {
    id: Uuid,
    host_id: Uuid,
    client_id: String,
    subject: String,
    storage: AccountStorage,
    renewal_pending: bool,
    #[serde(default)]
    signout_pending: bool,
    #[serde(default)]
    disconnected: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    plan_permission_missing: bool,
    token: Option<TokenRecord>,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelCheck {
    account_id: Uuid,
    fingerprint: [u8; 32],
    checked_at_sec: u64,
    expires_at_sec: u64,
}

impl ModelCheck {
    fn valid(&self) -> bool {
        self.account_id.get_version() == Some(Version::SortRand)
            && self.checked_at_sec != 0
            && self.checked_at_sec.checked_add(MODEL_CHECK_AGE_SECONDS) == Some(self.expires_at_sec)
    }
}

impl AccountEntry {
    fn valid(&self) -> bool {
        self.id.get_version() == Some(Version::SortRand)
            && self.host_id.get_version() == Some(Version::Random)
            && self.host_id.get_variant() == Variant::RFC4122
            && valid_client_id(&self.client_id)
            && !self.subject.is_empty()
            && self.subject.len() <= 512
            && self.subject.bytes().all(|byte| byte.is_ascii_graphic())
            && !(self.signout_pending && (self.renewal_pending || self.disconnected))
            && (!self.plan_permission_missing || (self.disconnected && self.token.is_none()))
            && match (&self.storage, &self.token) {
                (AccountStorage::PrivateFile, Some(token)) => {
                    !self.disconnected
                        && token.schema == 1
                        && token.credentials.valid_saved()
                        && token.credentials.client_id == self.client_id
                        && token.credentials.host_id == self.host_id
                        && token.credentials.subject == self.subject
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
    schema: u8,
    selected: Option<Uuid>,
    accounts: Vec<AccountEntry>,
    #[serde(default, deserialize_with = "bounded_model_checks")]
    model_checks: Vec<ModelCheck>,
}

fn bounded_model_checks<'de, D>(deserializer: D) -> Result<Vec<ModelCheck>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ChecksVisitor;

    impl<'de> de::Visitor<'de> for ChecksVisitor {
        type Value = Vec<ModelCheck>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded ChatGPT model-check array")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut rows = Vec::new();
            while let Some(row) = sequence.next_element::<ModelCheck>()? {
                if rows.len() == MAX_MODEL_CHECKS {
                    return Err(de::Error::custom("too many model checks"));
                }
                rows.push(row);
            }
            Ok(rows)
        }
    }

    deserializer.deserialize_seq(ChecksVisitor)
}

impl AccountIndex {
    fn empty() -> Self {
        Self {
            schema: 1,
            selected: None,
            accounts: Vec::new(),
            model_checks: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), AuthorizationError> {
        if self.schema != 1
            || self.accounts.len() > MAX_ACCOUNTS
            || self.model_checks.len() > MAX_MODEL_CHECKS
            || self
                .selected
                .is_some_and(|id| !self.accounts.iter().any(|entry| entry.id == id))
            || self.accounts.iter().any(|entry| !entry.valid())
            || self.model_checks.iter().any(|check| {
                !check.valid()
                    || !self.accounts.iter().any(|account| {
                        account.id == check.account_id
                            && !account.disconnected
                            && !account.signout_pending
                    })
            })
            || self
                .accounts
                .iter()
                .any(|entry| entry.host_id != self.accounts[0].host_id)
        {
            return Err(AuthorizationError::InvalidIdentity);
        }
        for (index, account) in self.accounts.iter().enumerate() {
            if self.accounts[..index]
                .iter()
                .any(|prior| prior.id == account.id || prior.client_id == account.client_id)
            {
                return Err(AuthorizationError::InvalidIdentity);
            }
        }
        for (index, check) in self.model_checks.iter().enumerate() {
            if self.model_checks[..index]
                .iter()
                .any(|prior| prior.fingerprint == check.fingerprint)
            {
                return Err(AuthorizationError::InvalidIdentity);
            }
        }
        Ok(())
    }

    fn require_selected_client(
        &self,
        account_id: Uuid,
        client_id: &str,
    ) -> Result<(), AuthorizationError> {
        if self.selected == Some(account_id)
            && self
                .accounts
                .iter()
                .any(|entry| entry.id == account_id && entry.client_id == client_id)
        {
            Ok(())
        } else {
            Err(AuthorizationError::SelectedAccountChanged)
        }
    }

    fn read(state: &StateRoot) -> Result<Self, AuthorizationError> {
        let Some(bytes) = state
            .read_chatgpt_accounts_record()
            .map_err(|_| AuthorizationError::Unavailable)?
        else {
            return Ok(Self::empty());
        };
        let index: Self =
            serde_json::from_slice(&bytes).map_err(|_| AuthorizationError::InvalidIdentity)?;
        index.validate()?;
        Ok(index)
    }

    fn write(&self, state: &StateRoot) -> Result<(), AuthorizationError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| AuthorizationError::Unavailable)?;
        state
            .replace_chatgpt_accounts_record(&bytes)
            .map_err(|_| AuthorizationError::Unavailable)
    }
}

fn model_check_fingerprint(
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
) -> Result<[u8; 32], AuthorizationError> {
    account_fingerprint(
        selected,
        model,
        effort,
        MODEL_CHECK_VERSION,
        "three-probes-1024-local-output-tokens",
    )
}

fn account_fingerprint(
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
    version: &str,
    contract: &str,
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
        version,
        env!("CARGO_PKG_VERSION"),
        "https://api.openai.com/v1/responses;store=false;stream=true",
        contract,
        "sequential-concurrency-one",
        &selected.credentials.client_id,
        &selected.credentials.subject,
        model,
        effort.as_str(),
    ] {
        message.extend_from_slice(&(field.len() as u64).to_be_bytes());
        message.extend_from_slice(field.as_bytes());
    }
    let consent = selected.consent.fingerprint_bytes()?;
    message.extend_from_slice(&(consent.len() as u64).to_be_bytes());
    message.extend_from_slice(&consent);
    message.extend_from_slice(selected.id.as_bytes());
    message.extend_from_slice(selected.credentials.host_id.as_bytes());
    let key = hmac::Key::new(hmac::HMAC_SHA256, selected.credentials.id_token.as_bytes());
    let tag = hmac::sign(&key, &message);
    tag.as_ref()
        .try_into()
        .map_err(|_| AuthorizationError::EvidenceUnavailable)
}

fn verify_selected_snapshot(
    index: &AccountIndex,
    selected: &SelectedAccount,
) -> Result<(), AuthorizationError> {
    if index.selected != Some(selected.id) {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let entry = index
        .accounts
        .iter()
        .find(|entry| entry.id == selected.id)
        .ok_or(AuthorizationError::InvalidIdentity)?;
    if entry.disconnected {
        return Err(if entry.plan_permission_missing {
            AuthorizationError::PermissionMissing
        } else {
            AuthorizationError::NoSelectedAccount
        });
    }
    if entry.signout_pending {
        return Err(AuthorizationError::SignOutPending);
    }
    if entry.renewal_pending {
        return Err(AuthorizationError::RenewalStorageUncertain);
    }
    if entry.storage != selected.storage
        || entry.host_id != selected.credentials.host_id
        || entry.client_id != selected.credentials.client_id
        || entry.subject != selected.credentials.subject
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let token = match entry.storage {
        AccountStorage::Keyring => {
            read_keyring_token(&entry.client_id, &entry.subject, entry.host_id)?
        }
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
    Ok(())
}

pub(super) async fn clear_model_check(
    workspace: PathBuf,
    selected: SelectedAccount,
    model: String,
    effort: Effort,
) -> Result<(), AuthorizationError> {
    tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        clear_model_check_at(&state, &workspace, &selected, &model, effort)
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

fn clear_model_check_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
) -> Result<(), AuthorizationError> {
    let fingerprint = model_check_fingerprint(selected, model, effort)?;
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            verify_selected_snapshot(&index, selected)?;
            let previous = index.model_checks.len();
            index
                .model_checks
                .retain(|check| check.fingerprint != fingerprint);
            if index.model_checks.len() != previous {
                index.write(state)?;
            }
            Ok(())
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) async fn record_model_check(
    workspace: PathBuf,
    selected: SelectedAccount,
    model: String,
    effort: Effort,
) -> Result<(), AuthorizationError> {
    tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthorizationError::EvidenceUnavailable)?
            .as_secs();
        record_model_check_at(&state, &workspace, &selected, &model, effort, now)
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

fn record_model_check_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
    now: u64,
) -> Result<(), AuthorizationError> {
    let fingerprint = model_check_fingerprint(selected, model, effort)?;
    let expires_at_sec = now
        .checked_add(MODEL_CHECK_AGE_SECONDS)
        .ok_or(AuthorizationError::EvidenceUnavailable)?;
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            verify_selected_snapshot(&index, selected)?;
            index
                .model_checks
                .retain(|check| check.expires_at_sec > now && check.fingerprint != fingerprint);
            if index.model_checks.len() == MAX_MODEL_CHECKS {
                return Err(AuthorizationError::EvidenceUnavailable);
            }
            index.model_checks.push(ModelCheck {
                account_id: selected.id,
                fingerprint,
                checked_at_sec: now,
                expires_at_sec,
            });
            index.write(state)?;
            let saved = AccountIndex::read(state)?;
            verify_selected_snapshot(&saved, selected)?;
            if !model_check_valid(&saved, selected.id, fingerprint, now) {
                return Err(AuthorizationError::EvidenceUnavailable);
            }
            Ok(())
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn model_check_valid(
    index: &AccountIndex,
    account_id: Uuid,
    fingerprint: [u8; 32],
    now: u64,
) -> bool {
    index.model_checks.iter().any(|check| {
        check.account_id == account_id
            && check.fingerprint == fingerprint
            && check.checked_at_sec <= now
            && now < check.expires_at_sec
    })
}

pub(super) async fn admitted_selected_for_run(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
    model: String,
    effort: Effort,
) -> Result<(SelectedAccount, [u8; 32]), AuthorizationError> {
    let selected = selected_for_use(workspace.clone(), expected_account_id).await?;
    let admitted = selected.clone();
    let fingerprint = tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        admitted_model_at(&state, &workspace, &admitted, &model, effort)
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)??;
    Ok((selected, fingerprint))
}

fn admitted_model_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    model: &str,
    effort: Effort,
) -> Result<[u8; 32], AuthorizationError> {
    let fingerprint = account_fingerprint(
        selected,
        model,
        effort,
        ACCOUNT_ADMISSION_VERSION,
        "no-preflight-inference;strict-stream-outcomes;local-output-bound",
    )?;
    state
        .with_account_replacement_lock(workspace, || {
            let index = AccountIndex::read(state)?;
            verify_selected_snapshot(&index, selected)?;
            Ok(fingerprint)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) fn contains_saved_client_at(
    state: &StateRoot,
    client_id: &str,
    host_id: Uuid,
) -> Result<bool, AuthorizationError> {
    match AccountIndex::read(state)?
        .accounts
        .into_iter()
        .find(|entry| entry.client_id == client_id)
    {
        Some(entry) if entry.host_id == host_id => Ok(true),
        Some(_) => Err(AuthorizationError::InvalidIdentity),
        None => Ok(false),
    }
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
    .map_err(|_| AuthorizationError::Unavailable)?
}

pub(crate) async fn save_without_plan_permission(
    workspace: PathBuf,
    identity: VerifiedIdentity,
    acknowledgment: RiskAcknowledgment,
    expected_selected: Option<Uuid>,
) -> Result<DisabledSignInOutcome, AuthorizationError> {
    let storage = acknowledgment.storage();
    tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::admit(&path).map_err(|_| AuthorizationError::Unavailable)?;
        save_without_plan_permission_at(
            &state,
            &workspace,
            identity,
            storage,
            expected_selected,
            |slot| delete_chatgpt_keyring_record(slot).map_err(|_| AuthorizationError::Unavailable),
        )
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

fn save_without_plan_permission_at(
    state: &StateRoot,
    workspace: &Path,
    identity: VerifiedIdentity,
    storage: AccountStorage,
    expected_selected: Option<Uuid>,
    delete: impl FnOnce(&str) -> Result<(), AuthorizationError>,
) -> Result<DisabledSignInOutcome, AuthorizationError> {
    if !identity.valid() {
        return Err(AuthorizationError::InvalidIdentity);
    }
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            registration::verify_host_at(state, identity.host_id)?;
            if index
                .accounts
                .first()
                .is_some_and(|entry| entry.host_id != identity.host_id)
            {
                return Err(AuthorizationError::RegistrationConflict);
            }
            if let Some(id) = expected_selected {
                index.require_selected_client(id, &identity.client_id)?;
            }
            let previous = index
                .accounts
                .iter()
                .position(|entry| entry.client_id == identity.client_id);
            let id = if let Some(position) = previous {
                let entry = &mut index.accounts[position];
                if entry.subject != identity.subject
                    || entry.host_id != identity.host_id
                    || entry.storage != storage
                {
                    return Err(AuthorizationError::RegistrationConflict);
                }
                entry.token = None;
                entry.renewal_pending = false;
                entry.signout_pending = false;
                entry.disconnected = true;
                entry.plan_permission_missing = true;
                entry.id
            } else {
                if index.accounts.len() == MAX_ACCOUNTS {
                    return Err(AuthorizationError::Unavailable);
                }
                let id = Uuid::now_v7();
                index.accounts.push(AccountEntry {
                    id,
                    host_id: identity.host_id,
                    client_id: identity.client_id.clone(),
                    subject: identity.subject,
                    storage,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: true,
                    plan_permission_missing: true,
                    token: None,
                });
                id
            };
            index.model_checks.retain(|check| check.account_id != id);
            index.selected = Some(id);
            index.write(state)?;
            registration::complete_issued_client_at(state, &identity.client_id, identity.host_id)?;
            let local_cleared = storage != AccountStorage::Keyring
                || previous.is_none()
                || delete(&keyring_slot(&identity.client_id)).is_ok();
            Ok(DisabledSignInOutcome { id, local_cleared })
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) fn save_private_file_at(
    state: &StateRoot,
    workspace: &Path,
    credentials: VerifiedCredentials,
    consent: ConsentReceipt,
    expected_selected: Option<Uuid>,
) -> Result<Uuid, AuthorizationError> {
    let token = TokenRecord {
        schema: 1,
        credentials,
        consent,
    };
    if !token.valid_for(AccountStorage::PrivateFile) {
        return Err(AuthorizationError::ConsentRequired);
    }
    let completed_client_id = token.credentials.client_id.clone();
    let host_id = token.credentials.host_id;
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
            if let Some(expected_id) = expected_selected {
                index.require_selected_client(expected_id, client_id)?;
            }
            let subject = &token.credentials.subject;
            let id = if let Some(entry) = index
                .accounts
                .iter_mut()
                .find(|entry| entry.client_id == *client_id)
            {
                if entry.subject != *subject
                    || entry.host_id != host_id
                    || entry.storage != AccountStorage::PrivateFile
                {
                    return Err(AuthorizationError::RegistrationConflict);
                }
                entry.token = Some(token);
                entry.renewal_pending = false;
                entry.signout_pending = false;
                entry.disconnected = false;
                entry.plan_permission_missing = false;
                entry.id
            } else {
                if index.accounts.len() == MAX_ACCOUNTS {
                    return Err(AuthorizationError::Unavailable);
                }
                let id = Uuid::now_v7();
                index.accounts.push(AccountEntry {
                    id,
                    host_id,
                    client_id: client_id.clone(),
                    subject: subject.clone(),
                    storage: AccountStorage::PrivateFile,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: false,
                    plan_permission_missing: false,
                    token: Some(token),
                });
                id
            };
            index.model_checks.retain(|check| check.account_id != id);
            index.selected = Some(id);
            index.write(state)?;
            registration::complete_issued_client_at(state, &completed_client_id, host_id)?;
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
            write_chatgpt_keyring_record(slot, record).map_err(|_| AuthorizationError::Unavailable)
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
    let token = TokenRecord {
        schema: 1,
        credentials,
        consent,
    };
    if !token.valid_for(AccountStorage::Keyring) {
        return Err(AuthorizationError::ConsentRequired);
    }
    let record = serde_json::to_string(&token).map_err(|_| AuthorizationError::Unavailable)?;
    if record.len() > MAX_TOKEN_RECORD_BYTES {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let completed_client_id = token.credentials.client_id.clone();
    let host_id = token.credentials.host_id;
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
            if let Some(expected_id) = expected_selected {
                index.require_selected_client(expected_id, client_id)?;
            }
            let subject = &token.credentials.subject;
            let id = if let Some(entry) = index
                .accounts
                .iter_mut()
                .find(|entry| entry.client_id == *client_id)
            {
                if entry.subject != *subject
                    || entry.host_id != host_id
                    || entry.storage != AccountStorage::Keyring
                {
                    return Err(AuthorizationError::RegistrationConflict);
                }
                let id = entry.id;
                entry.renewal_pending = true;
                index.write(state)?;
                id
            } else {
                if index.accounts.len() == MAX_ACCOUNTS {
                    return Err(AuthorizationError::Unavailable);
                }
                let id = Uuid::now_v7();
                index.accounts.push(AccountEntry {
                    id,
                    host_id,
                    client_id: client_id.clone(),
                    subject: subject.clone(),
                    storage: AccountStorage::Keyring,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: false,
                    plan_permission_missing: false,
                    token: None,
                });
                id
            };
            write(&keyring_slot(client_id), &record)?;
            let entry = index
                .accounts
                .iter_mut()
                .find(|entry| entry.id == id)
                .ok_or(AuthorizationError::InvalidIdentity)?;
            entry.renewal_pending = false;
            entry.signout_pending = false;
            entry.disconnected = false;
            entry.plan_permission_missing = false;
            index.model_checks.retain(|check| check.account_id != id);
            index.selected = Some(id);
            index.write(state)?;
            registration::complete_issued_client_at(state, &completed_client_id, host_id)?;
            Ok(id)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
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
    if entry.disconnected {
        return Err(if entry.plan_permission_missing {
            AuthorizationError::PermissionMissing
        } else {
            AuthorizationError::NoSelectedAccount
        });
    }
    if entry.signout_pending {
        return Err(AuthorizationError::SignOutPending);
    }
    if entry.renewal_pending {
        return Err(AuthorizationError::RenewalStorageUncertain);
    }
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
    if entry.disconnected {
        return Err(if entry.plan_permission_missing {
            AuthorizationError::PermissionMissing
        } else {
            AuthorizationError::NoSelectedAccount
        });
    }
    if entry.signout_pending {
        return Err(AuthorizationError::SignOutPending);
    }
    if entry.renewal_pending {
        return Err(AuthorizationError::RenewalStorageUncertain);
    }
    Ok(read_keyring_token(&entry.client_id, &entry.subject, entry.host_id)?.credentials)
}

fn read_keyring_token(
    client_id: &str,
    subject: &str,
    host_id: Uuid,
) -> Result<TokenRecord, AuthorizationError> {
    let slot = keyring_slot(client_id);
    let bytes = read_chatgpt_keyring_record(&slot)
        .map_err(AuthorizationError::CredentialStore)?
        .ok_or(AuthorizationError::CredentialMissing)?;
    if bytes.len() > MAX_TOKEN_RECORD_BYTES {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let token: TokenRecord =
        serde_json::from_slice(&bytes).map_err(|_| AuthorizationError::InvalidIdentity)?;
    if token.schema != 1
        || !token.credentials.valid_saved()
        || token.credentials.subject != subject
        || token.credentials.host_id != host_id
        || keyring_slot(&token.credentials.client_id) != slot
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
    refresh_selected_at(state, workspace, id).await
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

pub(super) fn saved_ids() -> Result<(Uuid, Vec<Uuid>), AuthorizationError> {
    saved_ids_at(&open_account_state()?)
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
            .filter(|entry| entry.id != selected && !entry.disconnected && !entry.signout_pending)
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
    .map_err(|_| AuthorizationError::Unavailable)?
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

fn sign_out_at_with(
    state: &StateRoot,
    workspace: &Path,
    read_keyring: impl FnOnce(&str, &str, Uuid) -> Result<TokenRecord, AuthorizationError>,
    revoke: impl FnOnce(&VerifiedCredentials) -> Result<(), AuthorizationError>,
    delete_keyring: impl FnOnce(&str) -> Result<(), AuthorizationError>,
) -> Result<SignOutOutcome, AuthorizationError> {
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            let id = index
                .selected
                .ok_or(AuthorizationError::NoSelectedAccount)?;
            let position = index
                .accounts
                .iter()
                .position(|entry| entry.id == id)
                .ok_or(AuthorizationError::InvalidIdentity)?;
            let entry = &index.accounts[position];
            let slot = keyring_slot(&entry.client_id);
            if entry.disconnected {
                return Ok(SignOutOutcome {
                    remote_confirmed: false,
                    local_cleared: entry.storage == AccountStorage::PrivateFile
                        || delete_keyring(&slot).is_ok(),
                });
            }
            let storage = entry.storage;
            if !entry.signout_pending {
                index.accounts[position].renewal_pending = false;
                index.accounts[position].signout_pending = true;
                index.model_checks.retain(|check| check.account_id != id);
                index.write(state)?;
            }
            let entry = &index.accounts[position];
            let token = match storage {
                AccountStorage::Keyring => {
                    read_keyring(&entry.client_id, &entry.subject, entry.host_id).ok()
                }
                AccountStorage::PrivateFile => entry.token.clone(),
            };
            let remote_confirmed = token
                .filter(|token| token.valid_for(storage))
                .is_some_and(|token| revoke(&token.credentials).is_ok());
            index.accounts[position].token = None;
            index.accounts[position].signout_pending = false;
            index.accounts[position].disconnected = true;
            index
                .write(state)
                .map_err(|_| AuthorizationError::SignOutStorageUncertain)?;
            Ok(SignOutOutcome {
                remote_confirmed,
                local_cleared: storage == AccountStorage::PrivateFile
                    || delete_keyring(&slot).is_ok(),
            })
        })
        .map_err(|_| AuthorizationError::Unavailable)?
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
                AccountStorage::Keyring => {
                    read_keyring_token(&entry.client_id, &entry.subject, entry.host_id)?
                }
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
        plan_permission_missing: entry.plan_permission_missing,
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
    StateRoot::open_existing(&path).map_err(|error| match error {
        arany::StoreError::Io(source) if source.kind() == std::io::ErrorKind::NotFound => {
            AuthorizationError::NoSelectedAccount
        }
        _ => AuthorizationError::Unavailable,
    })
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
    .map_err(|_| AuthorizationError::Unavailable)?
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
            if index.selected != Some(id) {
                return Err(AuthorizationError::InvalidIdentity);
            }
            let position = index
                .accounts
                .iter()
                .position(|entry| entry.id == id)
                .ok_or(AuthorizationError::InvalidIdentity)?;
            let entry = &index.accounts[position];
            if entry.disconnected {
                return Err(if entry.plan_permission_missing {
                    AuthorizationError::PermissionMissing
                } else {
                    AuthorizationError::NoSelectedAccount
                });
            }
            if entry.signout_pending {
                return Err(AuthorizationError::SignOutPending);
            }
            if entry.renewal_pending {
                return Err(AuthorizationError::RenewalStorageUncertain);
            }
            let client_id = entry.client_id.clone();
            let subject = entry.subject.clone();
            let storage = entry.storage;
            let token = match storage {
                AccountStorage::Keyring => read_keyring_token(&client_id, &subject, entry.host_id)?,
                AccountStorage::PrivateFile => entry
                    .token
                    .as_ref()
                    .cloned()
                    .ok_or(AuthorizationError::InvalidIdentity)?,
            };
            if token.credentials.client_id != client_id
                || token.credentials.host_id != entry.host_id
                || token.credentials.subject != subject
            {
                return Err(AuthorizationError::InvalidIdentity);
            }
            if !token.valid_for(storage) {
                return Err(AuthorizationError::ConsentRequired);
            }
            if token.credentials.access_expires_at_unix > now.saturating_add(REFRESH_EARLY_SECONDS)
            {
                return Ok(SelectedAccount::from_token(id, storage, token));
            }
            index.accounts[position].renewal_pending = true;
            index.write(state)?;
            let replacement = match refresh(&token.credentials) {
                Ok(replacement) => replacement,
                Err(error) => {
                    if matches!(&error, AuthorizationError::Unavailable) {
                        index.accounts[position].renewal_pending = false;
                        index
                            .write(state)
                            .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
                    } else if matches!(&error, AuthorizationError::RefreshTokenUnusable) {
                        disconnect_unusable_refresh_with(state, &mut index, position, |slot| {
                            delete_chatgpt_keyring_record(slot)
                                .map_err(|_| AuthorizationError::Unavailable)
                        })?;
                    }
                    return Err(error);
                }
            };
            if !replacement.valid_saved()
                || replacement.client_id != client_id
                || replacement.subject != subject
                || replacement.host_id != token.credentials.host_id
                || replacement.id_token != token.credentials.id_token
                || replacement.access_token == token.credentials.access_token
                || replacement.refresh_token == token.credentials.refresh_token
                || replacement.access_expires_at_unix <= now
            {
                return Err(AuthorizationError::InvalidIdentity);
            }
            let updated = TokenRecord {
                schema: 1,
                credentials: replacement,
                consent: token.consent,
            };
            if !updated.valid_for(storage) {
                return Err(AuthorizationError::InvalidIdentity);
            }
            match storage {
                AccountStorage::Keyring => {
                    let record = serde_json::to_string(&updated)
                        .map_err(|_| AuthorizationError::RenewalStorageUncertain)?;
                    write_chatgpt_keyring_record(&keyring_slot(&client_id), &record)
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
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn disconnect_unusable_refresh_with(
    state: &StateRoot,
    index: &mut AccountIndex,
    position: usize,
    delete_keyring: impl FnOnce(&str) -> Result<(), AuthorizationError>,
) -> Result<(), AuthorizationError> {
    let entry = &mut index.accounts[position];
    let id = entry.id;
    let slot = (entry.storage == AccountStorage::Keyring).then(|| keyring_slot(&entry.client_id));
    entry.token = None;
    entry.renewal_pending = false;
    entry.disconnected = true;
    index.model_checks.retain(|check| check.account_id != id);
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
