use super::*;

impl RunLoop<'_> {
    pub(super) async fn classify_wave_primary(
        &self,
        result: CallResult,
        limits: Option<crate::provider::RateLimits>,
        queue: &ChildQueue,
        abort: Option<WaveStop>,
    ) -> Result<(PrimaryDecision, Option<RepromptReason>), EngineError> {
        let deferral = RepromptReason::deferral(&result);
        let validity = self.wave_validity(&result)?;
        let facts = RunFacts::observe(self)?;
        let site = PrimarySite::Wave {
            queue_stopped: abort.is_some() || queue.stop.is_some(),
        };
        let record_admitted = primary_admitted(site, &result, validity, &facts);
        let mut record = call_record(
            AgentPhase::RootSynthesis,
            &result,
            record_admitted,
            None,
            None,
            limits,
        );
        if wave_deferred(&result, record_admitted, &facts) {
            record.failure_reason = deferral.map(RepromptReason::failure_reason);
        }
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            record,
        })
        .await?;
        let facts = RunFacts::observe(self)?;
        let decision = decide_primary_call(site, result, validity, &facts);
        Ok((decision, deferral))
    }

    pub(super) async fn apply_wave_primary(
        &self,
        wave: &mut ChildWave<'_>,
        decision: PrimaryDecision,
        deferral: Option<RepromptReason>,
        abort: &mut Option<WaveStop>,
    ) -> Result<(), EngineError> {
        match decision {
            PrimaryDecision::ToolStep(requests, turn) => {
                let process = requests.iter().any(|r| r.call.is_process());
                if process {
                    wave.queue.primary_step = Some((requests, turn));
                } else {
                    self.apply_wave_primary_tool(wave, requests, turn, abort)
                        .await?;
                }
            }
            PrimaryDecision::Defer => {
                let settled = self.settled_agents()?;
                wave.queue.deferred = deferral.map(|reason| (reason, settled));
                wave.queue.primary_pending = false;
            }
            PrimaryDecision::Stop(cause) => *abort = Some(WaveStop::Abort(cause)),
            PrimaryDecision::Plan(..)
            | PrimaryDecision::Finish(..)
            | PrimaryDecision::Delegate(..)
            | PrimaryDecision::Refused(_)
            | PrimaryDecision::RefusedTool(..)
            | PrimaryDecision::CorrectOutcome
            | PrimaryDecision::Fail(_) => {
                return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
            }
        }
        Ok(())
    }

    async fn apply_wave_primary_tool(
        &self,
        wave: &mut ChildWave<'_>,
        requests: Vec<ToolRequest>,
        turn: ProviderTurn,
        abort: &mut Option<WaveStop>,
    ) -> Result<(), EngineError> {
        if let Some(segment) = wave.segments.take() {
            self.settle_active_segment(segment)?;
        }
        // Wave validity admits a bounded request list.
        let request = self
            .begin_tool_step(requests)
            .await?
            .ok_or(EngineError::CoordinatorFailed)?;
        let end = match Box::pin(self.execute_step(
            Grant::Primary {
                request: wave.root_request,
                review_allowed: false,
            },
            request,
            turn,
        ))
        .await?
        {
            StepEnd::Continue => StepEnd::Continue,
            StepEnd::Stopped(StopCause::ActiveTime)
                if self
                    .active_time_floor
                    .load(std::sync::atomic::Ordering::Relaxed) =>
            {
                StepEnd::Stopped(StopCause::ActiveTime)
            }
            StepEnd::Stopped(cause @ (StopCause::ActiveTime | StopCause::Capacity)) => {
                wave.queue.stop.get_or_insert(cause);
                self.observe_worker_stop().await?;
                StepEnd::Continue
            }
            end @ StepEnd::Stopped(
                StopCause::Uncertain
                | StopCause::Cancelled
                | StopCause::HookStop
                | StopCause::Paused
                | StopCause::Budget
                | StopCause::ContextFull
                | StopCause::DelegationDeadline
                | StopCause::Refused(_)
                | StopCause::ToolRefused(_)
                | StopCause::ToolStopped
                | StopCause::InvalidCommandResult,
            ) => end,
        };
        wave.queue.primary_pending = false;
        if let StepEnd::Stopped(cause) = end {
            *abort = self
                .hook_facts()?
                .stop()
                .or(*abort)
                .or(Some(WaveStop::Abort(cause)));
            if matches!(cause, StopCause::ToolRefused(_)) {
                wave.queue.stop = Some(cause);
                return Ok(());
            }
        }
        if wave.queue.stop.is_none() {
            self.verify_workers().await?;
        }
        self.resume_delegation(wave)?;
        Ok(())
    }

    pub(super) async fn fill_primary_slot(
        &self,
        queue: &mut ChildQueue,
        root_request: &mut ProviderRequest,
        deadline: Instant,
    ) -> Result<Admission<SlotDispatch>, EngineError> {
        // Settled agents only grow, so a deferral ends once more agents settle than it saw.
        let deferred = match queue.deferred {
            Some((_, at)) => self.settled_agents()? <= at,
            None => false,
        };
        if !queue.primary_pending
            && !deferred
            && queue.stop.is_none()
            && self.controls.tools.is_some()
            && self.coordination_ready()?
        {
            self.refresh_team_context(root_request)?;
            if let (Some((reason, _)), Some(tools)) =
                (queue.deferred.take(), root_request.input.tools.as_mut())
            {
                let mut catalog: serde_json::Value = serde_json::from_str(tools.catalog())
                    .map_err(|_| EngineError::InvalidRequest)?;
                catalog["last_answer_refused"] = serde_json::json!(reason.text());
                tools.set_catalog(&catalog);
            }
            root_request.phase = AgentPhase::RootSynthesis;
            match self
                .maintain_primary_context(root_request, Some(deadline))
                .await?
            {
                super::super::tool_compaction::PrimaryContext::Ready => {}
                super::super::tool_compaction::PrimaryContext::Stop(cause) => {
                    queue.stop.get_or_insert(cause);
                    return Ok(Admission::Reserved(SlotDispatch::Stopped));
                }
            }
            let mut model = root_request.model.clone();
            let mut max_output_tokens = root_request.max_output_tokens;
            self.routing.bind(
                crate::provider::ProviderRole::Primary,
                self.primary_id,
                &mut model,
                &mut max_output_tokens,
            )?;
            if !self.admit_worker_call(queue).await? {
                return Ok(Admission::Reserved(SlotDispatch::Stopped));
            }
            let Admission::Reserved(()) = self
                .reserve_provider_call(self.primary_id, self.primary_call(), 0)
                .await?
            else {
                return Ok(Admission::Exhausted);
            };
            self.primary_context(root_request).await?;
            let mut request = root_request.clone();
            request.model = model;
            request.max_output_tokens = max_output_tokens;
            self.spawn_wave_primary(queue, request, deadline)?;
            queue.primary_pending = true;
            return Ok(Admission::Reserved(SlotDispatch::Started));
        }
        Ok(Admission::Reserved(SlotDispatch::Stopped))
    }

    fn spawn_wave_primary(
        &self,
        queue: &mut ChildQueue,
        request: ProviderRequest,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        let provider = self
            .routing
            .provider(crate::provider::ProviderRole::Primary, self.primary_id)?;
        self.spawn_attempt(
            queue,
            ActiveChild {
                limits: crate::provider::RateLimitSlot::default(),
                index: None,
                request,
            },
            provider,
            Some(crate::provider::ResponsePreview::new().0),
            deadline,
        );
        Ok(())
    }
}
