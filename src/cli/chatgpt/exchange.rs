use super::{
    AuthorizationError, CodeExchange, VerifiedCredentials, VerifiedIdentity, VerifiedSignIn,
    identity,
};
use reqwest::{Client, Response, StatusCode, Url, header, redirect::Policy};
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod revoke;

pub(super) async fn revoke_remote(
    credentials: &VerifiedCredentials,
) -> Result<(), AuthorizationError> {
    revoke::revoke_remote(credentials).await
}

const TOKEN_ENDPOINT: &str = "https://auth.openai.com/api/accounts/oauth/token";
const JWKS_ENDPOINT: &str = "https://auth.openai.com/.well-known/jwks.json";
const MAX_FORM_BYTES: usize = 4096;
const MAX_REFRESH_FORM_BYTES: usize = 25 * 1024;
const MAX_TOKEN_RESPONSE_BYTES: usize = 32 * 1024;
const MAX_JWKS_BYTES: usize = 64 * 1024;
const DEADLINE: Duration = Duration::from_secs(30);

struct Endpoints {
    token: Url,
    jwks: Url,
    https_only: bool,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            token: Url::parse(TOKEN_ENDPOINT).expect("compiled token endpoint"),
            jwks: Url::parse(JWKS_ENDPOINT).expect("compiled JWKS endpoint"),
            https_only: true,
        }
    }
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
    id_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
}

#[derive(Deserialize)]
struct CodeErrorReply {
    error: String,
}

#[derive(Deserialize)]
struct RefreshReply {
    access_token: String,
    refresh_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
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

pub(super) async fn redeem(exchange: CodeExchange) -> Result<VerifiedSignIn, AuthorizationError> {
    tokio::time::timeout(DEADLINE, redeem_at(exchange, Endpoints::production()))
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
}

async fn redeem_at(
    exchange: CodeExchange,
    endpoints: Endpoints,
) -> Result<VerifiedSignIn, AuthorizationError> {
    let client = client(endpoints.https_only)?;
    let fields = [
        ("grant_type", "authorization_code"),
        ("client_id", exchange.client_id.as_str()),
        ("code", exchange.code.as_str()),
        ("code_verifier", exchange.verifier.as_str()),
        ("redirect_uri", exchange.redirect_uri.as_str()),
        ("resource", super::RESOURCE),
    ];
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in fields {
        serializer.append_pair(name, value);
    }
    let body = serializer.finish();
    if body.len() > MAX_FORM_BYTES {
        return Err(AuthorizationError::ExchangeFailed);
    }
    let requested_at_unix = unix_now()?;
    let token_response = client
        .post(endpoints.token)
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?;
    if token_response.status() != StatusCode::OK {
        return Err(classify_code_error(token_response).await);
    }
    let token_bytes = read_json(
        token_response,
        StatusCode::OK,
        MAX_TOKEN_RESPONSE_BYTES,
        false,
        || AuthorizationError::ExchangeFailed,
    )
    .await?;
    let token: TokenReply =
        serde_json::from_slice(&token_bytes).map_err(|_| AuthorizationError::ExchangeFailed)?;
    let plan_enabled = validate_reply(&token)?;

    let jwks_response = client
        .get(endpoints.jwks)
        .header(header::ACCEPT, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?;
    let jwks_bytes = read_json(jwks_response, StatusCode::OK, MAX_JWKS_BYTES, true, || {
        AuthorizationError::IdentityRejected(identity::IdentityFailure::KeyTransport)
    })
    .await?;
    let now_seconds = unix_now()?;
    let subject = identity::verify(
        &token.id_token,
        &jwks_bytes,
        &exchange.client_id,
        &exchange.nonce,
        now_seconds,
    )?;
    if exchange
        .expected_subject
        .as_deref()
        .is_some_and(|expected| expected != subject)
    {
        return Err(AuthorizationError::IdentityRejected(
            identity::IdentityFailure::SelectedSubject,
        ));
    }
    if !plan_enabled {
        return Ok(VerifiedSignIn::PlanDisabled(VerifiedIdentity {
            client_id: exchange.client_id,
            host_id: exchange.host_id,
            subject,
        }));
    }
    Ok(VerifiedSignIn::PlanEnabled(VerifiedCredentials {
        client_id: exchange.client_id,
        host_id: exchange.host_id,
        subject,
        id_token: token.id_token,
        access_token: token.access_token,
        refresh_token: token
            .refresh_token
            .expect("enabled grant has a refresh token"),
        access_expires_at_unix: access_expiry_at(requested_at_unix, token.expires_in)?,
    }))
}

async fn classify_code_error(response: Response) -> AuthorizationError {
    let status = response.status();
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        return AuthorizationError::Unavailable;
    }
    if !status.is_client_error() {
        return AuthorizationError::ExchangeFailed;
    }
    let bytes = match read_json(response, status, MAX_TOKEN_RESPONSE_BYTES, false, || {
        AuthorizationError::ExchangeFailed
    })
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    match serde_json::from_slice::<CodeErrorReply>(&bytes) {
        Ok(reply) if reply.error == "invalid_grant" => AuthorizationError::GrantUnusable,
        _ => AuthorizationError::ExchangeFailed,
    }
}

fn client(https_only: bool) -> Result<Client, AuthorizationError> {
    Client::builder()
        .https_only(https_only)
        .no_proxy()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(Policy::none())
        .referer(false)
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| AuthorizationError::Unavailable)
}

