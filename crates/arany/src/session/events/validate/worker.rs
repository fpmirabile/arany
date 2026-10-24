use super::*;

impl Event {
    pub(super) fn validate_worker(&self) -> Result<(), ReplayError> {
        match self {
            Self::WorkerDataChunk { record, .. } => {
                if record.decoded().is_some() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerDataSealed { record, .. } => {
                if record.data.valid() && record.tool_id.get_version() == Some(Version::SortRand) {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerStopObserved { .. } => Ok(()),
            Self::WorkerHandoverRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.assignment_id == record.data.assignment_id
                    && record.data.valid()
                    && record.base_digest != [0; 32]
                    && record.criterion_evidence.len() <= 8
                    && record.criterion_evidence.iter().enumerate().all(|(i, s)| {
                        *s > 0 && record.criterion_evidence[..i].iter().all(|p| p < s)
                    })
                    && matches!(
                        record.data.purpose,
                        WorkerDataPurpose::Handover | WorkerDataPurpose::Mutation
                    )
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerIntegrationRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.handover_sequence > 0
                    && record.finished_sequence > record.handover_sequence
                    && record.tool_id.get_version() == Some(Version::SortRand)
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerHandoverDiscarded { record, .. }
            | Self::WorkerLeaseReleased { record, .. } => {
                if record.assignment_id.valid()
                    && record.cause_sequence > 0
                    && record.handover_sequence != Some(0)
                {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkerVerificationRecorded { record, .. } => {
                if record.assignment_id.valid()
                    && record.criterion_digest != [0; 32]
                    && !record.evidence.is_empty()
                    && record.evidence.len() <= 8
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
