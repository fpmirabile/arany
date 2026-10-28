use super::LimitUnavailable;
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
