use super::decide::{decide_child_call, wave_stop_after};
use super::*;

impl RunLoop<'_> {
    pub(super) fn drive_children<'a>(
        &'a self,
        children: &'a mut Vec<ChildWork>,
        root_request: &'a mut ProviderRequest,
        queue: &'a mut ChildQueue,
        segments: &'a mut Option<ActiveSegment>,
        deadline: Instant,
    ) -> super::super::step::BoxedStep<'a, RunProgress<Drained>> {
        Box::pin(self.drive_children_inner(ChildWave {
            children,
            root_request,
            queue,
            segments,
            deadline,
        }))
    }

    // A same-route retry waits out its committed delay inside the task, so cancellation still drains it.
    fn respawn_attempt(
        &self,
        queue: &mut ChildQueue,
        task_id: Id,
        index: Option<usize>,
        delay: Duration,
        deadline: Instant,
    ) -> Result<(), EngineError> {
        let role = if index.is_none() {
            crate::provider::ProviderRole::Primary
        } else {
            crate::provider::ProviderRole::Child
        };
        let mut active = queue
            .active
            .remove(&task_id)
            .ok_or(EngineError::CoordinatorFailed)?;
        let provider = self.routing.bind(
            role,
            active.request.agent_run_id,
            &mut active.request.model,
            &mut active.request.max_output_tokens,
        )?;
        let preview = (index.is_none()
            || self.routing.chain(role, active.request.agent_run_id)?.len() > 1)
            .then(|| crate::provider::ResponsePreview::new().0);
        let request = active.request.clone();
        let limits = active.limits.clone();
        let mut cancellation = self.controls.cancellation.clone();
        let steering = self.controls.steering.clone();
        let session_id = self.session_id;
        let provider_activity = self.ledger().provider_activity();
        let handle = queue.calls.spawn(async move {
            let _provider_work = provider_activity.start();
            // A pause or cancellation in the wait ends the child's call unsent; the wave then drains.
            if !delay.is_zero()
                && crate::engine::routing::back_off(
                    session_id,
                    delay,
                    &mut cancellation,
                    steering.as_ref(),
                )
                .await
                    != crate::engine::routing::Backoff::Elapsed
            {
                return crate::engine::routing::AttemptResult {
                    result: CallResult::Cancelled,
                    transient: None,
                    visible: false,
                };
            }
            crate::engine::routing::invoke_attempt(
                provider.as_ref(),
                request,
                deadline,
                cancellation,
                preview,
                limits,
                crate::fault::LatencyReceipt::default(),
            )
            .await
        });
        queue.active.insert(handle.id(), active);
        Ok(())
    }

    async fn drive_children_inner(
        &self,
        mut wave: ChildWave<'_>,
    ) -> Result<RunProgress<Drained>, EngineError> {
        if let Some(cause) = self.refill_or_abort(&mut wave).await? {
            return Ok(RunProgress::Stopped(cause));
        }
        let mut abort = self.unstarted_stop(wave.queue);
        let mut drain_deadline = None;

        while !wave.queue.active.is_empty() {
            let (index, active, result) = match self
                .next_child_attempt(&mut wave, &mut abort, &mut drain_deadline)
                .await?
            {
                ControlFlow::Continue(()) => continue,
                ControlFlow::Break(completed) => {
                    let CompletedChildAttempt {
                        index,
                        active,
                        result,
                    } = completed;
                    (index, active, result)
                }
            };
            let limits = active.limits.take();
            let result = result.result;
            self.settle_result(&result)?;
            if index.is_none() {
                let (decision, deferral) = self
                    .classify_wave_primary(result, limits, wave.queue, abort)
                    .await?;
                self.apply_wave_primary(&mut wave, decision, deferral, &mut abort)
                    .await?;
                if abort.is_some() {
                    drain_deadline = Some(wave.queue.begin_drain());
                } else if let RunProgress::Stopped(cause) = self.refill_wave(&mut wave).await? {
                    return Ok(RunProgress::Stopped(cause));
                }
                continue;
            }
            let index = index.ok_or(EngineError::CoordinatorFailed)?;
            let step_disposition = match self
                .settle_wave_child(&mut wave, index, result, limits, &mut abort)
                .await?
            {
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
                RunProgress::Ready(ChildProgress::Refilled) => {
                    if abort.is_none() {
                        abort = self.unstarted_stop(wave.queue);
                    }
                    continue;
                }
                RunProgress::Ready(ChildProgress::Settled(disposition)) => disposition,
            };
            if abort.is_none() {
                abort = wave_stop_after(
                    wave.queue.stop,
                    self.controls.cancellation.is_cancelled(),
                    self.pause_requested(),
                    step_disposition,
                    self.hook_facts()?,
                );
            }
            if abort.is_none() {
                if let Some(cause) = self.refill_or_abort(&mut wave).await? {
                    return Ok(RunProgress::Stopped(cause));
                }
                abort = self.unstarted_stop(wave.queue);
            }
        }
        Ok(RunProgress::Ready(Drained {
            abort,
            pending: std::mem::take(&mut wave.queue.pending),
        }))
    }

    // A refill that stops or fails tears down the attempts already running.
    async fn refill_or_abort(
        &self,
        wave: &mut ChildWave<'_>,
    ) -> Result<Option<RunRefusal>, EngineError> {
        match self.refill_wave(wave).await {
            Ok(RunProgress::Ready(())) => Ok(None),
            Ok(RunProgress::Stopped(cause)) => Ok(Some(wave.queue.abort_with(cause).await)),
            Err(error) => Err(wave.queue.abort_with(error).await),
        }
    }

    async fn next_child_attempt(
        &self,
        wave: &mut ChildWave<'_>,
        abort: &mut Option<WaveStop>,
        drain_deadline: &mut Option<Instant>,
    ) -> Result<ChildAttempt, EngineError> {
        let joined = self
            .join_child_call(wave.queue, abort, drain_deadline)
            .await?;
        let (task_id, result) = classify_join(joined);
        let Some(active) = wave.queue.active.get(&task_id) else {
            return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
        };
        let index = active.index;
        let respawn = self
            .child_attempt_continuation(active, &result, *abort, wave.deadline())
            .await;
        match respawn {
            Ok(Some(delay)) => {
                if let Err(error) =
                    self.respawn_attempt(wave.queue, task_id, index, delay, wave.deadline())
                {
                    return Err(wave.queue.abort_with(error).await);
                }
                return Ok(ControlFlow::Continue(()));
            }
            Ok(None) => {}
            Err(error) => return Err(wave.queue.abort_with(error).await),
        }
        let Some(active) = wave.queue.active.remove(&task_id) else {
            return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
        };
        Ok(ControlFlow::Break(CompletedChildAttempt {
            index,
            active,
            result,
        }))
    }

    async fn child_attempt_continuation(
        &self,
        active: &ActiveChild,
        result: &crate::engine::routing::AttemptResult,
        abort: Option<WaveStop>,
        deadline: Instant,
    ) -> Result<Option<Duration>, EngineError> {
        let role = if active.index.is_none() {
            crate::provider::ProviderRole::Primary
        } else {
            crate::provider::ProviderRole::Child
        };
        let facts = self.routing.attempt_facts(
            active.request.agent_run_id,
            role,
            deadline,
            abort.is_some() || self.controls.cancellation.is_cancelled() || self.pause_requested(),
            result.visible,
        )?;
        let scope = CallScope {
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id: active.request.agent_run_id,
        };
        self.routing
            .continue_attempt(
                scope,
                role,
                decide_attempt(result.transient, facts),
                &active.request,
                &active.limits,
                || self.routing.provider(role, active.request.agent_run_id),
            )
            .await
    }

    pub(super) fn refill_wave<'a>(
        &'a self,
        wave: &'a mut ChildWave<'_>,
    ) -> super::super::step::BoxedStep<'a, RunProgress<()>> {
        Box::pin(self.fill_slots(wave))
    }

    async fn join_child_call(
        &self,
        queue: &mut ChildQueue,
        abort: &mut Option<WaveStop>,
        drain_deadline: &mut Option<Instant>,
    ) -> Result<Result<(Id, crate::engine::routing::AttemptResult), JoinError>, EngineError> {
        if let Some(cause) = queue.stop
            && abort.is_none()
        {
            *abort = Some(WaveStop::from_queue(cause));
        }
        // A pause lets in-flight child calls finish and be recorded; every other stop drains them.
        if abort.is_some_and(|stop| stop != WaveStop::Boundary(StopCause::Paused))
            && drain_deadline.is_none()
        {
            *drain_deadline = Some(queue.begin_drain());
        }
        let joined = match *drain_deadline {
            Some(limit) => tokio::time::timeout_at(limit, queue.calls.join_next_with_id())
                .await
                .map_err(|_| EngineError::CancellationDrainTimeout)?,
            None => queue.calls.join_next_with_id().await,
        }
        .ok_or(EngineError::CoordinatorFailed)?;
        Ok(joined)
    }

    async fn settle_wave_child(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        result: CallResult,
        limits: Option<crate::provider::RateLimits>,
        abort: &mut Option<WaveStop>,
    ) -> Result<RunProgress<ChildProgress>, EngineError> {
        let valid_response = self.ledger().tokens_within_limit()
            && match &result {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Answer(answer),
                    ..
                }) => valid_answer(answer, MAX_CHILD_RESULT_BYTES),
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(requests),
                    ..
                }) => self.valid_child_tools(
                    &wave.children[index],
                    requests,
                    self.ledger().plan_contract()?.as_ref(),
                    self.ledger().resource_settings()?,
                ),
                CallResult::Response(ProviderResponse {
                    outcome:
                        ProviderOutcome::Delegate(_)
                        | ProviderOutcome::Plan(_)
                        | ProviderOutcome::Review(_),
                    ..
                })
                | CallResult::Failed(_)
                | CallResult::Cancelled => false,
            };
        if wave.children[index].kind.is_editor() {
            let decision = self
                .classify_editor_call(
                    &wave.children[index],
                    result,
                    limits,
                    valid_response,
                    *abort,
                    wave.deadline(),
                )
                .await?;
            return self.apply_editor_call(wave, index, decision, abort).await;
        }
        let deadline = wave.deadline();
        let admission = match self
            .settle_child_call(
                &mut wave.children[index],
                ChildCall {
                    result,
                    limits,
                    valid_response,
                },
                abort,
                deadline,
            )
            .await
        {
            Ok(admission) => admission,
            Err(error) => return Err(wave.queue.abort_with(error).await),
        };
        if matches!(admission, Admission::Exhausted) {
            wave.queue.abort_and_drain().await;
        }
        Ok(RunProgress::from(admission).map(|disposition| {
            if disposition.is_none() {
                wave.queue.pending.push_back(index);
            }
            ChildProgress::Settled(disposition)
        }))
    }

    async fn settle_child_call(
        &self,
        child: &mut ChildWork,
        call: ChildCall,
        abort: &mut Option<WaveStop>,
        deadline: Instant,
    ) -> Result<Admission<Option<AgentDisposition>>, EngineError> {
        let ChildCall {
            result,
            limits,
            valid_response,
        } = call;
        let agent_run_id = child.agent_run_id;
        let refusal = match &result {
            CallResult::Response(ProviderResponse {
                outcome: ProviderOutcome::Tools(requests),
                ..
            }) if child.tools.is_some()
                && requests.len() > usize::from(crate::session::MAX_STEP_TOOL_CALLS) =>
            {
                super::super::proposal_refusal(
                    &result,
                    crate::session::ProposalRefusalCause::InvalidArguments,
                )
            }
            _ => None,
        };
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id,
            record: call_record(
                AgentPhase::ChildWork,
                &result,
                valid_response,
                None,
                refusal,
                limits,
            ),
        })
        .await?;
        let facts = self.child_facts(child, deadline)?;
        let decision = decide_child_call(result, valid_response, None, *abort, &facts);
        self.apply_researcher_call(child, decision, abort, deadline)
            .await
    }

    // Honor pause before reporting an exhausted queued-child call budget.
    fn unstarted_stop(&self, queue: &ChildQueue) -> Option<WaveStop> {
        if self.pause_requested() {
            Some(WaveStop::Boundary(StopCause::Paused))
        } else if let Some(cause) = queue.stop {
            Some(WaveStop::from_queue(cause))
        } else if queue.pending.is_empty() && queue.unspawned.is_empty() {
            None
        } else if !self.ledger().calls_open() {
            Some(WaveStop::BUDGET)
        } else {
            None
        }
    }

    async fn fill_slots(&self, wave: &mut ChildWave<'_>) -> Result<RunProgress<()>, EngineError> {
        if let Some(deadline) = &wave.queue.active_deadline
            && deadline.reached()
        {
            wave.queue.stop = Some(match deadline.cause {
                ActiveDeadlineCause::Goal => StopCause::ActiveTime,
                ActiveDeadlineCause::Delegation => StopCause::DelegationDeadline,
            });
        }
        while wave.queue.active.len() < self.ledger().provider_concurrency(wave.queue.parallel)
            && !self.controls.cancellation.is_cancelled()
            && !self.pause_requested()
            && self.ledger().calls_open()
            && wave.queue.stop.is_none()
            && !self.worker_stopped()?
        {
            if wave.queue.pending.is_empty() {
                match self
                    .fill_primary_slot(wave.queue, wave.root_request, wave.deadline())
                    .await?
                {
                    Admission::Reserved(SlotDispatch::Started) => continue,
                    Admission::Reserved(SlotDispatch::Stopped) => {}
                    Admission::Exhausted => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
                }
            }
            if wave.queue.stop.is_some() {
                break;
            }
            let index = match self.next_child_slot(wave).await? {
                RunProgress::Ready(ChildSlot::Ready(index)) => index,
                RunProgress::Ready(ChildSlot::Stopped) => break,
                RunProgress::Ready(ChildSlot::Retry) => continue,
                RunProgress::Stopped(cause) => return Ok(RunProgress::Stopped(cause)),
            };
            match self.fill_child_slot(wave, index).await? {
                Admission::Reserved(SlotDispatch::Started) => {}
                Admission::Reserved(SlotDispatch::Stopped) => break,
                Admission::Exhausted => return Ok(RunProgress::Stopped(RunRefusal::Budget)),
            }
        }
        Ok(RunProgress::Ready(()))
    }

    async fn next_child_slot(
        &self,
        wave: &mut ChildWave<'_>,
    ) -> Result<RunProgress<ChildSlot>, EngineError> {
        let index = if let Some(index) = wave.queue.pending.pop_front() {
            index
        } else {
            let child = match self
                .spawn_next_child(
                    wave.queue,
                    wave.root_request,
                    wave.segments,
                    wave.deadline(),
                )
                .await?
            {
                RunProgress::Ready(Some(child)) => child,
                RunProgress::Ready(None) => {
                    return Ok(RunProgress::Ready(if wave.queue.unspawned.is_empty() {
                        ChildSlot::Stopped
                    } else {
                        ChildSlot::Retry
                    }));
                }
                RunProgress::Stopped(RunRefusal::Capacity) => {
                    return Ok(RunProgress::Stopped(RunRefusal::Capacity));
                }
                RunProgress::Stopped(RunRefusal::Budget) => {
                    wave.queue.stop.get_or_insert(StopCause::ActiveTime);
                    return Ok(RunProgress::Ready(ChildSlot::Stopped));
                }
            };
            let index = wave.children.len();
            wave.children.push(child);
            index
        };
        Ok(RunProgress::Ready(ChildSlot::Ready(index)))
    }

    async fn fill_child_slot(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
    ) -> Result<Admission<SlotDispatch>, EngineError> {
        let child = &mut wave.children[index];
        let researcher = child.tools.is_some();
        let mut request = self.child_request(child, wave.root_request)?;
        let provider = self.routing.bind(
            crate::provider::ProviderRole::Child,
            child.agent_run_id,
            &mut request.model,
            &mut request.max_output_tokens,
        )?;
        if child.kind.is_editor() && !self.admit_worker_call(wave.queue).await? {
            wave.queue.pending.push_front(index);
            return Ok(Admission::Reserved(SlotDispatch::Stopped));
        }
        let Admission::Reserved(()) = self
            .reserve_provider_call(
                child.agent_run_id,
                Action::Child {
                    in_flight: self.ledger().in_flight_children(),
                    research: researcher,
                },
                0,
            )
            .await?
        else {
            return Ok(Admission::Exhausted);
        };
        if self.controls.cancellation.is_cancelled()
            || self.pause_requested()
            || self.worker_stopped()?
            || self.hook_requested_stop()?
            || wave.queue.stop.is_some()
        {
            let result = CallResult::Cancelled;
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: child.agent_run_id,
                record: call_record(AgentPhase::ChildWork, &result, false, None, None, None),
            })
            .await?;
            self.settle_result(&result)?;
            self.ledger().child_call_finished()?;
            wave.queue.pending.push_front(index);
            return Ok(Admission::Reserved(SlotDispatch::Stopped));
        }
        self.child_steering_note(child, &mut request).await?;
        self.spawn_child_attempt(wave.queue, index, request, provider, wave.deadline())?;
        Ok(Admission::Reserved(SlotDispatch::Started))
    }
}

fn classify_join(
    joined: Result<(Id, crate::engine::routing::AttemptResult), JoinError>,
) -> (Id, crate::engine::routing::AttemptResult) {
    match joined {
        Ok((task_id, result)) => (task_id, result),
        Err(error) => (
            error.id(),
            crate::engine::routing::AttemptResult {
                result: if error.is_cancelled() {
                    CallResult::Cancelled
                } else {
                    CallResult::Failed(CallFailure::TaskPanic)
                },
                transient: None,
                visible: false,
            },
        ),
    }
}
