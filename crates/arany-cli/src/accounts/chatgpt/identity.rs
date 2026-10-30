use super::AuthorizationError;
use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer};

pub(super) const ISSUER: &str = "https://auth.openai.com";
const MAX_KEYS: usize = 16;
const CLOCK_SKEW_SECONDS: u64 = 5;
const MAX_HEADER_BYTES: usize = 1024;
const MAX_KEY_ID_BYTES: usize = 128;
const MAX_CLAIMS_BYTES: usize = 8 * 1024;
// RS256 keys of 2048 to 8192 bits; a signature is as long as its modulus.
const MIN_MODULUS_BYTES: usize = 2048 / 8;
const MAX_MODULUS_BYTES: usize = 8192 / 8;
const MAX_EXPONENT_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum IdentityFailure {
    #[error("malformed ID token")]
    TokenFormat,
    #[error("malformed ID-token header")]
    HeaderFormat,
    #[error("unsupported ID-token signing algorithm; RS256 required")]
    Algorithm,
    #[error("invalid published signing-key set")]
    KeySet,
    #[error("no unique published key matches the ID token")]
    KeyId,
    #[error("invalid signing-key parameters")]
    KeyParameters,
    #[error("ID-token signature verification failed")]
    Signature,
    #[error("missing or malformed ID-token identity claims")]
    ClaimsFormat,
    #[error("ID-token issuer mismatch")]
    Issuer,
    #[error("ID-token audience mismatch")]
    Audience,
    #[error("ID-token authorized-party mismatch")]
    AuthorizedParty,
    #[error("ID-token nonce mismatch")]
    Nonce,
    #[error("invalid ID-token subject")]
    Subject,
    #[error("ID-token time claims failed validation; check the system clock")]
    Lifetime,
    #[error("the signed identity differs from the selected account")]
    SelectedSubject,
    #[error("missing, malformed or changed ChatGPT account claim")]
    AccountClaim,
    #[error("could not read the published signing keys as bounded JSON")]
    KeyTransport,
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: String,
    typ: Option<String>,
    crit: Option<serde_json::Value>,
    b64: Option<bool>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    #[serde(deserialize_with = "single_audience")]
    aud: String,
    #[serde(default, deserialize_with = "present_authorized_party")]
    azp: Option<String>,
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
    nonce: String,
    sub: String,
    #[serde(rename = "https://api.openai.com/auth")]
    account: Option<AccountClaim>,
}

#[derive(Deserialize)]
struct AccountClaim {
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct VerifiedIdentity {
    pub(super) subject: String,
    pub(super) account_id: String,
}

#[derive(Deserialize)]
struct Jwks {
    #[serde(deserialize_with = "bounded_keys")]
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    #[serde(rename = "use")]
    purpose: Option<String>,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

pub(super) fn verify(
    id_token: &str,
    jwks: &[u8],
    client_id: &str,
    nonce: &str,
    now_seconds: u64,
) -> Result<VerifiedIdentity, AuthorizationError> {
    let (header_segment, payload_segment, signature_segment) = split_token(id_token)?;
    let header = checked_header(header_segment)?;
    let signed_length = header_segment.len() + 1 + payload_segment.len();
    check_signature(
        &id_token.as_bytes()[..signed_length],
        signature_segment,
        jwks,
        &header.kid,
    )?;
    checked_claims(payload_segment, client_id, nonce, now_seconds)
}

fn split_token(id_token: &str) -> Result<(&str, &str, &str), AuthorizationError> {
    let invalid = || AuthorizationError::IdentityRejected(IdentityFailure::TokenFormat);
    if id_token.is_empty()
        || id_token.len() > super::MAX_ID_TOKEN_BYTES
        || !id_token.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid());
    }
    let mut segments = id_token.split('.');
    let header_segment = segments.next().ok_or_else(invalid)?;
    let payload_segment = segments.next().ok_or_else(invalid)?;
    let signature_segment = segments.next().ok_or_else(invalid)?;
    if segments.next().is_some() {
        return Err(invalid());
    }
    Ok((header_segment, payload_segment, signature_segment))
}

fn checked_header(segment: &str) -> Result<Header, AuthorizationError> {
    let header_error = || AuthorizationError::IdentityRejected(IdentityFailure::HeaderFormat);
    let header_bytes = decode_segment(segment, MAX_HEADER_BYTES).map_err(|_| header_error())?;
    let header: Header = serde_json::from_slice(&header_bytes).map_err(|_| header_error())?;
    if header.alg != "RS256" {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::Algorithm,
        ));
    }
    if header.kid.is_empty()
        || header.kid.len() > MAX_KEY_ID_BYTES
        || !header.kid.bytes().all(|byte| byte.is_ascii_graphic())
        || header.typ.as_deref().is_some_and(|kind| kind != "JWT")
        || header.crit.is_some()
        || header.b64.is_some()
    {
        return Err(header_error());
    }
    Ok(header)
}

