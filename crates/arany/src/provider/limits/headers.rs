use super::{
    ApiLimit, ApiLimitDialect, ApiLimitDimension, ApiLimits, LimitFailure, LimitSource,
    LimitUnavailable, RateLimitSlot, RateLimits,
};
use reqwest::header::HeaderMap;

pub(crate) const MAX_LIMIT_HEADER_VALUES: usize = 32;
pub(crate) const MAX_LIMIT_HEADER_VALUE_BYTES: usize = 128;
pub(crate) const MAX_LIMIT_HEADER_BYTES: usize = super::MAX_RATE_LIMIT_BYTES;

pub(crate) fn observed_at() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|value| i64::try_from(value.as_secs()).ok())
        .unwrap_or(-1)
}

pub(crate) type HeaderField<'a> = Result<Option<&'a str>, LimitUnavailable>;

pub(crate) fn fields<'a, const N: usize>(
    headers: &'a HeaderMap,
    names: [&str; N],
) -> Result<[HeaderField<'a>; N], LimitUnavailable> {
    let mut result = [Ok(None); N];
    let mut count = 0;
    let mut bytes = 0;
    for (index, name) in names.into_iter().enumerate() {
        for value in headers.get_all(name).iter() {
            count += 1;
            bytes += value.as_bytes().len();
            if count > MAX_LIMIT_HEADER_VALUES || bytes > MAX_LIMIT_HEADER_BYTES {
                return Err(LimitUnavailable::Oversized);
            }
            if result[index].is_err() {
                continue;
            }
            result[index] = if result[index].is_ok_and(|value| value.is_some()) {
                Err(LimitUnavailable::Duplicate)
            } else if value.as_bytes().len() > MAX_LIMIT_HEADER_VALUE_BYTES {
                Err(LimitUnavailable::Oversized)
            } else {
                value
                    .to_str()
                    .map(Some)
                    .map_err(|_| LimitUnavailable::Invalid)
            };
        }
    }
    Ok(result)
}

pub(crate) struct ApiHeaders<const N: usize> {
    pub names: [&'static str; N],
    pub dimensions: &'static [ApiLimitDimension],
    pub dialect: ApiLimitDialect,
    pub reset_at: fn(&str, i64) -> Option<i64>,
}

impl<const N: usize> ApiHeaders<N> {
    pub fn collect(&self, headers: &HeaderMap, slot: &RateLimitSlot, clock: impl FnOnce() -> i64) {
        if !self.names.iter().any(|name| headers.contains_key(*name)) {
            return;
        }
        let at = clock();
        let mut api = ApiLimits {
            dialect: self.dialect,
            rows: Vec::new(),
            unavailable: None,
        };
        match fields(headers, self.names) {
            Err(reason) => {
                api.unavailable = Some(LimitFailure {
                    reason,
                    source: LimitSource::Headers,
                    observed_at: at,
                });
            }
            Ok(fields) => {
                for (dimension, values) in self.dimensions.iter().zip(fields.as_chunks::<3>().0) {
                    if values.iter().all(|value| *value == Ok(None)) {
                        continue;
                    }
                    api.rows.push(row(*dimension, values, at, self.reset_at));
                }
            }
        }
        slot.record(RateLimits {
            api: Some(api),
            ..RateLimits::default()
        });
    }
}

fn row(
    dimension: ApiLimitDimension,
    values: &[HeaderField<'_>],
    observed_at: i64,
    reset_at: fn(&str, i64) -> Option<i64>,
) -> ApiLimit {
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

pub(crate) fn unsigned(value: &str) -> Result<u64, LimitUnavailable> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(LimitUnavailable::Invalid);
    }
    value.parse().map_err(|_| LimitUnavailable::Invalid)
}
