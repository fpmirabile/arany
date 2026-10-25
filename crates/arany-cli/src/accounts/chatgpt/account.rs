use super::{
    AuthorizationError, RiskAcknowledgment, VerifiedCredentials, VerifiedIdentity,
    consent::ConsentReceipt, registration, valid_client_id,
};
use crate::accounts::credentials::{
    AccountStorage, delete_chatgpt_keyring_record, read_chatgpt_keyring_record,
    write_chatgpt_keyring_record,
};
use arany::{CheckId, RecordFormat, RecordKind, RecordStep};
use arany::{Effort, StateRoot, validate_native_model_id};
use aws_lc_rs::hmac;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::{Uuid, Variant, Version};

pub(crate) const TOKEN_RECORD_SCHEMA: u8 = 1;

const MAX_ACCOUNTS: usize = 8;
const MAX_MODEL_CHECKS: usize = 64;
const MODEL_CHECK_AGE_SECONDS: u64 = 24 * 60 * 60;
const MODEL_CHECK_VERSION: &str = "chatgpt-goal-contract-strict-stream-conformance-journal-v38";
const ACCOUNT_ADMISSION_VERSION: &str = "chatgpt-consented-account-admission-v37";
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

pub(crate) struct AccountInspection {
    pub(crate) id: Uuid,
    pub(crate) storage: AccountStorage,
    pub(crate) access: AccountAccess,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum AccountAccess {
    Connected,
    Disconnected,
    PlanDisabled,
    RenewalPending,
    SignOutPending,
}

pub(crate) struct DisabledSignInOutcome {
    pub(crate) id: Uuid,
    pub(crate) local_cleared: bool,
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
    fn valid_for(&self, storage: AccountStorage) -> bool {
        self.schema == TOKEN_RECORD_SCHEMA
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
    signout_pending: bool,
    disconnected: bool,
    plan_permission_missing: bool,
    #[serde(deserialize_with = "Option::deserialize")]
    token: Option<TokenRecord>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelCheck {
    check_id: CheckId,
    account_id: Uuid,
    fingerprint: [u8; 32],
    checked_at_sec: u64,
    expires_at_sec: u64,
    // Optional in the current format: absent or null means no window.
    #[serde(default)]
    context_window_tokens: Option<u32>,
}

impl ModelCheck {
    fn current_at(&self, now: u64) -> bool {
        self.checked_at_sec <= now && now < self.expires_at_sec
    }

    fn valid(&self) -> bool {
        self.account_id.get_version() == Some(Version::SortRand)
            && self.check_id.to_string().parse::<CheckId>().is_ok()
            && self.checked_at_sec != 0
            && self.checked_at_sec.checked_add(MODEL_CHECK_AGE_SECONDS) == Some(self.expires_at_sec)
            && self
                .context_window_tokens
                .is_none_or(|tokens| arany::admitted_context_window(u64::from(tokens)).is_some())
    }
}

impl AccountEntry {
    fn ensure_usable(&self) -> Result<(), AuthorizationError> {
        if self.disconnected {
            return Err(if self.plan_permission_missing {
                AuthorizationError::PermissionMissing
            } else {
                AuthorizationError::NoSelectedAccount
            });
        }
        if self.signout_pending {
            return Err(AuthorizationError::SignOutPending);
        }
        if self.renewal_pending {
            return Err(AuthorizationError::RenewalStorageUncertain);
        }
        Ok(())
    }

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
                        && token.schema == TOKEN_RECORD_SCHEMA
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
    #[serde(deserialize_with = "Option::deserialize")]
    selected: Option<Uuid>,
    accounts: Vec<AccountEntry>,
    #[serde(deserialize_with = "bounded_model_checks")]
    model_checks: Vec<ModelCheck>,
}

fn bounded_model_checks<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ModelCheck>, D::Error> {
    crate::files::bounded_list::<D, ModelCheck, MAX_MODEL_CHECKS>(
        deserializer,
        "too many model checks",
    )
}

pub(crate) const CHATGPT_ACCOUNTS: RecordFormat = RecordFormat {
    kind: RecordKind::ChatgptAccounts,
    field: "schema",
    floor: 1,
    current: 2,
    steps: &[RecordStep {
        from: 1,
        transform: migrate_account_index,
    }],
    accepts: |bytes| AccountIndex::decode(bytes).is_ok(),
};

fn migrate_account_index(mut record: serde_json::Value) -> Option<serde_json::Value> {
    let checks = record.get_mut("model_checks")?.as_array_mut()?;
    if checks.len() > MAX_MODEL_CHECKS {
        return None;
    }
    for check in checks {
        let object = check.as_object_mut()?;
        if object.contains_key("check_id") {
            return None;
        }
        // Validate the old shape before discarding checks that cannot prove a journal link.
        object.insert(
            "check_id".into(),
            "01a15ecf-a92a-7707-8af6-ce3d36320b67".into(),
        );
    }
    record
        .as_object_mut()?
        .insert("schema".into(), CHATGPT_ACCOUNTS.current.into());
    let mut index = AccountIndex::decode(&serde_json::to_vec(&record).ok()?).ok()?;
    index.model_checks.clear();
    serde_json::to_value(index).ok()
}

pub(super) fn retained_checks(state: &StateRoot) -> Result<Vec<CheckId>, arany::StoreError> {
    AccountIndex::read(state)
        .map(|index| {
            index
                .model_checks
                .into_iter()
                .map(|check| check.check_id)
                .collect()
        })
        .map_err(|_| arany::StoreError::InvalidHistory)
}

impl ModelCheck {
    pub(super) fn digest(&self) -> Result<[u8; 32], AuthorizationError> {
        Ok(Sha256::digest(
            serde_json::to_vec(self).map_err(|_| AuthorizationError::EvidenceUnavailable)?,
        )
        .into())
    }
}

impl AccountIndex {
    fn account_position(&self, id: Uuid) -> Result<usize, AuthorizationError> {
        self.accounts
            .iter()
            .position(|entry| entry.id == id)
            .ok_or(AuthorizationError::InvalidIdentity)
    }

    fn matching_account_position(
        &self,
        client_id: &str,
        host_id: Uuid,
        subject: &str,
        storage: AccountStorage,
    ) -> Result<Option<usize>, AuthorizationError> {
        let Some(position) = self
            .accounts
            .iter()
            .position(|entry| entry.client_id == client_id)
        else {
            return Ok(None);
        };
        let entry = &self.accounts[position];
        if entry.subject != subject || entry.host_id != host_id || entry.storage != storage {
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
        self.model_checks.retain(|check| check.account_id != id);
        self.selected = Some(id);
    }

    fn empty() -> Self {
        Self {
            schema: CHATGPT_ACCOUNTS.current as u8,
            selected: None,
            accounts: Vec::new(),
            model_checks: Vec::new(),
        }
    }

    fn validate(&self) -> Result<(), AuthorizationError> {
        if u64::from(self.schema) != CHATGPT_ACCOUNTS.current
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
            .read_current(&CHATGPT_ACCOUNTS)
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

pub(super) fn model_check_fingerprint(
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
    Ok(token)
}

fn verify_selected_snapshot(
    index: &AccountIndex,
    selected: &SelectedAccount,
) -> Result<(), AuthorizationError> {
    load_selected_snapshot(index, selected).map(|_| ())
}

fn verify_check_snapshot(
    state: &StateRoot,
    index: &AccountIndex,
    selected: &SelectedAccount,
) -> Result<(), AuthorizationError> {
    let token = load_selected_snapshot(index, selected)?;
    registration::verify_host_at(state, selected.credentials.host_id)?;
    if token.credentials.access_token != selected.credentials.access_token
        || token.credentials.refresh_token != selected.credentials.refresh_token
        || token.credentials.access_expires_at_unix != selected.credentials.access_expires_at_unix
    {
        return Err(AuthorizationError::SelectedAccountChanged);
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

pub(super) async fn prepare_model_check(
    workspace: PathBuf,
    selected: SelectedAccount,
    selection: (String, Effort),
    context_window_tokens: Option<u32>,
    check_id: CheckId,
) -> Result<ModelCheck, AuthorizationError> {
    tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        state
            .with_account_replacement_lock(&workspace, || {
                verify_check_snapshot(&state, &AccountIndex::read(&state)?, &selected)
            })
            .map_err(|_| AuthorizationError::Unavailable)??;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthorizationError::EvidenceUnavailable)?
            .as_secs();
        new_model_check(
            &selected,
            (&selection.0, selection.1),
            context_window_tokens,
            check_id,
            now,
        )
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

fn new_model_check(
    selected: &SelectedAccount,
    selection: (&str, Effort),
    context_window_tokens: Option<u32>,
    check_id: CheckId,
    now: u64,
) -> Result<ModelCheck, AuthorizationError> {
    Ok(ModelCheck {
        check_id,
        account_id: selected.id,
        fingerprint: model_check_fingerprint(selected, selection.0, selection.1)?,
        checked_at_sec: now,
        expires_at_sec: now
            .checked_add(MODEL_CHECK_AGE_SECONDS)
            .ok_or(AuthorizationError::EvidenceUnavailable)?,
        context_window_tokens,
    })
}

pub(super) async fn record_model_check(
    workspace: PathBuf,
    selected: SelectedAccount,
    record: ModelCheck,
) -> Result<(), AuthorizationError> {
    tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        publish_model_check_at(&state, &workspace, &selected, record)
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)?
}

fn publish_model_check_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    record: ModelCheck,
) -> Result<(), AuthorizationError> {
    let fingerprint = record.fingerprint;
    let now = record.checked_at_sec;
    let digest = record.digest()?;
    state
        .with_account_replacement_lock(workspace, || {
            let mut index = AccountIndex::read(state)?;
            verify_check_snapshot(state, &index, selected)?;
            if !state
                .check_publication_pending(record.check_id, fingerprint, digest)
                .map_err(|_| AuthorizationError::EvidenceUnavailable)?
            {
                return Err(AuthorizationError::EvidenceUnavailable);
            }
            index
                .model_checks
                .retain(|check| check.expires_at_sec > now && check.fingerprint != fingerprint);
            if index.model_checks.len() == MAX_MODEL_CHECKS {
                return Err(AuthorizationError::EvidenceUnavailable);
            }
            index.model_checks.push(record);
            index.write(state)?;
            let saved = AccountIndex::read(state)?;
            verify_check_snapshot(state, &saved, selected)?;
            if !saved.model_checks.iter().any(|check| {
                check.fingerprint == fingerprint && check.digest().ok() == Some(digest)
            }) {
                return Err(AuthorizationError::EvidenceUnavailable);
            }
            Ok(())
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(super) struct AdmittedModel {
    pub(super) selected: SelectedAccount,
    pub(super) fingerprint: [u8; 32],
    pub(super) context_window_tokens: Option<u32>,
}

struct RunModel<'a> {
    slug: &'a str,
    effort: Effort,
    now: u64,
}

pub(super) async fn admitted_selected_for_run(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
    model: String,
    effort: Effort,
) -> Result<AdmittedModel, AuthorizationError> {
    let selected = selected_for_use(workspace.clone(), expected_account_id).await?;
    let models = super::catalog::read_for_run(super::catalog::list_models(
        &selected.credentials,
        &selected.consent,
        selected.storage,
    ))
    .await;
    let admitted = selected.clone();
    let (fingerprint, context_window_tokens) = tokio::task::spawn_blocking(move || {
        let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
        let state = StateRoot::open_existing(&path).map_err(|_| AuthorizationError::Unavailable)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthorizationError::EvidenceUnavailable)?
            .as_secs();
        admitted_model_at(
            &state,
            &workspace,
            &admitted,
            RunModel {
                slug: &model,
                effort,
                now,
            },
            models.as_deref(),
        )
    })
    .await
    .map_err(|_| AuthorizationError::Unavailable)??;
    Ok(AdmittedModel {
        selected,
        fingerprint,
        context_window_tokens,
    })
}

fn admitted_model_at(
    state: &StateRoot,
    workspace: &Path,
    selected: &SelectedAccount,
    selection: RunModel<'_>,
    catalog: Result<&[super::ChatGptModel], &AuthorizationError>,
) -> Result<([u8; 32], Option<u32>), AuthorizationError> {
    let fingerprint = account_fingerprint(
        selected,
        selection.slug,
        selection.effort,
        ACCOUNT_ADMISSION_VERSION,
        "no-preflight-inference;strict-stream-outcomes;local-output-bound",
    )?;
    state
        .with_account_replacement_lock(workspace, || {
            let index = AccountIndex::read(state)?;
            verify_selected_snapshot(&index, selected)?;
            let window = match catalog {
                Ok(models) => super::catalog::model_window(models, selection.slug)
                    .ok()
                    .flatten(),
                Err(_) => {
                    checked_model_window(state, &index, selected, selection.slug, selection.now)?
                }
            };
            Ok((fingerprint, window))
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn checked_model_window(
    state: &StateRoot,
    index: &AccountIndex,
    selected: &SelectedAccount,
    slug: &str,
    now: u64,
) -> Result<Option<u32>, AuthorizationError> {
    let fingerprints = Effort::ALL
        .into_iter()
        .map(|effort| model_check_fingerprint(selected, slug, effort))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(index
        .model_checks
        .iter()
        .filter(|check| {
            check.account_id == selected.id
                && fingerprints.contains(&check.fingerprint)
                && check.current_at(now)
                && check.digest().is_ok_and(|digest| {
                    state
                        .check_evidence_valid(check.check_id, check.fingerprint, digest)
                        .unwrap_or(false)
                })
        })
        .max_by_key(|check| check.checked_at_sec)
        .and_then(|check| check.context_window_tokens))
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
    .map_err(|_| {
        AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown,
        )
    })?
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
    .map_err(|_| {
        AuthorizationError::CredentialStore(
            crate::accounts::credentials::CredentialError::WriteOutcomeUnknown,
        )
    })?
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
            let previous = index.matching_account_position(
                &identity.client_id,
                identity.host_id,
                &identity.subject,
                storage,
            )?;
            let id = if let Some(position) = previous {
                let entry = &mut index.accounts[position];
                entry.token = None;
                entry.renewal_pending = false;
                entry.signout_pending = false;
                entry.disconnected = true;
                entry.plan_permission_missing = true;
                entry.id
            } else {
                index.ensure_account_capacity()?;
                index.push_account(AccountEntry {
                    id: Uuid::now_v7(),
                    host_id: identity.host_id,
                    client_id: identity.client_id.clone(),
                    subject: identity.subject,
                    storage,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: true,
                    plan_permission_missing: true,
                    token: None,
                })
            };
            index.select_account(id);
            index.write(state)?;
            registration::complete_issued_client_at(state, &identity.client_id, identity.host_id)
                .map_err(uncertain_account_publication)?;
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
        schema: TOKEN_RECORD_SCHEMA,
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
            let position = index.matching_account_position(
                client_id,
                host_id,
                subject,
                AccountStorage::PrivateFile,
            )?;
            let id = if let Some(position) = position {
                let entry = &mut index.accounts[position];
                entry.token = Some(token);
                entry.renewal_pending = false;
                entry.signout_pending = false;
                entry.disconnected = false;
                entry.plan_permission_missing = false;
                entry.id
            } else {
                index.ensure_account_capacity()?;
                index.push_account(AccountEntry {
                    id: Uuid::now_v7(),
                    host_id,
                    client_id: client_id.clone(),
                    subject: subject.clone(),
                    storage: AccountStorage::PrivateFile,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: false,
                    plan_permission_missing: false,
                    token: Some(token),
                })
            };
            index.select_account(id);
            index.write(state)?;
            registration::complete_issued_client_at(state, &completed_client_id, host_id)
                .map_err(uncertain_account_publication)?;
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
    let token = TokenRecord {
        schema: TOKEN_RECORD_SCHEMA,
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
            let position = index.matching_account_position(
                client_id,
                host_id,
                subject,
                AccountStorage::Keyring,
            )?;
            let id = if let Some(position) = position {
                let entry = &mut index.accounts[position];
                let id = entry.id;
                entry.renewal_pending = true;
                index.write(state)?;
                id
            } else {
                index.ensure_account_capacity()?;
                index.push_account(AccountEntry {
                    id: Uuid::now_v7(),
                    host_id,
                    client_id: client_id.clone(),
                    subject: subject.clone(),
                    storage: AccountStorage::Keyring,
                    renewal_pending: false,
                    signout_pending: false,
                    disconnected: false,
                    plan_permission_missing: false,
                    token: None,
                })
            };
            write(&keyring_slot(client_id), &record).map_err(|error| {
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
                .ok_or_else(|| {
                    uncertain_account_publication(AuthorizationError::InvalidIdentity)
                })?;
            entry.renewal_pending = false;
            entry.signout_pending = false;
            entry.disconnected = false;
            entry.plan_permission_missing = false;
            index.select_account(id);
            index.write(state).map_err(uncertain_account_publication)?;
            registration::complete_issued_client_at(state, &completed_client_id, host_id)
                .map_err(uncertain_account_publication)?;
            Ok(id)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
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
    if token.schema != TOKEN_RECORD_SCHEMA
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
    Ok((entry.storage == AccountStorage::Keyring).then(|| keyring_slot(&entry.client_id)))
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
            access: if entry.plan_permission_missing {
                AccountAccess::PlanDisabled
            } else if entry.signout_pending {
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
                .read_current(&CHATGPT_ACCOUNTS)
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
    read_keyring: impl FnOnce(&str, &str, Uuid) -> Result<TokenRecord, AuthorizationError>,
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
        impl FnOnce(&str, &str, Uuid) -> Result<TokenRecord, AuthorizationError>,
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
            let slot = keyring_slot(&entry.client_id);
            if entry.disconnected {
                let local_cleared = entry.storage == AccountStorage::PrivateFile
                    || (actions.delete_keyring)(&slot).is_ok();
                if !local_cleared {
                    return Err(AuthorizationError::SignOutStorageUncertain);
                }
                return Ok(SignOutOutcome {
                    remote_confirmed: false,
                    local_cleared,
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
                    (actions.read_keyring)(&entry.client_id, &entry.subject, entry.host_id).ok()
                }
                AccountStorage::PrivateFile => entry.token.clone(),
            };
            let remote_confirmed =
                token
                    .filter(|token| token.valid_for(storage))
                    .is_some_and(|token| {
                        actions
                            .revoke
                            .is_some_and(|revoke| revoke(&token.credentials).is_ok())
                    });
            index.accounts[position].token = None;
            index.accounts[position].signout_pending = false;
            index.accounts[position].disconnected = true;
            index
                .write(state)
                .map_err(|_| AuthorizationError::SignOutStorageUncertain)?;
            let local_cleared =
                storage == AccountStorage::PrivateFile || (actions.delete_keyring)(&slot).is_ok();
            if !local_cleared {
                return Err(AuthorizationError::SignOutStorageUncertain);
            }
            Ok(SignOutOutcome {
                remote_confirmed,
                local_cleared,
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
    crate::files::open_optional_state(&path)
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
            if index.selected != Some(id) {
                return Err(AuthorizationError::InvalidIdentity);
            }
            let position = index.account_position(id)?;
            let entry = &index.accounts[position];
            entry.ensure_usable()?;
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
                schema: TOKEN_RECORD_SCHEMA,
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
