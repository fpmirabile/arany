use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, PoisonError};

mod api;
pub(crate) mod headers;
pub use api::{ApiLimit, ApiLimitDialect, ApiLimitDimension, ApiLimits};

pub const MAX_API_LIMIT_ROWS: usize = ApiLimitDimension::ALL.len();
pub const MAX_RATE_LIMIT_BYTES: usize = 4096;
pub const MAX_LIMIT_TIMESTAMP: i64 = 253_402_300_799;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct UsedPercent(f64);

impl Eq for UsedPercent {}

impl TryFrom<f64> for UsedPercent {
    type Error = &'static str;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && (0.0..=100.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err("invalid used percentage")
        }
    }
}

impl From<UsedPercent> for f64 {
    fn from(value: UsedPercent) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitSource {
    Headers,
    Event,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitUnavailable {
    Invalid,
    Duplicate,
    Oversized,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitFailure {
    pub reason: LimitUnavailable,
    pub source: LimitSource,
    pub observed_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanWindow {
    pub source: LimitSource,
    pub observed_at: i64,
    #[serde(deserialize_with = "Option::deserialize")]
    pub used_percent: Option<UsedPercent>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub window_minutes: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub reset_at: Option<i64>,
}

impl PlanWindow {
    pub fn valid(&self) -> bool {
        (0..=MAX_LIMIT_TIMESTAMP).contains(&self.observed_at)
            && self.window_minutes != Some(0)
            && self
                .reset_at
                .is_none_or(|time| (0..=MAX_LIMIT_TIMESTAMP).contains(&time))
            && (self.used_percent.is_some()
                || self.window_minutes.is_some()
                || self.reset_at.is_some())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiLimits>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub primary: Option<PlanWindow>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub secondary: Option<PlanWindow>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub unavailable: Option<LimitFailure>,
}

impl RateLimits {
    pub fn valid(&self) -> bool {
        serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_RATE_LIMIT_BYTES)
            && (self.primary.is_some()
                || self.secondary.is_some()
                || self.unavailable.is_some()
                || self.api.is_some())
            && self.api.as_ref().is_none_or(|api| {
                api.valid()
                    && self.primary.is_none()
                    && self.secondary.is_none()
                    && self.unavailable.is_none()
            })
            && self
                .unavailable
                .is_none_or(|failure| (0..=MAX_LIMIT_TIMESTAMP).contains(&failure.observed_at))
            && self
                .primary
                .iter()
                .chain(&self.secondary)
                .all(PlanWindow::valid)
    }
}

#[derive(Clone, Default)]
pub struct RateLimitSlot(Arc<Mutex<Option<RateLimits>>>);

impl RateLimitSlot {
    pub fn record(&self, sample: RateLimits) {
        if !sample.valid() {
            return;
        }
        let mut slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if slot
            .as_ref()
            .is_some_and(|current| current.api.is_some() != sample.api.is_some())
        {
            return;
        }
        if sample.api.is_some() {
            *slot = Some(sample);
            return;
        }
        let current = slot.get_or_insert_with(RateLimits::default);
        for (old, new) in [
            (&mut current.primary, sample.primary),
            (&mut current.secondary, sample.secondary),
        ] {
            if new.is_some() {
                *old = new;
            }
        }
        current.unavailable = sample.unavailable.or(current.unavailable);
    }

    pub(crate) fn snapshot(&self) -> Option<RateLimits> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn take(&self) -> Option<RateLimits> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}
