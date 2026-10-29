use super::{AuthorizationAttempt, AuthorizationError, account};
use arany::{RecordFormat, RecordKind, StateRoot};
use serde::{Deserialize, Serialize};
use std::path::Path;
use uuid::{Uuid, Variant, Version};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u8,
    host_id: Uuid,
}

pub(crate) const CHATGPT_REGISTRATION: RecordFormat = RecordFormat {
    kind: RecordKind::ChatgptRegistration,
    field: "schema",
    current: 2,
    accepts: |bytes| Record::decode(bytes).is_ok(),
};

impl Record {
    fn validate(&self) -> Result<(), AuthorizationError> {
        if u64::from(self.schema) != CHATGPT_REGISTRATION.current
            || self.host_id.get_version() != Some(Version::Random)
            || self.host_id.get_variant() != Variant::RFC4122
        {
            return Err(AuthorizationError::InvalidIdentity);
        }
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        let record: Self =
            serde_json::from_slice(bytes).map_err(|_| AuthorizationError::InvalidSavedState)?;
        record
            .validate()
            .map_err(|_| AuthorizationError::InvalidSavedState)?;
        Ok(record)
    }

    fn encode(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| AuthorizationError::Unavailable)?;
        if bytes.len() > RecordKind::ChatgptRegistration.limit() {
            return Err(AuthorizationError::Unavailable);
        }
        Ok(bytes)
    }
}

pub(crate) fn validate_saved_setup(workspace: &Path) -> Result<(), AuthorizationError> {
    let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
    // Without State there is no saved host or registration to validate.
    let Some(state) =
        arany::StateRoot::open_optional(&path).map_err(|_| AuthorizationError::Unavailable)?
    else {
        return Ok(());
    };
    state
        .with_account_replacement_lock(workspace, || {
            let host = account::saved_host_at(&state)?;
            let bytes = state
                .record(CHATGPT_REGISTRATION.kind)
                .read()
                .map_err(|_| AuthorizationError::Unavailable)?;
            match bytes {
                Some(bytes) => {
                    let record = Record::decode(&bytes)?;
                    if host.is_some_and(|host| host != record.host_id) {
                        return Err(AuthorizationError::InvalidSavedState);
                    }
                }
                None if host.is_some() => return Err(AuthorizationError::InvalidSavedState),
                None => {}
            }
            Ok(())
        })
        .map_err(|_| AuthorizationError::Unavailable)?
}

pub(crate) struct Registration(Record);

impl Registration {
    pub(crate) fn open_or_create(workspace: &Path) -> Result<Self, AuthorizationError> {
        let state = default_state()?;
        Self::at(&state, workspace)
    }

    pub(crate) fn host_id(&self) -> Uuid {
        self.0.host_id
    }

    fn at(state: &StateRoot, workspace: &Path) -> Result<Self, AuthorizationError> {
        state
            .with_account_replacement_lock(workspace, || {
                let saved_host = account::saved_host_at(state)?;
                if let Some(bytes) = state
                    .record(CHATGPT_REGISTRATION.kind)
                    .read()
                    .map_err(|_| AuthorizationError::Unavailable)?
                {
                    let record = Record::decode(&bytes)?;
                    if saved_host.is_some_and(|host| host != record.host_id) {
                        return Err(AuthorizationError::InvalidIdentity);
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
                    schema: CHATGPT_REGISTRATION.current as u8,
                    host_id: Uuid::from_bytes(random),
                };
                state
                    .record(RecordKind::ChatgptRegistration)
                    .replace(&record.encode()?)
                    .map_err(super::publication_error)?;
                Ok(Self(record))
            })
            .map_err(|_| AuthorizationError::Unavailable)?
    }

    pub(crate) fn authorization_attempt(
        &self,
        callback_port: u16,
    ) -> Result<AuthorizationAttempt, AuthorizationError> {
        AuthorizationAttempt::new(self.0.host_id, callback_port)
    }
}

pub(super) fn verify_host_at(state: &StateRoot, host_id: Uuid) -> Result<(), AuthorizationError> {
    let Some(bytes) = state
        .record(CHATGPT_REGISTRATION.kind)
        .read()
        .map_err(|_| AuthorizationError::Unavailable)?
    else {
        return Ok(());
    };
    if Record::decode(&bytes)?.host_id != host_id {
        return Err(AuthorizationError::InvalidIdentity);
    }
    Ok(())
}

fn default_state() -> Result<StateRoot, AuthorizationError> {
    let path = StateRoot::account_path().map_err(|_| AuthorizationError::Unavailable)?;
    StateRoot::admit(&path).map_err(|_| AuthorizationError::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_and_state() -> (tempfile::TempDir, std::path::PathBuf, StateRoot) {
        let temp = tempfile::tempdir().expect("temporary root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("workspace");
        let state = StateRoot::admit(&temp.path().join("state")).expect("private state");
        (temp, workspace, state)
    }

    #[test]
    fn registration_directory_sync_failure_is_uncertain() {
        let (_temp, workspace, state) = workspace_and_state();
        arany::FaultPlan::fail_record_sync(state.path().to_owned(), "chatgpt-registration.json");
        assert!(matches!(
            Registration::at(&state, &workspace),
            Err(AuthorizationError::CredentialStore(
                crate::accounts::credentials::CredentialError::WriteOutcomeUnknown
            ))
        ));
        assert!(
            state
                .record(RecordKind::ChatgptRegistration)
                .read()
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn host_survives_reopen_and_signs_in_through_the_public_client() {
        let (_temp, workspace, state) = workspace_and_state();
        let registration = Registration::at(&state, &workspace).unwrap();
        let host_id = registration.host_id();
        assert_eq!(host_id.get_version(), Some(Version::Random));
        let reopened = StateRoot::open_existing(state.path()).unwrap();
        assert_eq!(
            Registration::at(&reopened, &workspace).unwrap().host_id(),
            host_id
        );
        for port in super::super::CALLBACK_PORTS {
            let attempt = registration.authorization_attempt(port).unwrap();
            let url = reqwest::Url::parse(attempt.authorization_url().as_str()).unwrap();
            assert!(
                url.query_pairs().any(|(name, value)| {
                    name == "client_id" && value == super::super::CLIENT_ID
                })
            );
        }
    }

    #[test]
    fn malformed_registration_fails_closed() {
        let (_temp, workspace, state) = workspace_and_state();
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(b"{\"schema\":2}")
            .expect("synthetic malformed record");
        assert!(matches!(
            Registration::at(&state, &workspace),
            Err(AuthorizationError::InvalidSavedState)
        ));

        let record = Record {
            schema: 2,
            host_id: Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap(),
        };
        let mut missing = serde_json::to_value(&record).unwrap();
        missing.as_object_mut().unwrap().remove("host_id");
        assert!(Record::decode(&serde_json::to_vec(&missing).unwrap()).is_err());
        state
            .record(RecordKind::ChatgptRegistration)
            .replace(&record.encode().unwrap())
            .unwrap();
        state
            .record(RecordKind::ChatgptAccounts)
            .replace(b"not an account index")
            .unwrap();
        assert!(matches!(
            Registration::at(&state, &workspace),
            Err(AuthorizationError::InvalidSavedState)
        ));
    }
}
