use super::{AuthorizationAttempt, AuthorizationError, CodeExchange, account, valid_client_id};
use arany::{StateRoot, StoreError};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::{Uuid, Variant, Version};

const MIGRATED_REGISTRATION: &[u8] = b"{\"schema\":0,\"migrated\":\"os_user\"}";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u8,
    host_id: Uuid,
    issued_client_id: Option<String>,
}

impl Record {
    fn validate(&self) -> Result<(), AuthorizationError> {
        if self.schema != 1
            || self.host_id.get_version() != Some(Version::Random)
            || self.host_id.get_variant() != Variant::RFC4122
            || self
                .issued_client_id
                .as_deref()
                .is_some_and(|id| !valid_client_id(id))
        {
            return Err(AuthorizationError::InvalidIdentity);
        }
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        let record: Self =
            serde_json::from_slice(bytes).map_err(|_| AuthorizationError::InvalidIdentity)?;
        record.validate()?;
        Ok(record)
    }

    fn encode(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| AuthorizationError::Unavailable)?;
        if bytes.len() > StateRoot::MAX_CHATGPT_REGISTRATION_BYTES {
            return Err(AuthorizationError::Unavailable);
        }
        Ok(bytes)
    }
}

pub(crate) struct Registration(Record);

impl Registration {
    pub(crate) fn open_or_create(workspace: &Path) -> Result<Self, AuthorizationError> {
        migrate_legacy(workspace)?;
        let state = default_state()?;
        Self::at(&state, workspace)
    }

    pub(crate) fn host_id(&self) -> Uuid {
        self.0.host_id
    }

    pub(crate) fn has_unfinished_sign_in(&self) -> bool {
        self.0.issued_client_id.is_some()
    }

    fn at(state: &StateRoot, workspace: &Path) -> Result<Self, AuthorizationError> {
        state
            .with_account_replacement_lock(workspace, || {
                let saved_host = account::saved_host_at(state)?;
                if let Some(bytes) = state
                    .read_chatgpt_registration_record()
                    .map_err(|_| AuthorizationError::Unavailable)?
                {
                    let mut record = Record::decode(&bytes)?;
                    if saved_host.is_some_and(|host| host != record.host_id) {
                        return Err(AuthorizationError::InvalidIdentity);
                    }
                    if let Some(client_id) = record.issued_client_id.as_deref()
                        && account::contains_saved_client_at(state, client_id, record.host_id)?
                    {
                        // Account publication may have committed before retry-slot cleanup.
                        record.issued_client_id = None;
                        state
                            .replace_chatgpt_registration_record(&record.encode()?)
                            .map_err(|_| AuthorizationError::Unavailable)?;
                    }
                    return Ok(Self(record));
                }
                if saved_host.is_some() {
                    return Err(AuthorizationError::InvalidIdentity);
                }
                let mut random = [0u8; 16];
                getrandom::fill(&mut random).map_err(|_| AuthorizationError::Unavailable)?;
                random[6] = random[6] & 0x0f | 0x40;
                random[8] = random[8] & 0x3f | 0x80;
                let record = Record {
                    schema: 1,
                    host_id: Uuid::from_bytes(random),
                    issued_client_id: None,
                };
                state
                    .replace_chatgpt_registration_record(&record.encode()?)
                    .map_err(|_| AuthorizationError::Unavailable)?;
                Ok(Self(record))
            })
            .map_err(|_| AuthorizationError::Unavailable)?
    }

    pub(crate) fn authorization_attempt(
        &self,
        callback_port: u16,
    ) -> Result<AuthorizationAttempt, AuthorizationError> {
        match &self.0.issued_client_id {
            Some(client_id) => AuthorizationAttempt::for_new_registration_retry(
                self.0.host_id,
                client_id.clone(),
                callback_port,
            ),
            None => AuthorizationAttempt::new(self.0.host_id, callback_port),
        }
    }
}

pub(super) fn verify_host_at(state: &StateRoot, host_id: Uuid) -> Result<(), AuthorizationError> {
    let Some(bytes) = state
        .read_chatgpt_registration_record()
        .map_err(|_| AuthorizationError::Unavailable)?
    else {
        return Ok(());
    };
    if Record::decode(&bytes)?.host_id != host_id {
        return Err(AuthorizationError::InvalidIdentity);
    }
    Ok(())
}