// Exactly one RS256 signing key in the set carries the header's key id.
fn check_signature(
    signed: &[u8],
    signature_segment: &str,
    jwks: &[u8],
    kid: &str,
) -> Result<(), AuthorizationError> {
    let keys: Jwks = serde_json::from_slice(jwks)
        .map_err(|_| AuthorizationError::IdentityRejected(IdentityFailure::KeySet))?;
    let mut matches = keys
        .keys
        .iter()
        .filter(|key| key.kid.as_deref() == Some(kid));
    let key = matches
        .next()
        .ok_or(AuthorizationError::IdentityRejected(IdentityFailure::KeyId))?;
    if matches.next().is_some() {
        return Err(AuthorizationError::IdentityRejected(IdentityFailure::KeyId));
    }
    let key_error = || AuthorizationError::IdentityRejected(IdentityFailure::KeyParameters);
    if key.kty != "RSA"
        || key
            .purpose
            .as_deref()
            .is_some_and(|purpose| purpose != "sig")
        || key.alg.as_deref().is_some_and(|alg| alg != "RS256")
    {
        return Err(key_error());
    }
    let modulus = decode_segment(key.n.as_deref().ok_or_else(key_error)?, MAX_MODULUS_BYTES)
        .map_err(|_| key_error())?;
    let exponent = decode_segment(key.e.as_deref().ok_or_else(key_error)?, MAX_EXPONENT_BYTES)
        .map_err(|_| key_error())?;
    let signature = decode_segment(signature_segment, MAX_MODULUS_BYTES)?;
    if !(MIN_MODULUS_BYTES..=MAX_MODULUS_BYTES).contains(&modulus.len())
        || modulus[0] == 0
        || exponent.is_empty()
        || exponent[0] == 0
        || signature.len() != modulus.len()
    {
        return Err(key_error());
    }
    RsaPublicKeyComponents {
        n: &modulus,
        e: &exponent,
    }
    .verify(&RSA_PKCS1_2048_8192_SHA256, signed, &signature)
    .map_err(|_| AuthorizationError::IdentityRejected(IdentityFailure::Signature))
}

fn checked_claims(
    payload_segment: &str,
    client_id: &str,
    nonce: &str,
    now_seconds: u64,
) -> Result<VerifiedIdentity, AuthorizationError> {
    let claims_error = || AuthorizationError::IdentityRejected(IdentityFailure::ClaimsFormat);
    let claims_bytes =
        decode_segment(payload_segment, MAX_CLAIMS_BYTES).map_err(|_| claims_error())?;
    let claims: Claims = serde_json::from_slice(&claims_bytes).map_err(|_| claims_error())?;
    if claims.iss != ISSUER {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::Issuer,
        ));
    }
    if claims.aud != client_id {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::Audience,
        ));
    }
    if claims
        .azp
        .as_deref()
        .is_some_and(|party| party != client_id)
    {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::AuthorizedParty,
        ));
    }
    if claims.nonce != nonce {
        return Err(AuthorizationError::IdentityRejected(IdentityFailure::Nonce));
    }
    if claims.sub.is_empty()
        || claims.sub.len() > super::MAX_SUBJECT_BYTES
        || !claims.sub.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::Subject,
        ));
    }
    if claims.exp <= claims.iat
        || claims.exp.saturating_add(CLOCK_SKEW_SECONDS) <= now_seconds
        || claims.iat > now_seconds.saturating_add(CLOCK_SKEW_SECONDS)
        || claims
            .nbf
            .is_some_and(|not_before| not_before > now_seconds.saturating_add(CLOCK_SKEW_SECONDS))
    {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::Lifetime,
        ));
    }
    let account_id = claims
        .account
        .and_then(|account| account.chatgpt_account_id)
        .filter(|id| super::valid_account_id(id))
        .ok_or(AuthorizationError::IdentityRejected(
            IdentityFailure::AccountClaim,
        ))?;
    Ok(VerifiedIdentity {
        subject: claims.sub,
        account_id,
    })
}

fn decode_segment(value: &str, max_bytes: usize) -> Result<Vec<u8>, AuthorizationError> {
    let invalid = || AuthorizationError::IdentityRejected(IdentityFailure::TokenFormat);
    if value.is_empty() || value.len() > max_bytes.saturating_mul(4) / 3 + 4 {
        return Err(invalid());
    }
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| invalid())?;
    if decoded.len() > max_bytes || URL_SAFE_NO_PAD.encode(&decoded) != value {
        return Err(invalid());
    }
    Ok(decoded)
}

fn single_audience<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Audience {
        Text(String),
        Single([String; 1]),
    }

    Ok(match Audience::deserialize(deserializer)? {
        Audience::Text(value) | Audience::Single([value]) => value,
    })
}

fn present_authorized_party<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn bounded_keys<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Jwk>, D::Error> {
    crate::files::bounded_list::<D, Jwk, MAX_KEYS>(deserializer, "too many keys")
}
