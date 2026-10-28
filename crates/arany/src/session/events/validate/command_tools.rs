use super::*;

impl Event {
    pub(super) fn validate_command_probe(&self) -> Result<(), ReplayError> {
        use crate::tools::permissions::{HookEvent, HookResult};
        let valid = match self {
            Self::CommandToolProbeStarted { probe } => {
                probe.outcome.is_none()
                    && probe.intent.valid()
                    && probe.tool.valid()
                    && probe.intent.call == probe.tool.call()
                    && match probe.tool.role {
                        crate::tools::CommandToolRole::Rewrite => probe.intent.wrap.is_none(),
                        crate::tools::CommandToolRole::Prefix => {
                            probe.intent.wrap.as_ref().is_some_and(|wrap| {
                                wrap.stage == crate::tools::WrapStage::Probe
                                    && wrap.tool == probe.tool
                            })
                        }
                    }
                    && !probe.tool.enabled
                    && probe.intent.limits.network == crate::tools::NetworkGrant::None
                    && probe.intent.limits.runtime_ms <= crate::tools::command_tools::TIMEOUT_MS
            }
            Self::CommandToolProbeFinished { outcome } => {
                outcome.observation.valid()
                    && !outcome.observation.intent.call.is_host()
                    && outcome.boundary_active_ms.is_none()
                    && outcome.result
                        == HookResult::for_event(HookEvent::CommandRewrite, &outcome.observation)
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(ReplayError::InvalidPayload)
        }
    }
}