pub(super) async fn refresh_uncommitted(
    credentials: &VerifiedCredentials,
) -> Result<VerifiedCredentials, AuthorizationError> {
    tokio::time::timeout(DEADLINE, refresh_at(credentials, Endpoints::production()))
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
}

async fn refresh_at(
    credentials: &VerifiedCredentials,
    endpoints: Endpoints,
) -> Result<VerifiedCredentials, AuthorizationError> {
    if !super::valid_client_id(&credentials.client_id)
        || credentials.host_id.is_nil()
        || !super::bounded_graphic(&credentials.subject, 512)
        || !bounded_token(&credentials.id_token, 16 * 1024)
        || !bounded_token(&credentials.access_token, 16 * 1024)
        || !bounded_token(&credentials.refresh_token, 8 * 1024)
    {
        return Err(AuthorizationError::InvalidIdentity);
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in [
        ("grant_type", "refresh_token"),
        ("client_id", credentials.client_id.as_str()),
        ("refresh_token", credentials.refresh_token.as_str()),
        ("resource", super::RESOURCE),
    ] {
        serializer.append_pair(name, value);
    }
    let body = serializer.finish();
    if body.len() > MAX_REFRESH_FORM_BYTES {
        return Err(AuthorizationError::ExchangeFailed);
    }
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
        .map_err(|_| AuthorizationError::Unavailable)?;
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
    .await?;
    let replacement: RefreshReply =
        serde_json::from_slice(&bytes).map_err(|_| AuthorizationError::ExchangeFailed)?;
    if !validate_token_fields(
        &replacement.access_token,
        Some(&replacement.refresh_token),
        &replacement.token_type,
        replacement.expires_in,
        &replacement.scope,
    )? {
        return Err(AuthorizationError::PermissionMissing);
    }
    if replacement.access_token == credentials.access_token
        || replacement.refresh_token == credentials.refresh_token
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    Ok(VerifiedCredentials {
        client_id: credentials.client_id.clone(),
        host_id: credentials.host_id,
        subject: credentials.subject.clone(),
        id_token: credentials.id_token.clone(),
        access_token: replacement.access_token,
        refresh_token: replacement.refresh_token,
        access_expires_at_unix: access_expiry_at(requested_at_unix, replacement.expires_in)?,
    })
}

async fn classify_refresh_error(response: Response) -> AuthorizationError {
    let status = response.status();
    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
        return AuthorizationError::Unavailable;
    }
    if !status.is_client_error() {
        return AuthorizationError::ExchangeFailed;
    }
    let bytes = match read_json(response, status, MAX_TOKEN_RESPONSE_BYTES, false, || {
        AuthorizationError::ExchangeFailed
    })
    .await
    {
        Ok(bytes) => bytes,
        Err(error) => return error,
    };
    let Ok(reply) = serde_json::from_slice::<RefreshErrorReply>(&bytes) else {
        return AuthorizationError::ExchangeFailed;
    };
    match reply.error.as_str() {
        "invalid_grant"
        | "invalid_refresh_token"
        | "token_expired"
        | "refresh_token_expired"
        | "refresh_token_invalidated"
        | "refresh_token_reused" => AuthorizationError::RefreshTokenUnusable,
        "invalid_client" => AuthorizationError::InvalidClient,
        _ => AuthorizationError::ExchangeFailed,
    }
}

