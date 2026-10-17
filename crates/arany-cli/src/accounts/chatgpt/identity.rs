use super::AuthorizationError;
use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Deserializer, de};
use std::fmt;

const ISSUER: &str = "https://auth.openai.com";
const MAX_ID_TOKEN_BYTES: usize = 16 * 1024;
const MAX_KEYS: usize = 16;
const CLOCK_SKEW_SECONDS: u64 = 5;

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
) -> Result<String, AuthorizationError> {
    let invalid = || AuthorizationError::IdentityRejected(IdentityFailure::TokenFormat);
    if id_token.is_empty()
        || id_token.len() > MAX_ID_TOKEN_BYTES
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
    let header_error = || AuthorizationError::IdentityRejected(IdentityFailure::HeaderFormat);
    let header_bytes = decode_segment(header_segment, 1024).map_err(|_| header_error())?;
    let header: Header = serde_json::from_slice(&header_bytes).map_err(|_| header_error())?;
    if header.alg != "RS256" {
        return Err(AuthorizationError::IdentityRejected(
            IdentityFailure::Algorithm,
        ));
    }
    if header.kid.is_empty()
        || header.kid.len() > 128
        || !header.kid.bytes().all(|byte| byte.is_ascii_graphic())
        || header.typ.as_deref().is_some_and(|kind| kind != "JWT")
        || header.crit.is_some()
        || header.b64.is_some()
    {
        return Err(header_error());
    }
    let keys: Jwks = serde_json::from_slice(jwks)
        .map_err(|_| AuthorizationError::IdentityRejected(IdentityFailure::KeySet))?;
    let mut matches = keys
        .keys
        .iter()
        .filter(|key| key.kid.as_deref() == Some(header.kid.as_str()));
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
    let modulus =
        decode_segment(key.n.as_deref().ok_or_else(key_error)?, 1024).map_err(|_| key_error())?;
    let exponent =
        decode_segment(key.e.as_deref().ok_or_else(key_error)?, 8).map_err(|_| key_error())?;
    let signature = decode_segment(signature_segment, 1024)?;
    if !(256..=1024).contains(&modulus.len())
        || modulus[0] == 0
        || exponent.is_empty()
        || exponent[0] == 0
        || signature.len() != modulus.len()
    {
        return Err(key_error());
    }
    let signed_length = header_segment.len() + 1 + payload_segment.len();
    RsaPublicKeyComponents {
        n: &modulus,
        e: &exponent,
    }
    .verify(
        &RSA_PKCS1_2048_8192_SHA256,
        &id_token.as_bytes()[..signed_length],
        &signature,
    )
    .map_err(|_| AuthorizationError::IdentityRejected(IdentityFailure::Signature))?;

    let claims_error = || AuthorizationError::IdentityRejected(IdentityFailure::ClaimsFormat);
    let claims_bytes = decode_segment(payload_segment, 8192).map_err(|_| claims_error())?;
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
        || claims.sub.len() > 512
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
    Ok(claims.sub)
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
    struct AudienceVisitor;

    impl<'de> de::Visitor<'de> for AudienceVisitor {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("one audience as a string or a one-element string array")
        }

        fn visit_str<E>(self, value: &str) -> Result<String, E>
        where
            E: de::Error,
        {
            Ok(value.to_owned())
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<String, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let audience = sequence
                .next_element::<String>()?
                .ok_or_else(|| de::Error::custom("missing audience"))?;
            if sequence.next_element::<de::IgnoredAny>()?.is_some() {
                return Err(de::Error::custom("additional audiences are not trusted"));
            }
            Ok(audience)
        }
    }

    deserializer.deserialize_any(AudienceVisitor)
}

fn present_authorized_party<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn bounded_keys<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Jwk>, D::Error> {
    crate::cli::bounded_list::<D, Jwk, MAX_KEYS>(deserializer, "too many keys")
}
