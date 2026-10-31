use super::{
    AuthorizationError, DEADLINE, Endpoints, MAX_JWKS_BYTES, MAX_TOKEN_RESPONSE_BYTES,
    VerifiedCredentials, access_expiry_at, bounded_token, client, form_body, read_json, unix_now,
    validate_token_fields,
};
use crate::accounts::chatgpt::{
    CLIENT_ID, MAX_ACCESS_TOKEN_BYTES, MAX_ID_TOKEN_BYTES, MAX_REFRESH_TOKEN_BYTES,
    MAX_SUBJECT_BYTES, bounded_graphic, identity, valid_client_id,
};
use reqwest::{Client, Response, StatusCode, Url, header};
use serde::Deserialize;

pub(super) const MAX_REFRESH_FORM_BYTES: usize = 25 * 1024;

#[derive(Deserialize)]
struct RefreshReply {
    #[serde(default, deserialize_with = "identity::present_string")]
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
    token_type: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct RefreshErrorReply {
    error: RefreshErrorCode,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RefreshErrorCode {
    Plain(String),
    Nested { code: String },
}

impl RefreshErrorCode {
    fn as_str(&self) -> &str {
        match self {
            Self::Plain(code) | Self::Nested { code } => code,
        }
    }
}

pub(super) async fn refresh(
    credentials: &VerifiedCredentials,
) -> Result<VerifiedCredentials, AuthorizationError> {
    if credentials.client_id != CLIENT_ID {
        return Err(AuthorizationError::InvalidClient);
    }
    tokio::time::timeout(DEADLINE, refresh_at(credentials, Endpoints::production()))
        .await
        .map_err(|_| AuthorizationError::RefreshOutcomeUnknown)?
}

pub(super) async fn refresh_at(
    credentials: &VerifiedCredentials,
    endpoints: Endpoints,
) -> Result<VerifiedCredentials, AuthorizationError> {
    if !valid_client_id(&credentials.client_id)
        || credentials.host_id.is_nil()
        || !bounded_graphic(&credentials.subject, MAX_SUBJECT_BYTES)
        || !bounded_token(&credentials.id_token, MAX_ID_TOKEN_BYTES)
        || !bounded_token(&credentials.access_token, MAX_ACCESS_TOKEN_BYTES)
        || !bounded_token(&credentials.refresh_token, MAX_REFRESH_TOKEN_BYTES)
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let body = form_body(
        &[
            ("grant_type", "refresh_token"),
            ("client_id", credentials.client_id.as_str()),
            ("refresh_token", credentials.refresh_token.as_str()),
        ],
        MAX_REFRESH_FORM_BYTES,
    )?;
    let client = client(endpoints.https_only)?;
    let requested_at_unix = unix_now()?;
    let response = client
        .post(endpoints.token.clone())
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|error| {
            if error.is_connect() || error.is_builder() {
                AuthorizationError::Unavailable
            } else {
                AuthorizationError::RefreshOutcomeUnknown
            }
        })?;
    if response.url() != &endpoints.token {
        return Err(AuthorizationError::ExchangeFailed);
    }
    if response.status() != StatusCode::OK {
        return Err(classify_refresh_error(response).await);
    }
    let bytes = read_json(
        response,
        StatusCode::OK,
        MAX_TOKEN_RESPONSE_BYTES,
        false,
        || AuthorizationError::ExchangeFailed,
    )
    .await
    .map_err(after_exchange_error)?;
    let replacement: RefreshReply =
        serde_json::from_slice(&bytes).map_err(|_| AuthorizationError::ExchangeFailed)?;
    let access_expires_at_unix =
        validate_replacement(&replacement, credentials, requested_at_unix)?;
    if let Some(id_token) = &replacement.id_token {
        verify_replacement_identity(&client, endpoints.jwks, id_token, credentials).await?;
    }
    Ok(VerifiedCredentials {
        client_id: credentials.client_id.clone(),
        host_id: credentials.host_id,
        subject: credentials.subject.clone(),
        chatgpt_account_id: credentials.chatgpt_account_id.clone(),
        id_token: credentials.id_token.clone(),
        access_token: replacement.access_token,
        refresh_token: replacement.refresh_token,
        access_expires_at_unix,
    })
}

async fn classify_refresh_error(response: Response) -> AuthorizationError {
    let status = response.status();
    if status.is_server_error() {
        return AuthorizationError::RefreshOutcomeUnknown;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return AuthorizationError::Unavailable;
    }
    if !status.is_client_error() {
        return AuthorizationError::ExchangeFailed;
    }
    let reply = read_json(response, status, MAX_TOKEN_RESPONSE_BYTES, false, || {
        AuthorizationError::ExchangeFailed
    })
    .await
    .ok()
    .and_then(|bytes| serde_json::from_slice::<RefreshErrorReply>(&bytes).ok());
    match reply.as_ref().map(|reply| reply.error.as_str()) {
        Some("invalid_client") => AuthorizationError::InvalidClient,
        Some("refresh_token_expired" | "refresh_token_invalidated" | "refresh_token_reused") => {
            AuthorizationError::RefreshTokenUnusable
        }
        Some("invalid_grant") if status == StatusCode::BAD_REQUEST => {
            AuthorizationError::RefreshTokenUnusable
        }
        _ if status == StatusCode::UNAUTHORIZED => AuthorizationError::RefreshTokenUnusable,
        _ => AuthorizationError::ExchangeFailed,
    }
}

fn validate_replacement(
    replacement: &RefreshReply,
    current: &VerifiedCredentials,
    requested_at_unix: u64,
) -> Result<u64, AuthorizationError> {
    validate_token_fields(
        &replacement.access_token,
        Some(&replacement.refresh_token),
        replacement.token_type.as_deref(),
        replacement.scope.as_deref(),
    )?;
    if replacement
        .id_token
        .as_deref()
        .is_some_and(|token| !bounded_token(token, MAX_ID_TOKEN_BYTES))
        || replacement.access_token == current.access_token
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    access_expiry_at(requested_at_unix, replacement.expires_in)
}

async fn verify_replacement_identity(
    client: &Client,
    endpoint: Url,
    id_token: &str,
    credentials: &VerifiedCredentials,
) -> Result<(), AuthorizationError> {
    let response = client
        .get(endpoint)
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| AuthorizationError::RefreshOutcomeUnknown)?;
    let keys = read_json(response, StatusCode::OK, MAX_JWKS_BYTES, true, || {
        AuthorizationError::IdentityRejected(identity::IdentityFailure::KeyTransport)
    })
    .await
    .map_err(after_exchange_error)?;
    identity::verify_renewed(id_token, &keys, credentials, unix_now()?).map(|_| ())
}

fn after_exchange_error(error: AuthorizationError) -> AuthorizationError {
    match error {
        AuthorizationError::Unavailable => AuthorizationError::RefreshOutcomeUnknown,
        error => error,
    }
}

#[cfg(test)]
mod tests;
