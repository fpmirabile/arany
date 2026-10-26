use super::*;

mod agent;
mod goal;
mod run;
mod session;
mod tool;
mod worker;

impl Event {
    pub(in crate::session) fn validate(&self) -> Result<(), ReplayError> {
        if self.scope().1.is_some_and(|id| !id.valid()) {
            return Err(ReplayError::InvalidPayload);
        }
        let valid_goal_ids = match self {
            Self::GoalStarted { goal_id, .. }
            | Self::GoalRunLinked { goal_id, .. }
            | Self::GoalResumed { goal_id }
            | Self::GoalPlanContractRecorded { goal_id, .. }
            | Self::GoalCompactionCallReserved { goal_id, .. }
            | Self::GoalContextCompacted { goal_id, .. }
            | Self::GoalSteeringNoteRecorded { goal_id, .. }
            | Self::AgentSteeringNoteRecorded { goal_id, .. }
            | Self::GoalFinished { goal_id, .. } => goal_id.valid(),
            _ => true,
        };
        if !valid_goal_ids {
            return Err(ReplayError::InvalidPayload);
        }
        match self {
            Self::SupervisorVerdictRecorded { .. }
            | Self::WorkspaceRewindStarted { .. }
            | Self::WorkspaceRewindFinished { .. }
            | Self::SessionStarted { .. }
            | Self::SessionRenamed { .. }
            | Self::SessionDefaultChanged { .. }
            | Self::SessionForked { .. }
            | Self::ContextCompacted { .. }
            | Self::ToolResultsCleared { .. }
            | Self::MessageAccepted { .. }
            | Self::MessageCommitted { .. } => Event::validate_session(self),
            Self::ProviderSetupEditStarted { .. } | Self::ProviderSetupEditFinished { .. } => {
                Event::validate_setup_edit(self)
            }
            Self::GoalStarted { .. }
            | Self::GoalRunLinked { .. }
            | Self::GoalResumed { .. }
            | Self::GoalCompactionCallReserved { .. }
            | Self::GoalContextCompacted { .. }
            | Self::GoalPlanContractRecorded { .. }
            | Self::GoalSteeringNoteRecorded { .. }
            | Self::AgentSteeringNoteRecorded { .. }
            | Self::GoalFinished { .. }
            | Self::GoalProviderCallReserved { .. } => Event::validate_goal(self),
            Self::ToolStarted { .. }
            | Self::ToolActiveTimeReserved { .. }
            | Self::ToolFinished { .. }
            | Self::HookStarted { .. }
            | Self::HookFinished { .. }
            | Self::ToolPermissionDecided { .. } => Event::validate_tool(self),
            Self::AgentSpawned { .. } => Event::validate_agent(self),
            Self::RunStarted { .. }
            | Self::ProviderFallbackUsed { .. }
            | Self::ProviderRetryScheduled { .. }
            | Self::ProviderCallRecorded { .. }
            | Self::AssistantDisplayRecorded { .. } => Event::validate_run(self),
            Self::AgentFinished { .. } | Self::RunFinished { .. } => Event::validate_agent(self),
            Self::WorkerDataChunk { .. }
            | Self::WorkerDataSealed { .. }
            | Self::WorkerStopObserved { .. }
            | Self::WorkerHandoverRecorded { .. }
            | Self::WorkerIntegrationRecorded { .. }
            | Self::WorkerHandoverDiscarded { .. }
            | Self::WorkerLeaseReleased { .. }
            | Self::WorkerVerificationRecorded { .. } => Event::validate_worker(self),
        }
    }
}

pub(super) fn bounded(value: &str, max: usize) -> Result<(), ReplayError> {
    if value.is_empty() || value.len() > max {
        return Err(ReplayError::InvalidPayload);
    }
    Ok(())
}
