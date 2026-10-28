use crate::provider::limits::headers::{ApiHeaders, observed_at};
use crate::provider::{
    ApiLimitDialect, ApiLimitDimension, MAX_LIMIT_TIMESTAMP, ProviderError, RateLimitSlot,
};
use reqwest::header::HeaderMap;

const HEADERS: ApiHeaders<18> = ApiHeaders {
    names: [
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-limit",
        "anthropic-ratelimit-tokens-remaining",
        "anthropic-ratelimit-tokens-reset",
        "anthropic-ratelimit-input-tokens-limit",
        "anthropic-ratelimit-input-tokens-remaining",
        "anthropic-ratelimit-input-tokens-reset",
        "anthropic-ratelimit-output-tokens-limit",
        "anthropic-ratelimit-output-tokens-remaining",
        "anthropic-ratelimit-output-tokens-reset",
        "anthropic-priority-input-tokens-limit",
        "anthropic-priority-input-tokens-remaining",
        "anthropic-priority-input-tokens-reset",
        "anthropic-priority-output-tokens-limit",
        "anthropic-priority-output-tokens-remaining",
        "anthropic-priority-output-tokens-reset",
    ],
    dimensions: &[
        ApiLimitDimension::Requests,
        ApiLimitDimension::Tokens,
        ApiLimitDimension::InputTokens,
        ApiLimitDimension::OutputTokens,
        ApiLimitDimension::PriorityInputTokens,
        ApiLimitDimension::PriorityOutputTokens,
    ],
    dialect: ApiLimitDialect::Anthropic,
    reset_at: |value, _| {
        crate::timestamp::rfc3339_seconds(value).filter(|at| (0..=MAX_LIMIT_TIMESTAMP).contains(at))
    },
};

pub(super) fn headers(
    headers: &HeaderMap,
    key: &str,
    slot: &RateLimitSlot,
) -> Result<(), ProviderError> {
    if headers
        .values()
        .any(|value| crate::provider::raw_reflects_secret(value.as_bytes(), key))
    {
        return Err(ProviderError::InvalidOutcome);
    }
    HEADERS.collect(headers, slot, observed_at);
    Ok(())
}

#[cfg(test)]
mod tests;
