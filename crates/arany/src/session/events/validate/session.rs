use super::*;

impl Event {
    pub(super) fn validate_session(&self) -> Result<(), ReplayError> {
        match self {
            Self::SupervisorVerdictRecorded { verdict, .. } => {
                if verdict.valid() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkspaceRewindStarted { record } => {
                if record.valid() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::WorkspaceRewindFinished { record } => {
                if record.valid() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            Self::SessionStarted { title, .. } | Self::SessionRenamed { title } => {
                bounded(title, 128)
            }
            Self::SessionDefaultChanged { defaults } => defaults.validate(),
            Self::SessionForked {
                title,
                source_sequence,
                ..
            } if *source_sequence > 0 => bounded(title, 128),
            Self::SessionForked { .. } => Err(ReplayError::InvalidPayload),
            Self::ContextCompacted { record, .. } => record.validate(),
            Self::ToolResultsCleared {
                covered_tool_count,
                cleared_bytes,
                ..
            } => {
                if *covered_tool_count == 0
                    || *cleared_bytes as usize > crate::session::MAX_WINDOW_CONTENT_BYTES
                {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::MessageAccepted {
                text,
                images,
                skill,
                ..
            } => {
                if skill
                    .as_deref()
                    .is_some_and(|name| !crate::validate::valid_name(name))
                {
                    return Err(ReplayError::InvalidPayload);
                }
                if text.len() > super::MAX_USER_MESSAGE_BYTES
                    || text.is_empty() && images.is_empty()
                    || !crate::provider::valid_image_collection(images)
                {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::MessageCommitted { text, .. } => {
                bounded(text, super::MAX_ASSISTANT_MESSAGE_BYTES)
            }
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}
