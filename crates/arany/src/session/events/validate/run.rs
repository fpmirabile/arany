use super::{Event, ReplayError};
use crate::session::events::{MAX_GOAL_ACTIVE_MS, ProviderCallDisposition};

impl Event {
    pub(super) fn validate_run(&self) -> Result<(), ReplayError> {
        match self {
            Self::RunStarted {
                config,
                active_deadline_ms,
                ..
            } => {
                if active_deadline_ms.is_some_and(|ms| ms == 0 || ms > MAX_GOAL_ACTIVE_MS) {
                    return Err(ReplayError::InvalidPayload);
                }
                config.validate()
            }
            Self::ProviderFallbackUsed { fallback, .. } => {
                if fallback
                    .rate_limits
                    .as_ref()
                    .is_none_or(|limits| limits.valid())
                    && fallback.from.valid()
                    && fallback.to.valid()
                    && fallback.from != fallback.to
                    && fallback.failure == ProviderCallDisposition::Unavailable
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ProviderRetryScheduled { retry, .. } => {
                use crate::provider::{MAX_PROVIDER_RETRIES, MAX_PROVIDER_RETRY_DELAY_MS};
                let backoff = crate::provider::ProviderRetry::backoff(retry.attempt).as_millis();
                if retry
                    .rate_limits
                    .as_ref()
                    .is_none_or(|limits| limits.valid())
                    && (1..=MAX_PROVIDER_RETRIES).contains(&retry.attempt)
                    && (backoff..=u128::from(MAX_PROVIDER_RETRY_DELAY_MS))
                        .contains(&u128::from(retry.delay_ms))
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::ProviderCallRecorded { record, .. } => record.validate(),
            Self::AssistantDisplayRecorded { chunk, .. } => chunk.validate(),
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}
