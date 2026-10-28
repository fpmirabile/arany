use crate::provider::limits::headers::{HeaderField, fields, observed_at};
use crate::provider::{
    ApiLimit, ApiLimitDialect, ApiLimitDimension, ApiLimits, LimitFailure, LimitSource,
    LimitUnavailable, MAX_LIMIT_TIMESTAMP, ProviderError, RateLimitSlot, RateLimits,
};
use reqwest::header::HeaderMap;

const RESET_FRACTION_DIGITS: usize = 3;
const MILLIS_PER_SECOND: u64 = 1000;
const HEADER_NAMES: [&str; 9] = [
    "x-ratelimit-limit-requests",
    "x-ratelimit-remaining-requests",
    "x-ratelimit-reset-requests",
    "x-ratelimit-limit-tokens",
    "x-ratelimit-remaining-tokens",
    "x-ratelimit-reset-tokens",
    "x-ratelimit-limit-project-tokens",
    "x-ratelimit-remaining-project-tokens",
    "x-ratelimit-reset-project-tokens",
];

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
    headers_at(headers, slot, observed_at);
    Ok(())
}

fn headers_at(headers: &HeaderMap, slot: &RateLimitSlot, clock: impl FnOnce() -> i64) {
    if !HEADER_NAMES.iter().any(|name| headers.contains_key(*name)) {
        return;
    }
    let at = clock();
    let mut api = ApiLimits {
        dialect: ApiLimitDialect::OpenAi,
        rows: Vec::new(),
        unavailable: None,
    };
    match fields(headers, HEADER_NAMES) {
        Err(reason) => {
            api.unavailable = Some(LimitFailure {
                reason,
                source: LimitSource::Headers,
                observed_at: at,
            });
        }
        Ok(fields) => {
            for (dimension, values) in [
                ApiLimitDimension::Requests,
                ApiLimitDimension::Tokens,
                ApiLimitDimension::ProjectTokens,
            ]
            .into_iter()
            .zip(fields.as_chunks::<3>().0)
            {
                if values.iter().all(|value| *value == Ok(None)) {
                    continue;
                }
                api.rows.push(row(dimension, values, at));
            }
        }
    }
    slot.record(RateLimits {
        api: Some(api),
        ..RateLimits::default()
    });
}

fn row(dimension: ApiLimitDimension, values: &[HeaderField<'_>], observed_at: i64) -> ApiLimit {
    let mut row = ApiLimit {
        dimension,
        source: LimitSource::Headers,
        observed_at,
        limit: None,
        remaining: None,
        reset_at: None,
        unavailable: None,
    };
    let parsed = (|| {
        let limit = values[0]?.map(unsigned).transpose()?;
        let remaining = values[1]?.map(unsigned).transpose()?;
        let reset_at = values[2]?
            .map(|value| reset_at(value, observed_at).ok_or(LimitUnavailable::Invalid))
            .transpose()?;
        Ok::<_, LimitUnavailable>((limit, remaining, reset_at))
    })();
    match parsed {
        Ok((limit, remaining, reset_at)) => {
            row.limit = limit;
            row.remaining = remaining;
            row.reset_at = reset_at;
        }
        Err(reason) => row.unavailable = Some(reason),
    }
    row
}

fn unsigned(value: &str) -> Result<u64, LimitUnavailable> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LimitUnavailable::Invalid);
    }
    value.parse().map_err(|_| LimitUnavailable::Invalid)
}

fn reset_at(mut value: &str, observed_at: i64) -> Option<i64> {
    let mut millis = 0_u64;
    let mut previous = None;
    while !value.is_empty() {
        let digits = value.bytes().take_while(u8::is_ascii_digit).count();
        let whole = unsigned(&value[..digits]).ok()?;
        value = &value[digits..];
        let mut fraction = None;
        if let Some(rest) = value.strip_prefix('.') {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if !(1..=RESET_FRACTION_DIGITS).contains(&digits) {
                return None;
            }
            fraction =
                Some(unsigned(&rest[..digits]).ok()?.checked_mul(
                    10_u64.pow(u32::try_from(RESET_FRACTION_DIGITS - digits).ok()?),
                )?);
            value = &rest[digits..];
        }
        let (unit, scale, rest) = if let Some(rest) = value.strip_prefix("ms") {
            (4, 1, rest)
        } else {
            let (unit, scale) = match value.as_bytes().first()? {
                b'd' => (0, 86_400 * MILLIS_PER_SECOND),
                b'h' => (1, 3600 * MILLIS_PER_SECOND),
                b'm' => (2, 60 * MILLIS_PER_SECOND),
                b's' => (3, MILLIS_PER_SECOND),
                _ => return None,
            };
            (unit, scale, &value[1..])
        };
        if previous.is_some_and(|previous| unit <= previous) || fraction.is_some() && unit != 3 {
            return None;
        }
        previous = Some(unit);
        millis = millis
            .checked_add(whole.checked_mul(scale)?)?
            .checked_add(fraction.unwrap_or(0))?;
        value = rest;
    }
    previous?;
    let seconds = millis.checked_add(MILLIS_PER_SECOND - 1)? / MILLIS_PER_SECOND;
    observed_at
        .checked_add(i64::try_from(seconds).ok()?)
        .filter(|at| (0..=MAX_LIMIT_TIMESTAMP).contains(at))
}

#[cfg(test)]
mod tests;
