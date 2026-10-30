use super::{Event, ReplayError, bounded, checked};
use crate::session::{
    events::{
        AgentDisposition, AgentRole, ChildGrant, MAX_ASSISTANT_MESSAGE_BYTES, RunDisposition,
        SpawnPayload,
    },
    worker::ChildAssignment,
};

impl Event {
    pub(super) fn validate_agent(&self) -> Result<(), ReplayError> {
        match self {
            Self::AgentSpawned { .. } => self.validate_spawn(),
            Self::AgentFinished { .. } => self.validate_agent_finish(),
            Self::RunFinished {
                disposition,
                failure,
                ..
            } => checked((*disposition == RunDisposition::Failed) == failure.is_some()),
            _ => Err(ReplayError::InvalidPayload),
        }
    }

    fn validate_spawn(&self) -> Result<(), ReplayError> {
        let Self::AgentSpawned {
            role,
            ordinal,
            objective,
            grant,
            definition,
            profile,
            assignment,
            ..
        } = self
        else {
            return Err(ReplayError::InvalidPayload);
        };
        if !profile
            .as_deref()
            .is_none_or(crate::provider::valid_profile_name)
            || !serde_json::to_vec(&SpawnPayload {
                role: *role,
                ordinal: *ordinal,
                objective: objective.clone(),
                grant: *grant,
                definition: definition.clone(),
                profile: profile.clone(),
                assignment: assignment.clone(),
            })
            .is_ok_and(|b| b.len() <= crate::store::MAX_EVENT_BYTES)
        {
            return Err(ReplayError::InvalidPayload);
        }
        match (role, grant) {
            (AgentRole::Primary, None)
                if *ordinal == 0
                    && objective.is_none()
                    && definition.is_none()
                    && assignment.is_none() =>
            {
                Ok(())
            }
            (AgentRole::Child, Some(_))
                if (1..=8).contains(ordinal)
                    && definition.as_ref().is_none_or(|name| {
                        name.len() <= 64 && crate::validate::valid_name(name)
                    }) =>
            {
                if (matches!(*grant, Some(ChildGrant::EditOnly { .. }))) != assignment.is_some() {
                    return Err(ReplayError::InvalidPayload);
                }
                if let Some(p) = assignment
                    && (!(ChildAssignment::Editor {
                        assignment: Box::new(p.request.clone()),
                    })
                    .valid()
                        || !p.grant.valid()
                        || p.policy_digest == [0; 32]
                        || p.permissions_digest == Some([0; 32]))
                {
                    return Err(ReplayError::InvalidPayload);
                }
                bounded(
                    objective.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    2 * 1024,
                )
            }
            _ => Err(ReplayError::InvalidPayload),
        }
    }

    fn validate_agent_finish(&self) -> Result<(), ReplayError> {
        let Self::AgentFinished {
            disposition,
            summary,
            result,
            worker,
            ..
        } = self
        else {
            return Err(ReplayError::InvalidPayload);
        };
        if worker.as_ref().is_some_and(|o| !o.valid()) {
            return Err(ReplayError::InvalidPayload);
        }
        match disposition {
            AgentDisposition::Finished => {
                bounded(
                    summary.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    2 * 1024,
                )?;
                bounded(
                    result.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    MAX_ASSISTANT_MESSAGE_BYTES,
                )
            }
            AgentDisposition::Failed
            | AgentDisposition::Cancelled
            | AgentDisposition::StoppedByHook
                if summary.is_none() && result.is_none() =>
            {
                Ok(())
            }
            // A failed editor carries the Engine's note on why nothing was integrated.
            AgentDisposition::Failed if worker.is_some() => {
                bounded(
                    summary.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    crate::session::worker::MAX_EDITOR_CHANGE_REPORT_BYTES,
                )?;
                bounded(
                    result.as_deref().ok_or(ReplayError::InvalidPayload)?,
                    crate::session::worker::MAX_EDITOR_CHANGE_REPORT_BYTES,
                )
            }
            _ => Err(ReplayError::InvalidPayload),
        }
    }
}

#[cfg(test)]
mod tests;
