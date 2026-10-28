use super::{
    CallLimits, Effort, MAX_OUTPUT_TOKEN_CAP, MAX_RESPONSE_BYTES, Provenance, ProviderError,
    ProviderIdentity, ProviderRequest, http_failure, image, raw_reflects_secret,
    read_identity_body, resolve_native_effort, send_failure,
};
use std::time::Duration;

pub(super) const NATIVE_CALL_DEADLINE: Duration = Duration::from_secs(600);
pub(super) const REVIEWED_MODEL_CONCURRENCY: u8 = 3;
pub(super) const UNREVIEWED_MODEL_CONCURRENCY: u8 = 1;

pub(super) fn native_identity(profile: &str, model: String, effort: Effort) -> ProviderIdentity {
    let concurrency = if resolve_native_effort(profile, &model, Some(effort)).is_ok() {
        REVIEWED_MODEL_CONCURRENCY
    } else {
        UNREVIEWED_MODEL_CONCURRENCY
    };
    let mut identity = ProviderIdentity::new(profile, model, Provenance::native());
    identity.effort = Some(effort);
    identity.limits = CallLimits {
        concurrency,
        max_output_tokens: MAX_OUTPUT_TOKEN_CAP,
        deadline: NATIVE_CALL_DEADLINE,
        ..CallLimits::default()
    };
    identity
}

pub(super) fn admit_request(
    identity: &ProviderIdentity,
    request: &ProviderRequest,
) -> Result<(), ProviderError> {
    request.validate_scope()?;
    image::validate_images(request)?;
    if request.model != identity.model
        || !(1..=identity.limits.max_output_tokens).contains(&request.max_output_tokens)
    {
        return Err(ProviderError::Rejected);
    }
    Ok(())
}

pub(super) fn json_request(
    client: &reqwest::Client,
    endpoint: &str,
    key: &str,
    body: Vec<u8>,
) -> reqwest::RequestBuilder {
    client
        .post(endpoint)
        .bearer_auth(key)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .body(body)
}

pub(super) async fn post_json_observed(
    request: reqwest::RequestBuilder,
    key: &str,
    observe: impl FnOnce(&reqwest::header::HeaderMap) -> Result<(), ProviderError>,
) -> Result<Vec<u8>, ProviderError> {
    post_json_observed(request, key, |_| Ok(())).await
}

pub(super) async fn post_json_observed(
    request: reqwest::RequestBuilder,
    key: &str,
    observe: impl FnOnce(&reqwest::header::HeaderMap) -> Result<(), ProviderError>,
) -> Result<Vec<u8>, ProviderError> {
    let mut response = request.send().await.map_err(|error| send_failure(&error))?;
    observe(response.headers())?;
    if !response.status().is_success() {
        return Err(http_failure(&mut response).await);
    }
    let bytes = read_identity_body(&mut response, MAX_RESPONSE_BYTES).await?;
    if raw_reflects_secret(&bytes, key) {
        return Err(ProviderError::InvalidOutcome);
    }
    Ok(bytes)
}