pub(super) fn complete_issued_client_at(
    state: &StateRoot,
    client_id: &str,
    host_id: Uuid,
) -> Result<(), AuthorizationError> {
    let Some(bytes) = state
        .read_chatgpt_registration_record()
        .map_err(|_| AuthorizationError::Unavailable)?
    else {
        return Ok(());
    };
    let mut record = Record::decode(&bytes)?;
    if record.host_id != host_id {
        return Err(AuthorizationError::InvalidIdentity);
    }
    if record.issued_client_id.as_deref() != Some(client_id) {
        return Ok(());
    }
    record.issued_client_id = None;
    state
        .replace_chatgpt_registration_record(&record.encode()?)
        .map_err(|_| AuthorizationError::Unavailable)
}

pub(super) fn retain_issued_client(
    workspace: &Path,
    exchange: &CodeExchange,
) -> Result<(), AuthorizationError> {
    migrate_legacy(workspace)?;
    let state = default_state()?;
    retain_at(&state, workspace, exchange)
}

fn retain_at(
    state: &StateRoot,
    workspace: &Path,
    exchange: &CodeExchange,
) -> Result<(), AuthorizationError> {
    state
        .with_account_replacement_lock(workspace, || {
            let bytes = state
                .read_chatgpt_registration_record()
                .map_err(|_| AuthorizationError::Unavailable)?
                .ok_or(AuthorizationError::InvalidIdentity)?;
            let mut record = Record::decode(&bytes)?;
            if record.host_id != exchange.host_id || !valid_client_id(&exchange.client_id) {
                return Err(AuthorizationError::InvalidIdentity);
            }
            if let Some(existing) = &record.issued_client_id {
                return if existing == &exchange.client_id {
                    Ok(())
                } else {
                    Err(AuthorizationError::InvalidIdentity)
                };
            }
            record.issued_client_id = Some(exchange.client_id.clone());
            state
                .replace_chatgpt_registration_record(&record.encode()?)
                .map_err(|_| AuthorizationError::Unavailable)
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

fn default_state() -> Result<StateRoot, AuthorizationError> {
    let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
    StateRoot::admit(&path).map_err(|_| AuthorizationError::Unavailable)
}

fn migrate_legacy(workspace: &Path) -> Result<(), AuthorizationError> {
    let account_path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
    let old_path = StateRoot::default_path().map_err(|_| AuthorizationError::Unavailable)?;
    migrate_legacy_at(&account_path, &old_path, workspace)
}

fn migrate_legacy_at(
    account_path: &Path,
    old_path: &Path,
    workspace: &Path,
) -> Result<(), AuthorizationError> {
    if account_path == old_path {
        return Ok(());
    }
    let old = match StateRoot::open_existing(old_path) {
        Ok(state) => state,
        Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::symlink_metadata(old_path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(AuthorizationError::Unavailable),
            };
        }
        Err(_) => return Err(AuthorizationError::Unavailable),
    };
    old.with_account_replacement_lock(workspace, || {
        let record = old
            .read_chatgpt_registration_record()
            .map_err(|_| AuthorizationError::Unavailable)?;
        let Some(record) = record else {
            return Ok(());
        };
        if record == MIGRATED_REGISTRATION {
            return Ok(());
        }
        Record::decode(&record)?;
        let current =
            StateRoot::admit(account_path).map_err(|_| AuthorizationError::Unavailable)?;
        current
            .with_account_replacement_lock(workspace, || {
                match current
                    .read_chatgpt_registration_record()
                    .map_err(|_| AuthorizationError::Unavailable)?
                {
                    Some(existing) if existing != record => {
                        return Err(AuthorizationError::RegistrationConflict);
                    }
                    Some(_) => {}
                    None => current
                        .replace_chatgpt_registration_record(&record)
                        .map_err(|_| AuthorizationError::Unavailable)?,
                }
                old.replace_chatgpt_registration_record(MIGRATED_REGISTRATION)
                    .map_err(|_| AuthorizationError::Unavailable)
            })
            .map_err(|_| AuthorizationError::Unavailable)?
    })
    .map_err(|_| AuthorizationError::Unavailable)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{
        chatgpt::{VerifiedCredentials, consent::RiskPrompt},
        credentials::AccountStorage,
    };
    use reqwest::Url;

    fn workspace_and_state() -> (tempfile::TempDir, std::path::PathBuf, StateRoot) {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state");
        (temp, workspace, state)
    }

    fn exchange(registration: &Registration, client_id: &str) -> CodeExchange {
        let attempt = registration.authorization_attempt(1455).expect("attempt");
        let callback = format!(
            "{}?state={}&code=synthetic-code&client_id={client_id}",
            attempt.redirect_uri, attempt.state
        );
        attempt.accept_callback(&callback).expect("callback")
    }

    #[test]
    fn host_and_issued_client_survive_restart_before_redemption() {
        let (_temp, workspace, state) = workspace_and_state();
        let first = Registration::at(&state, &workspace).expect("created registration");
        assert_eq!(first.0.host_id.get_version(), Some(Version::Random));
        let first_url = first
            .authorization_attempt(1455)
            .unwrap()
            .authorization_url();
        assert!(
            first_url
                .as_str()
                .contains("client_id=dynamic_agent_client")
        );
        let code = exchange(&first, "oaiapp_issued");
        retain_at(&state, &workspace, &code).expect("retain before redeem");

        let reopened = StateRoot::open_existing(state.path()).expect("reopened state");
        let resumed = Registration::at(&reopened, &workspace).expect("saved registration");
        assert_eq!(resumed.0.host_id, first.0.host_id);
        assert_eq!(resumed.0.issued_client_id.as_deref(), Some("oaiapp_issued"));
        let url = Url::parse(
            resumed
                .authorization_attempt(1456)
                .unwrap()
                .authorization_url()
                .as_str(),
        )
        .unwrap();
        let fields = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], "oaiapp_issued");
        assert_eq!(
            fields["ext_agent_host_id"],
            format!("urn:uuid:{}", first.0.host_id)
        );
        assert!(!fields.contains_key("agent_name_hint"));
        let persisted = reopened
            .read_chatgpt_registration_record()
            .unwrap()
            .unwrap();
        assert!(
            !persisted
                .windows(code.code.len())
                .any(|part| part == code.code.as_bytes())
        );
        assert!(
            !persisted
                .windows(code.verifier.len())
                .any(|part| part == code.verifier.as_bytes())
        );

        let credentials = VerifiedCredentials {
            client_id: "oaiapp_issued".into(),
            host_id: first.0.host_id,
            subject: "first-subject".into(),
            id_token: "signed-first-hint".into(),
            access_token: "first-access".into(),
            refresh_token: "first-refresh".into(),
            access_expires_at_unix: 1_900_000_000,
        };
        let consent = RiskPrompt::new(AccountStorage::PrivateFile)
            .accept("Accept")
            .unwrap()
            .bind(&credentials)
            .unwrap();
        account::save_private_file_at(&reopened, &workspace, credentials, consent, None)
            .expect("verified account save clears pending client");
        let cleared = Registration::at(&reopened, &workspace).expect("completed registration");
        assert_eq!(cleared.0.issued_client_id, None);
        assert!(
            cleared
                .authorization_attempt(1457)
                .unwrap()
                .authorization_url()
                .as_str()
                .contains("client_id=dynamic_agent_client")
        );

        reopened
            .replace_chatgpt_registration_record(&persisted)
            .expect("simulate death after account save before marker clear");
        let recovered = Registration::at(&reopened, &workspace).expect("reconciled account");
        assert_eq!(recovered.0.issued_client_id, None);
        let next = exchange(&recovered, "oaiapp_second");
        retain_at(&reopened, &workspace, &next).expect("second account registration");
        assert_eq!(
            Registration::at(&reopened, &workspace)
                .unwrap()
                .0
                .issued_client_id
                .as_deref(),
            Some("oaiapp_second")
        );
        let second_credentials = VerifiedCredentials {
            client_id: "oaiapp_second".into(),
            host_id: first.0.host_id,
            subject: "second-subject".into(),
            id_token: "signed-second-hint".into(),
            access_token: "second-access".into(),
            refresh_token: "second-refresh".into(),
            access_expires_at_unix: 1_900_000_000,
        };
        let second_consent = RiskPrompt::new(AccountStorage::PrivateFile)
            .accept("Accept")
            .unwrap()
            .bind(&second_credentials)
            .unwrap();
        account::save_private_file_at(
            &reopened,
            &workspace,
            second_credentials,
            second_consent,
            None,
        )
        .expect("second verified account");
        assert!(
            Registration::at(&reopened, &workspace)
                .unwrap()
                .authorization_attempt(1458)
                .unwrap()
                .authorization_url()
                .as_str()
                .contains("client_id=dynamic_agent_client")
        );

        let conflicting = Record {
            schema: 1,
            host_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174001").unwrap(),
            issued_client_id: None,
        };
        reopened
            .replace_chatgpt_registration_record(&conflicting.encode().unwrap())
            .unwrap();
        assert!(matches!(
            Registration::at(&reopened, &workspace),
            Err(AuthorizationError::InvalidIdentity)
        ));
    }

    #[test]
    fn stale_or_mismatched_registration_cannot_replace_issued_client() {
        let (_temp, workspace, state) = workspace_and_state();
        let stale = Registration::at(&state, &workspace).expect("registration");
        let accepted = exchange(&stale, "oaiapp_first");
        retain_at(&state, &workspace, &accepted).expect("first client");
        let different = exchange(&stale, "oaiapp_other");
        assert_eq!(
            retain_at(&state, &workspace, &different),
            Err(AuthorizationError::InvalidIdentity)
        );
        let mut wrong_host = accepted;
        wrong_host.host_id = Uuid::now_v7();
        assert_eq!(
            retain_at(&state, &workspace, &wrong_host),
            Err(AuthorizationError::InvalidIdentity)
        );
        let persisted = Registration::at(&state, &workspace).expect("unchanged registration");
        assert_eq!(
            persisted.0.issued_client_id.as_deref(),
            Some("oaiapp_first")
        );
    }

    #[test]
    fn malformed_registration_fails_closed() {
        let (_temp, workspace, state) = workspace_and_state();
        state
            .replace_chatgpt_registration_record(b"{\"schema\":2}")
            .expect("synthetic malformed record");
        assert!(matches!(
            Registration::at(&state, &workspace),
            Err(AuthorizationError::InvalidIdentity)
        ));

        let record = Record {
            schema: 1,
            host_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
            issued_client_id: Some("oaiapp_pending".into()),
        };
        state
            .replace_chatgpt_registration_record(&record.encode().unwrap())
            .unwrap();
        state
            .replace_chatgpt_accounts_record(b"not an account index")
            .unwrap();
        assert!(matches!(
            Registration::at(&state, &workspace),
            Err(AuthorizationError::InvalidIdentity)
        ));
    }

    #[test]
    fn legacy_registration_migration_preserves_identity_and_rejects_conflicts() {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let old_path = temp.path().join("legacy");
        let account_path = temp.path().join("user-account");
        let old = StateRoot::admit(&old_path).expect("legacy root");
        let registration = Registration::at(&old, &workspace).expect("legacy registration");
        let source = old
            .read_chatgpt_registration_record()
            .expect("source record")
            .expect("registration");
        let current = StateRoot::admit(&account_path).expect("user account root");
        current
            .replace_chatgpt_registration_record(&source)
            .expect("simulated interrupted copy");
        migrate_legacy_at(&account_path, &old_path, &workspace).expect("finish migration");
        assert_eq!(
            Registration::at(&current, &workspace)
                .expect("migrated registration")
                .0
                .host_id,
            registration.0.host_id
        );
        assert_eq!(
            old.read_chatgpt_registration_record().expect("old marker"),
            Some(MIGRATED_REGISTRATION.to_vec())
        );

        let other_path = temp.path().join("conflicting-legacy");
        let other = StateRoot::admit(&other_path).expect("second legacy root");
        Registration::at(&other, &workspace).expect("different registration");
        assert_eq!(
            migrate_legacy_at(&account_path, &other_path, &workspace),
            Err(AuthorizationError::RegistrationConflict)
        );
        assert!(
            Record::decode(
                &other
                    .read_chatgpt_registration_record()
                    .expect("other record")
                    .expect("unmigrated record")
            )
            .is_ok()
        );
    }
}
