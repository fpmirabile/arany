use arany::{
    ChatGptAdmission, ChatGptProvenance, ChatGptProvider, Effort, ProviderError,
    validate_native_model_id,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::{Uuid, Variant, Version};

mod account;
mod browser;
mod callback;
mod catalog;
mod consent;
mod exchange;
mod identity;
pub(crate) mod registration;

pub(crate) use account::{
    MAX_TOKEN_RECORD_BYTES, ReauthorizationTarget, save_verified, save_without_plan_permission,
    valid_keyring_record,
};
pub(crate) use browser::open_authorization_url;
pub(crate) use callback::CallbackListener;
pub(crate) use catalog::ChatGptModel;
pub(crate) use consent::{RiskAcknowledgment, RiskPrompt};

pub(crate) async fn selected_models(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
) -> Result<(Uuid, Vec<ChatGptModel>), AuthorizationError> {
    let selected = account::selected_for_use(workspace, expected_account_id).await?;
    let models =
        catalog::list_models(&selected.credentials, &selected.consent, selected.storage).await?;
    Ok((selected.id, models))
}

pub(crate) fn selected_account_id() -> Result<Uuid, AuthorizationError> {
    account::selected_id()
}

pub(crate) fn selected_registration() -> Result<(Uuid, bool), AuthorizationError> {
    account::selected_registration()
}

pub(crate) async fn sign_out_selected(
    workspace: PathBuf,
) -> Result<account::SignOutOutcome, AuthorizationError> {
    account::sign_out_selected(workspace).await
}

pub(crate) fn saved_account_ids() -> Result<(Uuid, Vec<Uuid>), AuthorizationError> {
    account::saved_ids()
}

pub(crate) async fn select_saved_account(
    workspace: PathBuf,
    expected_selected: Uuid,
    target: Uuid,
) -> Result<(), AuthorizationError> {
    account::select_saved(workspace, expected_selected, target).await
}

pub(crate) fn selected_reauthorization_target(
    expected_id: Uuid,
) -> Result<ReauthorizationTarget, AuthorizationError> {
    account::selected_reauthorization_target(expected_id)
}

pub(crate) async fn check_selected_model(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
    model: String,
    effort: Effort,
) -> Result<Uuid, AuthorizationError> {
    validate_native_model_id(&model).map_err(|_| AuthorizationError::InvalidSelection)?;
    let selected = account::selected_for_use(workspace.clone(), expected_account_id).await?;
    account::clear_model_check(workspace.clone(), selected.clone(), model.clone(), effort).await?;
    let models =
        catalog::list_models(&selected.credentials, &selected.consent, selected.storage).await?;
    if !models.iter().any(|row| row.slug == model) {
        return Err(AuthorizationError::ModelUnavailable);
    }
    arany::probe_chatgpt_model(&selected.credentials.access_token, &model, effort)
        .await
        .map_err(|error| match error {
            ProviderError::RemoteHttp(status) => AuthorizationError::ConformanceHttp {
                status,
                advice: admission_http_advice(status),
            },
            ProviderError::RemoteStreamCode(code) => AuthorizationError::ConformanceStream {
                advice: stream_failure_advice(&code),
                code,
            },
            _ => AuthorizationError::ConformanceFailed,
        })?;
    let id = selected.id;
    account::record_model_check(workspace, selected, model, effort).await?;
    Ok(id)
}

fn stream_failure_advice(code: &str) -> &'static str {
    match code {
        "subscription_sharing_usage_limit_exceeded" => {
            "pause ChatGPT plan checks and review ChatGPT Settings > Usage"
        }
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            "keep the saved account and try again later"
        }
        "subscription_sharing_user_not_eligible" => {
            "this account or workspace cannot use ChatGPT plan sharing"
        }
        "subscription_sharing_unsupported_capability" => {
            "review model, effort, and request compatibility"
        }
        "subscription_sharing_route_not_supported"
        | "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context" => {
            "check direct-route integration and granted permissions"
        }
        "subscription_sharing_invalid_user" => {
            "verify the selected account; reconnect only after confirmed revocation"
        }
        _ => "review this Provider error before trying again",
    }
}

