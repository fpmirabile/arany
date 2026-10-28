use super::*;

pub(super) fn apply(current: &mut SessionView, event: &Event) -> Result<(), ReplayError> {
    match event {
        Event::CommandToolProbeStarted { probe } => {
            let identity = WorkspaceIdentity {
                device: probe.intent.workspace_device,
                inode: probe.intent.workspace_inode,
            };
            if current
                .command_tool_probe
                .as_ref()
                .is_some_and(|probe| probe.outcome.is_none())
                || current
                    .runs
                    .last()
                    .is_some_and(|run| matches!(run.status, RunStatus::Active | RunStatus::Pending))
                || current.workspace_identity != Some(identity)
            {
                return Err(ReplayError::InvalidTransition);
            }
            current.command_tool_probe = Some(probe.clone());
        }
        Event::CommandToolProbeFinished { outcome } => {
            let probe = current
                .command_tool_probe
                .as_mut()
                .ok_or(ReplayError::InvalidTransition)?;
            if probe.outcome.is_some() || probe.intent != outcome.observation.intent {
                return Err(ReplayError::InvalidTransition);
            }
            probe.outcome = Some((**outcome).clone());
        }
        _ => return Err(ReplayError::InvalidTransition),
    }
    Ok(())
}
