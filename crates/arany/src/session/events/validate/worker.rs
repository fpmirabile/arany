use super::*;

impl Event {
    pub(super) fn validate_worker(&self) -> Result<(), ReplayError> {
        match self {
            Self::WorkerStopObserved { .. } => Ok(()),
            Self::WorkerLeaseReleased { record, .. } => {
                if record.assignment_id.valid() && record.cause_sequence > 0 {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerVerificationRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.criterion_digest != [0; 32]
                    && !record.evidence.is_empty()
                    && record.evidence.len() <= MAX_WORKER_EVIDENCE
                    && record.evidence.iter().all(|s| *s > 0)
                    && record.evidence.windows(2).all(|s| s[0] < s[1])
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}