fn admission_http_advice(status: u16) -> &'static str {
    match status {
        401 => "verify the selected account and granted direct-use permission",
        403 => "check account, region, and integration policy",
        429 => "pause checks and retry later",
        503 => "direct routing may be unavailable; keep the saved account and retry later",
        500..=599 => "keep the saved account and retry later",
        _ => "review the direct-route request and selected account",
    }
}

struct AdmittedAccount {
    id: Uuid,
    access_token: String,
    evidence_fingerprint: [u8; 32],
}

async fn admitted_account_for_run(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
    model: String,
    effort: Effort,
) -> Result<AdmittedAccount, AuthorizationError> {
    validate_native_model_id(&model).map_err(|_| AuthorizationError::InvalidSelection)?;
    let (selected, evidence_fingerprint) =
        account::admitted_selected_for_run(workspace, expected_account_id, model, effort).await?;
    Ok(AdmittedAccount {
        id: selected.id,
        access_token: selected.credentials.access_token,
        evidence_fingerprint,
    })
}

pub(crate) async fn provider_for_run(
    workspace: PathBuf,
    expected_account_id: Option<Uuid>,
    model: String,
    effort: Effort,
) -> Result<ChatGptProvider, AuthorizationError> {
    let admitted =
        admitted_account_for_run(workspace, expected_account_id, model.clone(), effort).await?;
    ChatGptProvider::from_checked_account(
        model,
        effort,
        admitted.access_token,
        ChatGptProvenance {
            account_id: admitted.id,
            evidence_fingerprint: admitted.evidence_fingerprint,
            admission: ChatGptAdmission::AccountConsent,
        },
    )
    .map_err(|_| AuthorizationError::Unavailable)
}