fn validate_reply(token: &TokenReply) -> Result<bool, AuthorizationError> {
    let plan_enabled = validate_token_fields(
        &token.access_token,
        token.refresh_token.as_deref(),
        &token.token_type,
        token.expires_in,
        &token.scope,
    )?;
    if !bounded_token(&token.id_token, 16 * 1024) {
        return Err(AuthorizationError::ExchangeFailed);
    }
    Ok(plan_enabled)
}

fn validate_token_fields(
    access_token: &str,
    refresh_token: Option<&str>,
    token_type: &str,
    expires_in: u64,
    scope: &str,
) -> Result<bool, AuthorizationError> {
    if token_type != "Bearer"
        || !(1..=7200).contains(&expires_in)
        || !bounded_token(access_token, 16 * 1024)
        || refresh_token.is_some_and(|token| !bounded_token(token, 8 * 1024))
        || scope.len() > 512
        || !scope
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    let scopes: Vec<&str> = scope.split(' ').filter(|scope| !scope.is_empty()).collect();
    if scopes.len() > 16
        || scopes.iter().any(|scope| scope.len() > 64)
        || scopes
            .iter()
            .enumerate()
            .any(|(index, scope)| scopes[..index].contains(scope))
    {
        return Err(AuthorizationError::ExchangeFailed);
    }
    let plan_enabled = scopes.contains(&"chatgpt.tokens.use.direct")
        && scopes.contains(&"resource.invoke")
        && scopes.contains(&"offline_access");
    if plan_enabled && refresh_token.is_none() {
        return Err(AuthorizationError::ExchangeFailed);
    }
    Ok(plan_enabled)
}

fn bounded_token(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn unix_now() -> Result<u64, AuthorizationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AuthorizationError::InvalidIdentity)
}

fn access_expiry_at(requested_at_unix: u64, expires_in: u64) -> Result<u64, AuthorizationError> {
    if !(1..=7200).contains(&expires_in) {
        return Err(AuthorizationError::ExchangeFailed);
    }
    requested_at_unix
        .checked_add(expires_in)
        .ok_or(AuthorizationError::ExchangeFailed)
}

async fn read_json(
    mut response: Response,
    expected_status: StatusCode,
    max: usize,
    jwks: bool,
    invalid: fn() -> AuthorizationError,
) -> Result<Vec<u8>, AuthorizationError> {
    if response.status() != expected_status
        || response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .any(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity"))
        || response
            .content_length()
            .is_some_and(|length| length > max as u64)
    {
        return Err(invalid());
    }
    let mut types = response.headers().get_all(header::CONTENT_TYPE).iter();
    let content_type = types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if types.next().is_some()
        || !content_type.is_some_and(|kind| {
            kind.eq_ignore_ascii_case("application/json")
                || (jwks && kind.eq_ignore_ascii_case("application/jwk-set+json"))
        })
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| AuthorizationError::Unavailable)?
    {
        if chunk.len() > max - bytes.len() {
            return Err(invalid());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
