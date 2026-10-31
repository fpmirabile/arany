use super::decide::decide_child_stop;
use super::*;

impl RunLoop<'_> {
    pub(super) fn settle_delegation<'b>(
        &'b self,
        root_request: &'b mut ProviderRequest,
        settlement: DelegationSettlement,
    ) -> super::super::step::BoxedStep<'b, Admission<DelegationProgress>> {
        Box::pin(self.settle_delegation_inner(root_request, settlement))
    }

    async fn settle_delegation_inner(
        &self,
        root_request: &mut ProviderRequest,
        settlement: DelegationSettlement,
    ) -> Result<Admission<DelegationProgress>, EngineError> {
        let DelegationSettlement {
            mut children,
            mut active_segment,
            drained,
            primary_step,
        } = settlement;
        let Drained {
            abort: mut stop,
            pending,
        } = drained;
        if let Some(segment) = active_segment.take() {
            self.settle_active_segment(segment)?;
        }
        if stop == Some(WaveStop::Boundary(StopCause::Paused)) {
            self.pause_at_boundary();
        }

        if self.controls.cancellation.is_cancelled() {
            stop = Some(WaveStop::Abort(StopCause::Cancelled));
        }
        if stop.is_none() && self.ledger().active_time_exhausted() {
            stop = Some(WaveStop::BUDGET);
        }
        if stop.is_none() && self.worker_stopped()? {
            stop = Some(WaveStop::BUDGET);
        }
        if stop.is_none() && !pending.is_empty() {
            stop = Some(WaveStop::Abort(StopCause::ToolStopped));
        }
        if let Some(stop) = stop {
            let Admission::Reserved(()) = self
                .settle_stopped_children(
                    root_request,
                    &mut children,
                    pending,
                    WorkerEnd::stopped(stop),
                )
                .await?
            else {
                return Ok(Admission::Exhausted);
            };
            return self
                .settle_primary(PrimaryEnd::Stopped(stop.cause()))
                .await
                .map(|()| Admission::Reserved(DelegationProgress::Settled));
        }
        if let Some(step) = primary_step {
            match self.settle_delegation_tool(root_request, step).await? {
                DelegationProgress::Continue => {}
                DelegationProgress::Settled => {
                    return Ok(Admission::Reserved(DelegationProgress::Settled));
                }
            }
        }
        self.begin_synthesis(root_request).await.map(|continues| {
            if continues {
                Admission::Reserved(DelegationProgress::Continue)
            } else {
                Admission::Reserved(DelegationProgress::Settled)
            }
        })
    }

    async fn settle_delegation_tool(
        &self,
        root_request: &mut ProviderRequest,
        (requests, turn): (Vec<ToolRequest>, ProviderTurn),
    ) -> Result<DelegationProgress, EngineError> {
        let Some(request) = self.begin_tool_step(requests).await? else {
            return self
                .settle_primary(PrimaryEnd::Failed(super::super::RunFailure::PlanAdmission))
                .await
                .map(|()| DelegationProgress::Settled);
        };
        let end = Box::pin(self.execute_step(
            Grant::Primary {
                request: root_request,
                review_allowed: true,
            },
            request,
            turn,
        ))
        .await?;
        match end {
            StepEnd::Continue => {}
            StepEnd::Stopped(cause) => {
                return self
                    .settle_primary(PrimaryEnd::Stopped(cause))
                    .await
                    .map(|()| DelegationProgress::Settled);
            }
        }
        self.verify_workers().await?;
        Ok(DelegationProgress::Continue)
    }

    async fn settle_stopped_children(
        &self,
        _root_request: &mut ProviderRequest,
        children: &mut [ChildWork],
        pending: VecDeque<usize>,
        end: WorkerEnd,
    ) -> Result<Admission<()>, EngineError> {
        if children.iter().any(|child| child.kind.is_editor()) {
            self.observe_worker_stop().await?;
        }
        for index in pending {
            if children[index].kind.is_editor() {
                let _active_work = self.ledger().active_work();
                let completion = self
                    .complete_worker(
                        &mut children[index],
                        WorkerEnd {
                            answer: None,
                            integration: None,
                            ..end
                        },
                    )
                    .await?;
                match completion {
                    WorkerCompletion::Settled | WorkerCompletion::Uncertain => {}
                }
            } else {
                self.finish_child(
                    children[index].agent_run_id,
                    AgentDisposition::Cancelled,
                    None,
                    false,
                )
                .await?;
            }
        }
        Ok(Admission::Reserved(()))
    }

    // A wave admits only a well-formed Tool step, and only outside live worker leases unless it
    // spawns a process, which waits for lease quiescence.
    pub(super) fn wave_validity(
        &self,
        result: &CallResult,
    ) -> Result<Result<(), super::super::ProposalRefusalCause>, EngineError> {
        let CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tools(requests),
            ..
        }) = result
        else {
            return Ok(Err(super::super::ProposalRefusalCause::Phase));
        };
        if let Err(cause) = self.well_formed_tools(requests)? {
            return Ok(Err(cause));
        }
        for request in requests {
            if !request.call.is_process()
                && let Err(cause) = self.primary_lease_allows(&request.call)?
            {
                return Ok(Err(cause));
            }
        }
        Ok(Ok(()))
    }

    pub(super) fn settled_agents(&self) -> Result<usize, EngineError> {
        self.ledger().view(|v| {
            v.runs.last().map_or(0, |r| {
                r.agents
                    .iter()
                    .skip(1)
                    .filter(|a| a.status != crate::session::AgentStatus::Active)
                    .count()
            })
        })
    }

    pub(super) fn subagent_stop<'a>(
        &'a self,
        child: &'a mut ChildWork,
        answer: &'a Answer,
        turn: ProviderTurn,
        abort: &'a mut Option<WaveStop>,
        deadline: Instant,
    ) -> super::super::step::BoxedStep<'a, ChildStop> {
        Box::pin(self.subagent_stop_inner(child, answer, turn, abort, deadline))
    }

    async fn subagent_stop_inner(
        &self,
        child: &mut ChildWork,
        answer: &Answer,
        turn: ProviderTurn,
        abort: &mut Option<WaveStop>,
        deadline: Instant,
    ) -> Result<ChildStop, EngineError> {
        let run = match self
            .run_subagent_stop(child, &answer.text, turn, deadline)
            .await
        {
            Err(EngineError::Tool(crate::tools::ToolError::Uncertain))
                if child.kind.is_editor() =>
            {
                Ok(StopHookRun::Uncertain)
            }
            run => run,
        }?;
        if run == StopHookRun::Blocked && !child.kind.is_editor() {
            self.ledger().child_call_finished()?;
        }
        let decision = decide_child_stop(run, *abort, &self.child_facts(child, deadline)?);
        if let ChildStop::End {
            disposition,
            stops_wave,
            ..
        } = decision
        {
            if stops_wave {
                abort.get_or_insert(if self.controls.cancellation.is_cancelled() {
                    WaveStop::Abort(StopCause::Cancelled)
                } else {
                    WaveStop::BUDGET
                });
            }
            if !child.kind.is_editor() {
                self.finish_child(
                    child.agent_run_id,
                    disposition,
                    None,
                    run != StopHookRun::Blocked,
                )
                .await?;
            }
        }
        Ok(decision)
    }

    pub(super) async fn run_subagent_stop(
        &self,
        child: &mut ChildWork,
        message: &str,
        turn: ProviderTurn,
        deadline: Instant,
    ) -> Result<StopHookRun, EngineError> {
        let subject = child.hook_subject().to_owned();
        let fits = self
            .controls
            .tools
            .zip(child.tools.as_ref())
            .is_none_or(|(runtime, tools)| {
                tools.has_stop_room(&turn, runtime.context_limit_bytes())
            });
        let result = self
            .lifecycle_hooks(LifecycleHook {
                agent: child.agent_run_id,
                event: crate::tools::permissions::HookEvent::SubagentStop,
                subject: &subject,
                message,
                deadline: Some(deadline),
                feedback_fits: fits,
            })
            .await?;
        if self.controls.cancellation.is_cancelled() {
            return Ok(StopHookRun::Cancelled);
        }
        let feedback = match result {
            Ok(Some(feedback)) => feedback,
            Ok(None) => return Ok(StopHookRun::Passed),
            Err(HookRefusal::ActiveTime) => {
                return Ok(StopHookRun::CutOffByActiveTime);
            }
            Err(HookRefusal::Cancelled) => return Ok(StopHookRun::Cancelled),
            Err(HookRefusal::ContextFull) => return Ok(StopHookRun::CutOffByContext),
        };
        if let Some(tools) = child.tools.as_mut() {
            tools.add_stop_feedback(&turn, &feedback);
        }
        child
            .stop_hook_feedback
            .push(crate::provider::StopHookFeedback {
                after_step: child.tools.as_ref().map_or(0, |tools| tools.steps.len()),
                turn,
                text: feedback,
                facts: None,
                steering_notes: Vec::new(),
            });
        Ok(StopHookRun::Blocked)
    }

    pub(super) async fn finish_child(
        &self,
        agent_run_id: AgentRunId,
        disposition: AgentDisposition,
        answer: Option<&Answer>,
        call_reserved: bool,
    ) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            worker: None,
            run_id: self.run_id,
            agent_run_id,
            disposition,
            summary: answer.map(|value| crate::session::answer_summary(&value.text)),
            result: answer.map(|value| value.text.clone()),
        })
        .await?;
        if call_reserved {
            self.ledger().child_call_finished()?;
        }
        Ok(())
    }
}