const AUTHORIZE_ENDPOINT: &str = "https://auth.openai.com/api/accounts/authorize";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const CALLBACK_PATH: &str = "/auth/callback";
const MAX_CALLBACK_BYTES: usize = 4096;
const MAX_CODE_BYTES: usize = 2048;
const MAX_CLIENT_ID_BYTES: usize = 128;
const MAX_SCOPE_BYTES: usize = 512;

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum AuthorizationError {
    #[error("authorization is unavailable")]
    Unavailable,
    #[error("no selected ChatGPT account; run arany --setup")]
    NoSelectedAccount,
    #[error("ChatGPT sign-out is incomplete; retry arany provider logout chatgpt")]
    SignOutPending,
    #[error(
        "ChatGPT sign-out could not clear the local account; retry arany provider logout chatgpt"
    )]
    SignOutStorageUncertain,
    #[error("invalid authorization callback")]
    InvalidCallback,
    #[error("authorization was denied")]
    Denied,
    #[error("authorization code exchange failed")]
    ExchangeFailed,
    #[error("authorization code is unusable; restart sign-in")]
    GrantUnusable,
    #[error("ChatGPT authorization must be renewed")]
    RefreshTokenUnusable,
    #[error(
        "ChatGPT authorization is unusable; account blocked, but local token removal was not confirmed; retry arany provider logout chatgpt"
    )]
    RefreshCleanupUncertain,
    #[error(
        "ChatGPT token renewal may have succeeded but could not be saved; sign in again before continuing"
    )]
    RenewalStorageUncertain,
    #[error("ChatGPT client registration was rejected")]
    InvalidClient,
    #[error("invalid authorization identity")]
    InvalidIdentity,
    #[error("invalid authorization identity: {0}; sign-in was not saved; use /setup to retry")]
    IdentityRejected(identity::IdentityFailure),
    #[error("selected ChatGPT account changed; restart /setup")]
    SelectedAccountChanged,
    #[error("ChatGPT registrations in different state roots conflict")]
    RegistrationConflict,
    #[error(
        "ChatGPT plan permission was not granted; use /setup to enable the plan or explicitly choose an API account"
    )]
    PermissionMissing,
    #[error("ChatGPT plan risk warning was not accepted")]
    ConsentRequired,
    #[error("invalid ChatGPT model catalog")]
    InvalidCatalog,
    #[error("ChatGPT model catalog returned HTTP {status}; {advice}")]
    CatalogHttp { status: u16, advice: &'static str },
    #[error("ChatGPT model or effort is invalid")]
    InvalidSelection,
    #[error("model is not visible to the selected ChatGPT account")]
    ModelUnavailable,
    #[error("ChatGPT synthetic conformance failed")]
    ConformanceFailed,
    #[error(
        "ChatGPT synthetic conformance returned HTTP {status}; {advice}; no model check was saved"
    )]
    ConformanceHttp { status: u16, advice: &'static str },
    #[error("ChatGPT synthetic conformance failed ({code}); {advice}; no model check was saved")]
    ConformanceStream { code: String, advice: &'static str },
    #[error("ChatGPT conformance evidence is unavailable")]
    EvidenceUnavailable,
}

pub(crate) struct AuthorizationUrl(Url);

impl AuthorizationUrl {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

pub(crate) struct AuthorizationAttempt {
    redirect_uri: String,
    host_id: Uuid,
    state: String,
    nonce: String,
    verifier: String,
    mode: AuthorizationMode,
}

enum AuthorizationMode {
    NewRegistration,
    RegistrationRetry {
        client_id: String,
    },
    Returning {
        client_id: String,
        subject: String,
        plan_permission_missing: bool,
    },
}

pub(crate) struct CodeExchange {
    code: String,
    client_id: String,
    redirect_uri: String,
    verifier: String,
    nonce: String,
    host_id: Uuid,
    expected_subject: Option<String>,
}

struct AcceptedCallback {
    code: String,
    client_id: String,
    expected_subject: Option<String>,
}

pub(crate) enum VerifiedSignIn {
    PlanEnabled(VerifiedCredentials),
    PlanDisabled(VerifiedIdentity),
}

pub(crate) struct VerifiedIdentity {
    pub(crate) client_id: String,
    pub(crate) host_id: Uuid,
    pub(crate) subject: String,
}

impl VerifiedIdentity {
    fn valid(&self) -> bool {
        valid_client_id(&self.client_id)
            && self.host_id.get_version() == Some(Version::Random)
            && self.host_id.get_variant() == Variant::RFC4122
            && bounded_graphic(&self.subject, 512)
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerifiedCredentials {
    pub(crate) client_id: String,
    pub(crate) host_id: Uuid,
    pub(crate) subject: String,
    pub(crate) id_token: String,
    pub(crate) access_token: String,
    pub(crate) refresh_token: String,
    pub(crate) access_expires_at_unix: u64,
}

impl VerifiedCredentials {
    fn valid_saved(&self) -> bool {
        valid_client_id(&self.client_id)
            && self.host_id.get_version() == Some(Version::Random)
            && self.host_id.get_variant() == Variant::RFC4122
            && bounded_graphic(&self.subject, 512)
            && bounded_graphic(&self.id_token, 16 * 1024)
            && bounded_graphic(&self.access_token, 16 * 1024)
            && bounded_graphic(&self.refresh_token, 8 * 1024)
            && self.access_expires_at_unix != 0
    }

    pub(crate) async fn refresh_uncommitted(&self) -> Result<Self, AuthorizationError> {
        exchange::refresh_uncommitted(self).await
    }

    pub(crate) async fn revoke_remote(&self) -> Result<(), AuthorizationError> {
        exchange::revoke_remote(self).await
    }
}

impl CodeExchange {
    pub(crate) fn issued_client_id(&self) -> &str {
        &self.client_id
    }

    pub(crate) async fn redeem(
        self,
        workspace: &std::path::Path,
    ) -> Result<VerifiedSignIn, AuthorizationError> {
        self.retain_new_registration(workspace)?;
        exchange::redeem(self).await
    }

    fn retain_new_registration(
        &self,
        workspace: &std::path::Path,
    ) -> Result<(), AuthorizationError> {
        if !self.is_new_registration() {
            return Ok(());
        }
        registration::retain_issued_client(workspace, self)
    }

    fn is_new_registration(&self) -> bool {
        self.expected_subject.is_none()
    }
}

impl AuthorizationAttempt {
    pub(crate) fn new(host_id: Uuid, callback_port: u16) -> Result<Self, AuthorizationError> {
        let mut random = [0u8; 96];
        getrandom::fill(&mut random).map_err(|_| AuthorizationError::Unavailable)?;
        Self::from_random(host_id, callback_port, random)
    }

    pub(crate) fn for_existing(
        account: &ReauthorizationTarget,
        callback_port: u16,
    ) -> Result<Self, AuthorizationError> {
        if !valid_client_id(&account.client_id)
            || !bounded_graphic(&account.subject, 512)
            || account.id.get_version() != Some(Version::SortRand)
        {
            return Err(AuthorizationError::InvalidIdentity);
        }
        let mut attempt = Self::new(account.host_id, callback_port)?;
        attempt.mode = AuthorizationMode::Returning {
            client_id: account.client_id.clone(),
            subject: account.subject.clone(),
            plan_permission_missing: account.plan_permission_missing,
        };
        Ok(attempt)
    }

    pub(crate) fn for_new_registration_retry(
        host_id: Uuid,
        client_id: String,
        callback_port: u16,
    ) -> Result<Self, AuthorizationError> {
        if !valid_client_id(&client_id) {
            return Err(AuthorizationError::InvalidIdentity);
        }
        let mut attempt = Self::new(host_id, callback_port)?;
        attempt.mode = AuthorizationMode::RegistrationRetry { client_id };
        Ok(attempt)
    }

    fn from_random(
        host_id: Uuid,
        callback_port: u16,
        random: [u8; 96],
    ) -> Result<Self, AuthorizationError> {
        if host_id.get_version() != Some(Version::Random)
            || host_id.get_variant() != Variant::RFC4122
            || callback_port == 0
        {
            return Err(AuthorizationError::Unavailable);
        }
        Ok(Self {
            redirect_uri: format!("http://127.0.0.1:{callback_port}{CALLBACK_PATH}"),
            host_id,
            state: URL_SAFE_NO_PAD.encode(&random[..32]),
            nonce: URL_SAFE_NO_PAD.encode(&random[32..64]),
            verifier: URL_SAFE_NO_PAD.encode(&random[64..]),
            mode: AuthorizationMode::NewRegistration,
        })
    }

    pub(crate) fn authorization_url(&self) -> AuthorizationUrl {
        let mut url = Url::parse(AUTHORIZE_ENDPOINT).expect("compiled authorization endpoint");
        let mut query = url.query_pairs_mut();
        match &self.mode {
            AuthorizationMode::NewRegistration => {
                query
                    .append_pair("client_id", "dynamic_agent_client")
                    .append_pair("agent_name_hint", "Arany");
            }
            AuthorizationMode::RegistrationRetry { client_id } => {
                query.append_pair("client_id", client_id);
            }
            AuthorizationMode::Returning {
                client_id,
                plan_permission_missing,
                ..
            } => {
                query.append_pair("client_id", client_id);
                if *plan_permission_missing {
                    query.append_pair("prompt", "consent");
                }
            }
        }
        query
            .append_pair("ext_agent_host_id", &format!("urn:uuid:{}", self.host_id))
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", SCOPE)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &self.state)
            .append_pair("nonce", &self.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &pkce_challenge(&self.verifier));
        drop(query);
        AuthorizationUrl(url)
    }

    pub(crate) fn accept_callback(
        self,
        callback_url: &str,
    ) -> Result<CodeExchange, AuthorizationError> {
        let accepted = self.inspect_callback(callback_url)?;
        Ok(self.into_code_exchange(accepted))
    }

    fn inspect_callback(&self, callback_url: &str) -> Result<AcceptedCallback, AuthorizationError> {
        let expected_prefix = format!("{}?", self.redirect_uri);
        if callback_url.len() > MAX_CALLBACK_BYTES
            || !callback_url.starts_with(&expected_prefix)
            || callback_url.contains('#')
            || !valid_query_bytes(&callback_url[expected_prefix.len()..])
        {
            return Err(AuthorizationError::InvalidCallback);
        }
        let url = Url::parse(callback_url).map_err(|_| AuthorizationError::InvalidCallback)?;
        let mut code = None;
        let mut state = None;
        let mut client_id = None;
        let mut error = None;
        let mut scope_seen = false;
        let mut count = 0;
        for (name, value) in url.query_pairs() {
            count += 1;
            if count > 6 {
                return Err(AuthorizationError::InvalidCallback);
            }
            match name.as_ref() {
                "code" if code.is_none() && bounded_graphic(&value, MAX_CODE_BYTES) => {
                    code = Some(value.into_owned());
                }
                "state" if state.is_none() && bounded_graphic(&value, 64) => {
                    state = Some(value.into_owned());
                }
                "client_id" if client_id.is_none() && valid_client_id(&value) => {
                    client_id = Some(value.into_owned());
                }
                "scope"
                    if !scope_seen
                        && value.len() <= MAX_SCOPE_BYTES
                        && value
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() || byte == b' ') =>
                {
                    scope_seen = true;
                }
                "error" if error.is_none() && bounded_graphic(&value, 128) => {
                    error = Some(value.into_owned());
                }
                _ => return Err(AuthorizationError::InvalidCallback),
            }
        }
        if state.as_deref() != Some(&self.state) {
            return Err(AuthorizationError::InvalidCallback);
        }
        if let Some(error) = error {
            if code.is_some() || client_id.is_some() {
                return Err(AuthorizationError::InvalidCallback);
            }
            return if error == "access_denied" {
                Err(AuthorizationError::Denied)
            } else {
                Err(AuthorizationError::InvalidCallback)
            };
        }
        let code = code.ok_or(AuthorizationError::InvalidCallback)?;
        let (client_id, expected_subject) = match &self.mode {
            AuthorizationMode::NewRegistration => {
                (client_id.ok_or(AuthorizationError::InvalidCallback)?, None)
            }
            AuthorizationMode::RegistrationRetry {
                client_id: selected,
            } => {
                if client_id.is_some_and(|returned| returned != *selected) {
                    return Err(AuthorizationError::InvalidCallback);
                }
                (selected.clone(), None)
            }
            AuthorizationMode::Returning {
                client_id: selected,
                subject,
                ..
            } => {
                if client_id.is_some_and(|returned| returned != *selected) {
                    return Err(AuthorizationError::InvalidCallback);
                }
                (selected.clone(), Some(subject.clone()))
            }
        };
        Ok(AcceptedCallback {
            code,
            client_id,
            expected_subject,
        })
    }

    fn into_code_exchange(self, accepted: AcceptedCallback) -> CodeExchange {
        CodeExchange {
            code: accepted.code,
            client_id: accepted.client_id,
            redirect_uri: self.redirect_uri,
            verifier: self.verifier,
            nonce: self.nonce,
            host_id: self.host_id,
            expected_subject: accepted.expected_subject,
        }
    }
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn bounded_graphic(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn valid_client_id(value: &str) -> bool {
    value != "dynamic_agent_client"
        && !value.is_empty()
        && value.len() <= MAX_CLIENT_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_query_bytes(query: &str) -> bool {
    let bytes = query.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            if !bytes[index].is_ascii_graphic() {
                return false;
            }
            index += 1;
        }
    }
    true
}

#[cfg(test)]
fn fixture_host_id() -> Uuid {
    Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").expect("UUIDv4 host ID")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::credentials::AccountStorage;

    #[test]
    fn conformance_http_notice_has_fixed_admission_recovery() {
        for (status, advice) in [
            (
                401,
                "verify the selected account and granted direct-use permission",
            ),
            (403, "check account, region, and integration policy"),
            (429, "pause checks and retry later"),
            (500, "keep the saved account and retry later"),
            (
                503,
                "direct routing may be unavailable; keep the saved account and retry later",
            ),
            (418, "review the direct-route request and selected account"),
        ] {
            assert_eq!(
                AuthorizationError::ConformanceHttp {
                    status,
                    advice: admission_http_advice(status),
                }
                .to_string(),
                format!(
                    "ChatGPT synthetic conformance returned HTTP {status}; {advice}; no model check was saved"
                )
            );
        }
    }

    #[test]
    fn stream_failure_notice_preserves_code_with_specific_recovery() {
        let code = "subscription_sharing_usage_limit_exceeded";
        let advice = stream_failure_advice(code);
        assert_eq!(
            AuthorizationError::ConformanceStream {
                code: code.into(),
                advice,
            }
            .to_string(),
            "ChatGPT synthetic conformance failed (subscription_sharing_usage_limit_exceeded); pause ChatGPT plan checks and review ChatGPT Settings > Usage; no model check was saved"
        );
        assert_eq!(
            stream_failure_advice("subscription_sharing_usage_unavailable"),
            "keep the saved account and try again later"
        );
    }

    fn fixture_attempt() -> AuthorizationAttempt {
        let mut random = [0; 96];
        random[..32].fill(7);
        random[32..64].fill(8);
        random[64..].fill(9);
        AuthorizationAttempt::from_random(fixture_host_id(), 1455, random).expect("attempt")
    }

    fn callback(attempt: &AuthorizationAttempt, suffix: &str) -> String {
        format!("{}?state={}&{suffix}", attempt.redirect_uri, attempt.state)
    }

    fn saved_account() -> VerifiedCredentials {
        VerifiedCredentials {
            client_id: "oaiapp_saved".into(),
            host_id: fixture_host_id(),
            subject: "verified-subject".into(),
            id_token: "synthetic-signed-id-hint".into(),
            access_token: "synthetic-access".into(),
            refresh_token: "synthetic-refresh".into(),
            access_expires_at_unix: 1_800_000_000,
        }
    }

    fn reauthorization_target() -> ReauthorizationTarget {
        let account = saved_account();
        ReauthorizationTarget {
            id: Uuid::now_v7(),
            host_id: account.host_id,
            client_id: account.client_id,
            subject: account.subject,
            storage: AccountStorage::Keyring,
            plan_permission_missing: false,
        }
    }

    #[test]
    fn new_registration_url_pins_official_parameters_and_pkce() {
        let attempt = fixture_attempt();
        let url = attempt.authorization_url();
        let parsed = Url::parse(url.as_str()).expect("authorization URL");
        assert_eq!(
            parsed.origin().ascii_serialization(),
            "https://auth.openai.com"
        );
        assert_eq!(parsed.path(), "/api/accounts/authorize");
        let fields = parsed
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], "dynamic_agent_client");
        assert_eq!(fields["agent_name_hint"], "Arany");
        assert_eq!(
            fields["ext_agent_host_id"],
            format!("urn:uuid:{}", attempt.host_id)
        );
        assert_eq!(fields["response_type"], "code");
        assert_eq!(fields["redirect_uri"], attempt.redirect_uri);
        assert_eq!(fields["resource"], RESOURCE);
        assert_eq!(fields["scope"], SCOPE);
        assert_eq!(fields["state"], attempt.state);
        assert_eq!(fields["nonce"], attempt.nonce);
        assert_ne!(attempt.state, attempt.nonce);
        assert_ne!(attempt.state, attempt.verifier);
        assert_eq!(fields["code_challenge_method"], "S256");
        assert_eq!(fields["code_challenge"], pkce_challenge(&attempt.verifier));
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn uuid_host_id_requires_rfc_uuid_v4() {
        let wrong_variant =
            Uuid::parse_str("123e4567-e89b-42d3-7456-426614174000").expect("test UUID");
        let wrong_version =
            Uuid::parse_str("123e4567-e89b-72d3-a456-426614174000").expect("test UUIDv7");
        for invalid in [Uuid::nil(), wrong_version, wrong_variant] {
            assert!(
                AuthorizationAttempt::from_random(invalid, 1455, [0; 96]).is_err(),
                "accepted host ID {invalid}"
            );
        }
        assert!(AuthorizationAttempt::from_random(fixture_host_id(), 1455, [0; 96]).is_ok());
    }

    #[test]
    fn callback_consumes_only_the_matching_registration() {
        let attempt = fixture_attempt();
        let verifier = attempt.verifier.clone();
        let nonce = attempt.nonce.clone();
        let host_id = attempt.host_id;
        let callback = callback(
            &attempt,
            "code=synthetic-code&client_id=oaiapp_test&scope=openid+chatgpt.tokens.use.direct",
        );
        let exchange = attempt.accept_callback(&callback).expect("accepted code");
        assert_eq!(exchange.code, "synthetic-code");
        assert_eq!(exchange.client_id, "oaiapp_test");
        assert_eq!(exchange.redirect_uri, "http://127.0.0.1:1455/auth/callback");
        assert_eq!(exchange.verifier, verifier);
        assert_eq!(exchange.nonce, nonce);
        assert_eq!(exchange.host_id, host_id);
        assert_eq!(exchange.expected_subject, None);
        assert!(exchange.is_new_registration());
    }

    #[test]
    fn returning_account_pins_client_host_and_subject_without_registering_again() {
        let account = reauthorization_target();
        let attempt = AuthorizationAttempt::for_existing(&account, 1455).expect("selected account");
        let url = Url::parse(attempt.authorization_url().as_str()).expect("authorization URL");
        let fields = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], account.client_id);
        assert!(!fields.contains_key("id_token_hint"));
        assert_eq!(
            fields["ext_agent_host_id"],
            format!("urn:uuid:{}", account.host_id)
        );
        assert!(!fields.contains_key("agent_name_hint"));
        assert_eq!(fields["scope"], SCOPE);
        assert_eq!(fields["resource"], RESOURCE);
        assert!(!fields.contains_key("prompt"));
        assert!(!fields.contains_key("force_reconsent"));
        let mut disabled = reauthorization_target();
        disabled.plan_permission_missing = true;
        let repair =
            AuthorizationAttempt::for_existing(&disabled, 1455).expect("permission repair");
        let repair_url = Url::parse(repair.authorization_url().as_str()).unwrap();
        let repair_fields = repair_url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(repair_fields["prompt"], "consent");
        assert_eq!(repair_fields["client_id"], disabled.client_id);
        assert_eq!(repair_fields["scope"], SCOPE);
        assert_eq!(
            repair_fields["ext_agent_host_id"],
            format!("urn:uuid:{}", disabled.host_id)
        );
        assert!(!repair_fields.contains_key("force_reconsent"));
        assert!(!repair_fields.contains_key("id_token_hint"));
        assert!(!repair_fields.contains_key("agent_name_hint"));
        for suffix in [
            "code=renewed-code",
            "code=renewed-code&client_id=oaiapp_saved",
        ] {
            let attempt =
                AuthorizationAttempt::for_existing(&account, 1455).expect("selected account");
            let callback = callback(&attempt, suffix);
            let exchange = attempt
                .accept_callback(&callback)
                .expect("issued client retained");
            assert_eq!(exchange.code, "renewed-code");
            assert_eq!(exchange.client_id, account.client_id);
            assert_eq!(
                exchange.expected_subject.as_deref(),
                Some("verified-subject")
            );
            assert_eq!(exchange.host_id, account.host_id);
            assert!(!exchange.is_new_registration());
        }
    }

    #[test]
    fn issued_registration_retry_pins_client_host_and_callback_without_signed_account() {
        let first = fixture_attempt();
        let accepted = callback(&first, "code=spent-code&client_id=oaiapp_issued");
        let exchange = first.accept_callback(&accepted).expect("issued client");
        let host_id = exchange.host_id;
        let client_id = exchange.issued_client_id().to_owned();
        assert_eq!(client_id, "oaiapp_issued");
        let retry = AuthorizationAttempt::for_new_registration_retry(host_id, client_id, 1456)
            .expect("fresh attempt with issued client");
        let fields = Url::parse(retry.authorization_url().as_str())
            .expect("authorization URL")
            .query_pairs()
            .into_owned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(fields["client_id"], "oaiapp_issued");
        assert_eq!(fields["ext_agent_host_id"], format!("urn:uuid:{host_id}"));
        assert_eq!(
            fields["redirect_uri"],
            "http://127.0.0.1:1456/auth/callback"
        );
        assert!(!fields.contains_key("agent_name_hint"));
        assert!(!fields.contains_key("id_token_hint"));

        let mismatched = callback(&retry, "code=new-code&client_id=oaiapp_other");
        assert_eq!(
            retry.inspect_callback(&mismatched).err(),
            Some(AuthorizationError::InvalidCallback)
        );

        let returned = callback(&retry, "code=new-code");
        let retried = retry
            .accept_callback(&returned)
            .expect("same issued client");
        assert_eq!(retried.client_id, "oaiapp_issued");
        assert_eq!(retried.expected_subject, None);
        assert_eq!(retried.host_id, host_id);

        assert!(
            AuthorizationAttempt::for_new_registration_retry(
                host_id,
                "dynamic_agent_client".into(),
                1456
            )
            .is_err()
        );
    }

    #[test]
    fn returning_callback_rejects_registration_drift_and_invalid_saved_identity() {
        let account = reauthorization_target();
        for suffix in [
            "code=renewed-code&client_id=oaiapp_other",
            "code=renewed-code&client_id=dynamic_agent_client",
            "code=renewed-code&client_id=oaiapp_saved&client_id=oaiapp_saved",
        ] {
            let attempt =
                AuthorizationAttempt::for_existing(&account, 1455).expect("selected account");
            let callback = callback(&attempt, suffix);
            assert_eq!(
                attempt.accept_callback(&callback).err(),
                Some(AuthorizationError::InvalidCallback),
                "{suffix}"
            );
        }
        let attempt = AuthorizationAttempt::for_existing(&account, 1455).expect("selected account");
        let denial = callback(&attempt, "error=access_denied");
        assert_eq!(
            attempt.accept_callback(&denial).err(),
            Some(AuthorizationError::Denied)
        );
        for field in ["client", "subject", "account id", "host", "host version"] {
            let mut invalid = reauthorization_target();
            match field {
                "client" => invalid.client_id = "dynamic_agent_client".into(),
                "subject" => invalid.subject = "bad\nsubject".into(),
                "account id" => invalid.id = Uuid::nil(),
                "host" => invalid.host_id = Uuid::nil(),
                "host version" => {
                    invalid.host_id = Uuid::parse_str("123e4567-e89b-72d3-a456-426614174000")
                        .expect("test UUIDv7")
                }
                _ => unreachable!(),
            }
            assert!(
                AuthorizationAttempt::for_existing(&invalid, 1455).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn callback_rejects_cross_origin_duplicates_and_oversized_values() {
        let valid = callback(
            &fixture_attempt(),
            "code=synthetic-code&client_id=oaiapp_test",
        );
        let bad = [
            valid.replace("127.0.0.1", "localhost"),
            valid.replace(":1455", ":1456"),
            valid.replace("/auth/callback", "/callback"),
            valid.replace("state=", "state=wrong"),
            format!("{valid}&state=duplicate"),
            format!("{valid}&code=duplicate"),
            format!("{valid}&client_id=other"),
            format!("{valid}&extra=value"),
            format!("{valid}#fragment"),
            valid.replace("oaiapp_test", "dynamic_agent_client"),
            valid.replace("code=synthetic-code", "code=%G0"),
            valid.replace("code=synthetic-code", "code=synthetic-code%0A"),
            valid.replace("code=synthetic-code", &format!("code={}", "x".repeat(2049))),
            format!("{valid}&scope={}", "x".repeat(MAX_CALLBACK_BYTES)),
        ];
        for value in bad {
            assert!(matches!(
                fixture_attempt().accept_callback(&value),
                Err(AuthorizationError::InvalidCallback)
            ));
        }
        assert!(matches!(
            fixture_attempt().accept_callback("http://127.0.0.1:1455/auth/callback?code=x"),
            Err(AuthorizationError::InvalidCallback)
        ));
        let missing_client = valid.replace("&client_id=oaiapp_test", "");
        assert!(matches!(
            fixture_attempt().accept_callback(&missing_client),
            Err(AuthorizationError::InvalidCallback)
        ));
        assert!(AuthorizationAttempt::from_random(fixture_host_id(), 0, [0; 96]).is_err());
    }

    #[test]
    fn denial_is_state_bound_and_redacted() {
        let attempt = fixture_attempt();
        let denied = callback(&attempt, "error=access_denied");
        assert!(matches!(
            attempt.accept_callback(&denied),
            Err(AuthorizationError::Denied)
        ));
        assert_eq!(
            AuthorizationError::Denied.to_string(),
            "authorization was denied"
        );
        let attempt = fixture_attempt();
        let ambiguous = callback(&attempt, "error=access_denied&code=x");
        assert!(matches!(
            attempt.accept_callback(&ambiguous),
            Err(AuthorizationError::InvalidCallback)
        ));
    }
}
