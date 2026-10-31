use super::decide::researcher_end;
use super::*;

impl RunLoop<'_> {
    pub(super) async fn apply_researcher_call(
        &self,
        child: &mut ChildWork,
        decision: ChildDecision,
        abort: &mut Option<WaveStop>,
        deadline: Instant,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        let agent_run_id = child.agent_run_id;
        let disposition = match decision {
            ChildDecision::Finish {
                answer,
                turn,
                stop_hook,
            } => {
                if stop_hook {
                    match self
                        .subagent_stop(child, &answer, turn, abort, deadline)
                        .await?
                    {
                        ChildStop::Proceed => {}
                        ChildStop::Reprompt => return Ok(Admission::Reserved(None)),
                        ChildStop::End { disposition, .. } => {
                            return Ok(Admission::Reserved(Some(disposition)));
                        }
                    }
                }
                self.finish_child(
                    agent_run_id,
                    AgentDisposition::Finished,
                    Some(&answer),
                    true,
                )
                .await?;
                AgentDisposition::Finished
            }
            ChildDecision::ToolStep(requests, turn) => {
                return self
                    .run_researcher_tool(child, requests, turn, deadline)
                    .await;
            }
            ChildDecision::RefusedTools(requests, turn) => {
                return self
                    .continue_refused_researcher(child, requests, turn)
                    .await;
            }
            ChildDecision::End(ChildEnd::Skipped {
                disposition,
                budget,
            }) => {
                self.ledger().child_call_finished()?;
                if budget {
                    abort.get_or_insert(WaveStop::BUDGET);
                }
                self.finish_child(agent_run_id, disposition, None, false)
                    .await?;
                disposition
            }
            ChildDecision::End(ChildEnd::Researcher(disposition)) => {
                self.finish_child(agent_run_id, disposition, None, true)
                    .await?;
                disposition
            }
            ChildDecision::Reprompt(..) | ChildDecision::End(ChildEnd::Editor(_)) => {
                return Err(EngineError::CoordinatorFailed);
            }
        };
        Ok(Admission::Reserved(Some(disposition)))
    }

    async fn continue_refused_researcher(
        &self,
        child: &mut ChildWork,
        requests: Vec<ToolRequest>,
        turn: ProviderTurn,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        self.ledger().child_call_finished()?;
        let refusal = self
            .ledger()
            .view(|view| {
                view.runs
                    .last()?
                    .agents
                    .iter()
                    .find(|agent| agent.id == child.agent_run_id)?
                    .provider_calls
                    .last()?
                    .refusal
                    .clone()
            })?
            .ok_or(EngineError::InvalidHistory)?;
        let step = ToolStep {
            narrowed_observations: Default::default(),
            refusal_index: None,
            refusal: Some(refusal),
            cleared: false,
            plan: None,
            turn,
            requests,
            observations: Vec::new(),
            facts: None,
            steering_notes: Vec::new(),
            calls_remaining: Some(child.tool_calls_remaining),
            hook_feedback: Default::default(),
        };
        let context = child.tools.as_mut().ok_or(EngineError::InvalidRequest)?;
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let capacity = super::super::tool_compaction::context_capacity(
            context.with_step_len(&step),
            runtime.context_limit_bytes(),
            0,
        );
        if matches!(
            capacity,
            super::super::tool_compaction::ContextCapacity::Fits
        ) {
            context.steps.push(step);
            return Ok(Admission::Reserved(None));
        }
        self.finish_child(child.agent_run_id, AgentDisposition::Failed, None, false)
            .await?;
        Ok(Admission::Reserved(Some(AgentDisposition::Failed)))
    }

    async fn run_researcher_tool(
        &self,
        child: &mut ChildWork,
        requests: Vec<ToolRequest>,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        let agent_run_id = child.agent_run_id;

        self.ledger().child_call_finished()?;
        let calls = requests.len();
        // Boxed: the Tool future is large and would overflow the caller's stack.
        let active_deadline = child.active_deadline.clone();
        let end = Box::pin(self.execute_step(
            Grant::Researcher {
                child: &mut *child,
                deadline,
                active_deadline,
            },
            requests,
            turn,
        ))
        .await?;
        match end {
            StepEnd::Continue => {
                child.tool_calls_remaining = child.tool_calls_remaining.saturating_sub(calls);
                return Ok(Admission::Reserved(None));
            }
            StepEnd::Stopped(StopCause::ActiveTime) => {
                return Ok(Admission::Exhausted);
            }
            StepEnd::Stopped(
                StopCause::Uncertain
                | StopCause::Cancelled
                | StopCause::HookStop
                | StopCause::Paused
                | StopCause::Budget
                | StopCause::Capacity
                | StopCause::ContextFull
                | StopCause::DelegationDeadline
                | StopCause::Refused(_)
                | StopCause::ToolRefused(_)
                | StopCause::ToolStopped
                | StopCause::InvalidCommandResult,
            ) => {}
        }
        let disposition = researcher_end(
            self.actor_hook_requested_stop(agent_run_id)?,
            self.controls.cancellation.is_cancelled() || self.hook_requested_stop()?,
        );
        self.finish_child(agent_run_id, disposition, None, false)
            .await?;
        Ok(Admission::Reserved(Some(disposition)))
    }
}
