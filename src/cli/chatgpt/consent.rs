use super::{AuthorizationError, VerifiedCredentials};
use crate::cli::credentials::AccountStorage;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const WARNING_VERSION: u8 = 4;
const PLAN_RISK_WARNING: &str = "ChatGPT-plan requests cannot set a remote output-token limit. Arany can stop reading after local time and byte limits, but that does not guarantee a cap on generated tokens or plan usage. You can set an app-specific usage limit in ChatGPT Settings > Usage; it is not a per-request cap.";
#[cfg(target_os = "linux")]
const KEYRING_RISK_WARNING: &str = "An unlocked Linux Secret Service item may be read by another application running as your user. The keyring does not make this token private to Arany.";
#[cfg(not(target_os = "linux"))]
const KEYRING_RISK_WARNING: &str =
    "The OS credential store protects this token for your user account, not exclusively for Arany.";
const FILE_RISK_WARNING: &str = "The private account file is not encrypted. Other processes running as your user, and privileged processes, may read its tokens; file permissions do not prevent that.";

pub(crate) struct RiskPrompt {
    storage: AccountStorage,
}

impl RiskPrompt {
    pub(crate) fn new(storage: AccountStorage) -> Self {
        Self { storage }
    }

    pub(crate) fn text(&self) -> String {
        warning(self.storage)
    }

    pub(crate) fn accept(self, input: &str) -> Result<RiskAcknowledgment, AuthorizationError> {
        if input == "Accept" {
            Ok(RiskAcknowledgment {
                storage: self.storage,
            })
        } else {
            Err(AuthorizationError::ConsentRequired)
        }
    }
}

fn warning(storage: AccountStorage) -> String {
    let storage_warning = match storage {
        AccountStorage::Keyring => KEYRING_RISK_WARNING,
        AccountStorage::PrivateFile => FILE_RISK_WARNING,
    };
    format!("{PLAN_RISK_WARNING} {storage_warning} Choose Accept to continue, or Back to cancel.")
}

pub(crate) struct RiskAcknowledgment {
    storage: AccountStorage,
}

impl RiskAcknowledgment {
    pub(crate) fn storage(&self) -> AccountStorage {
        self.storage
    }

    pub(crate) fn bind(
        self,
        credentials: &VerifiedCredentials,
    ) -> Result<ConsentReceipt, AuthorizationError> {
        let accepted_at_sec = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthorizationError::ConsentRequired)?
            .as_secs();
        Ok(ConsentReceipt {
            warning_version: WARNING_VERSION,
            warning_digest: Sha256::digest(warning(self.storage).as_bytes()).into(),
            storage: self.storage,
            client_id: credentials.client_id.clone(),
            subject: credentials.subject.clone(),
            host_id: credentials.host_id,
            accepted_at_sec,
        })
    }
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConsentReceipt {
    warning_version: u8,
    warning_digest: [u8; 32],
    storage: AccountStorage,
    client_id: String,
    subject: String,
    host_id: Uuid,
    accepted_at_sec: u64,
}

impl ConsentReceipt {
    pub(crate) fn fingerprint_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        serde_json::to_vec(self).map_err(|_| AuthorizationError::InvalidIdentity)
    }

    pub(crate) fn matches(
        &self,
        credentials: &VerifiedCredentials,
        storage: AccountStorage,
    ) -> bool {
        self.warning_version == WARNING_VERSION
            && self.warning_digest == <[u8; 32]>::from(Sha256::digest(warning(storage).as_bytes()))
            && self.storage == storage
            && self.client_id == credentials.client_id
            && self.subject == credentials.subject
            && self.host_id == credentials.host_id
            && self.accepted_at_sec != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_credentials() -> VerifiedCredentials {
        VerifiedCredentials {
            client_id: "oaiapp_synthetic".into(),
            host_id: Uuid::now_v7(),
            subject: "subject-one".into(),
            id_token: "synthetic-id".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at_unix: 1_800_000_000,
        }
    }

    #[test]
    fn only_explicit_acceptance_can_be_bound_to_verified_registration() {
        for input in ["", "Back", "yes", "I ACCEPT", " Accept", "Accept\n"] {
            assert!(matches!(
                RiskPrompt::new(AccountStorage::Keyring).accept(input),
                Err(AuthorizationError::ConsentRequired)
            ));
        }
        let credentials = synthetic_credentials();
        let warning = RiskPrompt::new(AccountStorage::Keyring).text();
        assert!(warning.is_ascii());
        assert!(warning.len() <= 1024);
        assert!(warning.contains("remote output-token limit"));
        assert!(warning.contains("app-specific usage limit"));
        assert!(warning.contains("another application running as your user"));
        let receipt = RiskPrompt::new(AccountStorage::Keyring)
            .accept("Accept")
            .expect("explicit acceptance")
            .bind(&credentials)
            .expect("bound receipt");
        assert!(receipt.matches(&credentials, AccountStorage::Keyring));
        assert!(!receipt.matches(&credentials, AccountStorage::PrivateFile));
        let mut changed = synthetic_credentials();
        changed.host_id = credentials.host_id;
        changed.subject = "subject-two".into();
        assert!(!receipt.matches(&changed, AccountStorage::Keyring));
        changed.subject = credentials.subject.clone();
        changed.client_id = "oaiapp_other".into();
        assert!(!receipt.matches(&changed, AccountStorage::Keyring));
        changed.client_id = credentials.client_id.clone();
        changed.host_id = Uuid::now_v7();
        assert!(!receipt.matches(&changed, AccountStorage::Keyring));
    }

    #[test]
    fn persisted_receipt_fails_closed_after_warning_change() {
        let credentials = synthetic_credentials();
        let receipt = RiskPrompt::new(AccountStorage::PrivateFile)
            .accept("Accept")
            .unwrap()
            .bind(&credentials)
            .unwrap();
        assert!(
            RiskPrompt::new(AccountStorage::PrivateFile)
                .text()
                .contains("not encrypted")
        );
        let file_warning = RiskPrompt::new(AccountStorage::PrivateFile).text();
        assert!(file_warning.is_ascii());
        assert!(file_warning.len() <= 1024);
        for version in [1, 3, WARNING_VERSION + 1] {
            let mut value = serde_json::to_value(&receipt).expect("serializable receipt");
            value["warning_version"] = version.into();
            let changed: ConsentReceipt = serde_json::from_value(value).expect("receipt shape");
            assert!(!changed.matches(&credentials, AccountStorage::PrivateFile));
        }
        let mut value = serde_json::to_value(&receipt).expect("serializable receipt");
        value["warning_digest"][0] = (value["warning_digest"][0].as_u64().unwrap() ^ 1).into();
        let changed: ConsentReceipt = serde_json::from_value(value).expect("changed warning");
        assert!(!changed.matches(&credentials, AccountStorage::PrivateFile));
        let mut value = serde_json::to_value(&receipt).expect("serializable receipt");
        value["storage"] = serde_json::json!("keyring");
        let changed: ConsentReceipt = serde_json::from_value(value).expect("changed backend");
        assert!(!changed.matches(&credentials, AccountStorage::PrivateFile));
    }
}
