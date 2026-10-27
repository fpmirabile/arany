use super::{
    Admission, ChildWave, EngineError, Grant, ProviderTurn, RunLoop, StepEnd, StopCause,
    ToolRequest,
};
use tokio::time::Instant;

impl RunLoop<'_> {
    pub(super) fn resume_delegation(&self, wave: &mut ChildWave<'_>) -> Result<(), EngineError> {
        *wave.segments = match self
            .reserve_active_segment(
                wave.deadline().saturating_duration_since(Instant::now()),
                crate::session::BudgetAction::Delegation,
                self.primary_id,
            )?
            .map(Some)
        {
            Admission::Reserved(segment) => segment,
            Admission::Exhausted => {
                wave.queue.stop.get_or_insert(StopCause::ActiveTime);
                None
            }
        };
        Ok(())
    }

    pub(super) async fn run_editor_tool(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        requests: Vec<ToolRequest>,
        turn: ProviderTurn,
    ) -> Result<(Option<StopCause>, bool), EngineError> {
        if let Some(segment) = wave.segments.take() {
            self.settle_active_segment(segment)?;
        }
        let deadline = wave.deadline();
        let child = &mut wave.children[index];
        let active_deadline = child.active_deadline.clone();
        let step_stop = match Box::pin(self.execute_step(
            Grant::Researcher {
                child,
                deadline,
                active_deadline,
            },
            requests,
            turn,
        ))
        .await?
        {
            StepEnd::Continue => None,
            StepEnd::Stopped(
                cause @ (StopCause::ActiveTime
                | StopCause::DelegationDeadline
                | StopCause::Uncertain),
            ) => {
                wave.queue.stop.get_or_insert(cause);
                if cause == StopCause::ActiveTime {
                    self.observe_worker_stop().await?;
                }
                Some(cause)
            }
            StepEnd::Stopped(cause) => Some(cause),
        };
        child.tool_calls_remaining = child.tool_calls_remaining.saturating_sub(1);
        let can_continue = self.ledger().view(|v| {
            v.runs
                .last()
                .and_then(|r| r.agents.iter().find(|a| a.id == child.agent_run_id))
                .and_then(|a| a.tools.last())
                .is_some_and(|t| t.observation.is_some() && !crate::session::editor_tool_failed(t))
        })?;
        self.resume_delegation(wave)?;
        Ok((step_stop, can_continue))
    }
}
