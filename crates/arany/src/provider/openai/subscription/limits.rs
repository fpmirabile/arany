use crate::provider::{
    LimitFailure, LimitSource, LimitUnavailable, PlanWindow, ProviderError, RateLimitSlot,
    RateLimits, UsedPercent,
};
use reqwest::header::HeaderMap;
use serde_json::Value;

fn observed_at() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|value| i64::try_from(value.as_secs()).ok())
        .unwrap_or(-1)
}

pub(super) fn headers(
    headers: &HeaderMap,
    token: &str,
    slot: &RateLimitSlot,
) -> Result<(), ProviderError> {
    if headers
        .values()
        .any(|value| crate::provider::raw_reflects_secret(value.as_bytes(), token))
    {
        return Err(ProviderError::InvalidOutcome);
    }
    headers_at(headers, slot, observed_at);
    Ok(())
}

pub(super) fn headers_at(headers: &HeaderMap, slot: &RateLimitSlot, clock: impl FnOnce() -> i64) {
    let mut sample = RateLimits::default();
    let mut fields = [[None; 3]; 2];
    let mut failures = [None; 2];
    let mut count = 0;
    let mut bytes = 0;
    'windows: for (index, prefix) in ["primary", "secondary"].into_iter().enumerate() {
        for (field, suffix) in ["used-percent", "window-minutes", "reset-at"]
            .into_iter()
            .enumerate()
        {
            let name = format!("x-codex-{prefix}-{suffix}");
            for value in headers.get_all(name).iter() {
                count += 1;
                bytes += value.as_bytes().len();
                if count > 32 || bytes > 4096 {
                    failures = [Some(LimitUnavailable::Oversized); 2];
                    break 'windows;
                }
                if value.as_bytes().len() > 128 {
                    failures[index] = Some(LimitUnavailable::Oversized);
                    break;
                }
                if fields[index][field].is_some() {
                    failures[index] = Some(LimitUnavailable::Duplicate);
                }
                match value.to_str() {
                    Ok(value) => fields[index][field] = Some(value),
                    Err(_) => failures[index] = Some(LimitUnavailable::Invalid),
                }
            }
        }
    }
    if count == 0 {
        return;
    }
    let at = clock();
    for (index, target) in [&mut sample.primary, &mut sample.secondary]
        .into_iter()
        .enumerate()
    {
        if let Some(reason) = failures[index] {
            sample.unavailable = Some(LimitFailure {
                reason,
                source: LimitSource::Headers,
                observed_at: at,
            });
            continue;
        }
        let [percent, minutes, reset] = fields[index];
        if percent.is_none() && minutes.is_none() && reset.is_none() {
            continue;
        }
        let parsed = (|| {
            let window = PlanWindow {
                source: LimitSource::Headers,
                observed_at: at,
                used_percent: percent
                    .map(str::parse::<f64>)
                    .transpose()
                    .ok()?
                    .map(UsedPercent::try_from)
                    .transpose()
                    .ok()?,
                window_minutes: minutes.map(str::parse).transpose().ok()?,
                reset_at: reset.map(str::parse).transpose().ok()?,
            };
            window.valid().then_some(window)
        })();
        match parsed {
            Some(window) => *target = Some(window),
            None => {
                sample.unavailable = Some(LimitFailure {
                    reason: LimitUnavailable::Invalid,
                    source: LimitSource::Headers,
                    observed_at: at,
                })
            }
        }
    }
    slot.record(sample);
}

pub(super) fn event(value: &Value, slot: &RateLimitSlot) {
    event_at(value, slot, observed_at);
}

pub(super) fn event_at(value: &Value, slot: &RateLimitSlot, clock: impl FnOnce() -> i64) {
    for name in ["metered_limit_name", "limit_name"] {
        if let Some(meter) = value.get(name)
            && meter.as_str() != Some("codex")
        {
            return;
        }
    }
    let at = clock();
    let mut sample = RateLimits::default();
    if let Some(limits) = value.get("rate_limits").and_then(Value::as_object) {
        for (name, target) in [
            ("primary", &mut sample.primary),
            ("secondary", &mut sample.secondary),
        ] {
            let Some(value) = limits.get(name).filter(|v| !v.is_null()) else {
                continue;
            };
            let parsed = (|| {
                value.as_object()?;
                let window = PlanWindow {
                    source: LimitSource::Event,
                    observed_at: at,
                    used_percent: optional(value, "used_percent", |v| {
                        UsedPercent::try_from(v.as_f64()?).ok()
                    })?,
                    window_minutes: optional(value, "window_minutes", |v| {
                        u32::try_from(v.as_u64()?).ok()
                    })?,
                    reset_at: optional(value, "reset_at", Value::as_i64)?,
                };
                window.valid().then_some(window)
            })();
            match parsed {
                Some(window) => *target = Some(window),
                None => {
                    sample.unavailable = Some(LimitFailure {
                        reason: LimitUnavailable::Invalid,
                        source: LimitSource::Event,
                        observed_at: at,
                    })
                }
            }
        }
    } else {
        sample.unavailable = Some(LimitFailure {
            reason: LimitUnavailable::Invalid,
            source: LimitSource::Event,
            observed_at: at,
        });
    }
    slot.record(sample);
}

fn optional<T>(
    value: &Value,
    name: &str,
    parse: impl FnOnce(&Value) -> Option<T>,
) -> Option<Option<T>> {
    match value.get(name) {
        None | Some(Value::Null) => Some(None),
        Some(value) => parse(value).map(Some),
    }
}
