use super::*;

impl Event {
    pub(super) fn validate_goal(&self) -> Result<(), ReplayError> {
        match self {
            Self::GoalStarted {
                objective,
                include_paths,
                workspace_identity: WorkspaceIdentity { inode, .. },
                budget,
                resource_settings,
                continues,
                ..
            } => {
                if include_paths.as_ref().is_some_and(|paths| {
                    paths.len() > crate::session::input::MAX_INCLUDES
                        || paths
                            .iter()
                            .any(|path| !crate::session::input::valid_include_path(path))
                }) {
                    return Err(ReplayError::InvalidPayload);
                }
                if continues.is_some_and(|id| !id.valid()) {
                    return Err(ReplayError::InvalidPayload);
                }
                if objective.len() > super::MAX_USER_MESSAGE_BYTES || *inode == 0 {
                    return Err(ReplayError::InvalidPayload);
                }
                budget.validate()?;
                if !resource_settings.valid()
                    || resource_settings
                        .build_jobs
                        .is_some_and(|jobs| jobs > resource_settings.build_processes)
                {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalRunLinked { .. } | Self::GoalResumed { .. } => Ok(()),
            Self::GoalCompactionCallReserved { .. } => Ok(()),
            Self::GoalContextCompacted {
                covered_tool_count,
                record,
                ..
            } => {
                if *covered_tool_count == 0 || record.validate().is_err() {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalPlanContractRecorded { contract, .. } => {
                contract.validate()?;
                Ok(())
            }
            Self::GoalSteeringNoteRecorded { text, .. }
            | Self::AgentSteeringNoteRecorded { text, .. } => {
                if text.trim().is_empty() || text.len() > super::MAX_STEERING_NOTE_BYTES {
                    return Err(ReplayError::InvalidPayload);
                }
                Ok(())
            }
            Self::GoalFinished {
                status,
                report,
                exhausted,
                budget_refusal,
                ..
            } => {
                if !matches!(status, GoalStatus::Completed | GoalStatus::Failed) {
                    return Err(ReplayError::InvalidPayload);
                }
                if let Some(refusal) = budget_refusal {
                    if !exhausted {
                        return Err(ReplayError::InvalidPayload);
                    }
                    refusal.validate()?;
                }
                report.validate()
            }
            Self::GoalProviderCallReserved { goal_id, .. } => {
                if goal_id.valid() {
                    Ok(())
                } else {
                    Err(ReplayError::InvalidPayload)
                }
            }
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}
