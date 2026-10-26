use super::*;
use crate::engine::ledger::{ActiveDeadline, ActiveReservationLimit};
use tokio::task::JoinSet;

struct ReadDeadline {
    wall: Instant,
    active: Option<ActiveDeadline>,
    cause: Option<ActiveDeadlineCause>,
}

impl ReadDeadline {
    async fn elapsed(&self) {
        match &self.active {
            Some(deadline) => deadline.elapsed().await,
            None => tokio::time::sleep_until(self.wall).await,
        }
    }
}

enum ReadPhase {
    Admitting,
    Collecting,
}

struct ReadBatch<'a> {
    calls: JoinSet<(usize, Answer)>,
    cancellation: RunCancellation,
    order: &'a mut Vec<usize>,
    end: StepEnd,
    phase: ReadPhase,
    post_hooks: Vec<usize>,
}

impl RunLoop<'_> {
    pub(super) async fn execute_reads(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &mut ToolStep,
        reads: Vec<(usize, EffectIntent)>,
        order: &mut Vec<usize>,
    ) -> Result<StepEnd, EngineError> {
        let mut batch = ReadBatch {
            calls: JoinSet::new(),
            cancellation: RunCancellation::new(),
            order,
            end: StepEnd::Continue,
            phase: ReadPhase::Collecting,
            post_hooks: Vec::new(),
        };
        let result = async {
            for (index, intent) in reads {
                while let Some(result) = batch.calls.try_join_next() {
                    let (index, answer) = result.map_err(|_| EngineError::CoordinatorFailed)?;
                    self.settle_read(grant, step, (index, answer, None), &mut batch)
                        .await?;
                }
                if self.controls.cancellation.is_cancelled() {
                    batch.end = batch.end.merge(StepEnd::Stopped(StopCause::Cancelled));
                }
                match batch.end {
                    StepEnd::Stopped(_) => {
                        step.observations
                            .push(self.finish_undispatched(grant, runtime, intent).await?);
                        batch.order.push(index);
                    }
                    StepEnd::Continue => {
                        self.start_read(grant, runtime, step, (index, intent), &mut batch)
                            .await?
                    }
                }
            }
            while let Some(result) = batch.calls.join_next().await {
                let (index, answer) = result.map_err(|_| EngineError::CoordinatorFailed)?;
                self.settle_read(grant, step, (index, answer, None), &mut batch)
                    .await?;
            }
            self.finish_read_hooks(grant, runtime, step, &mut batch)
                .await?;
            Ok(batch.end)
        }
        .await;
        if result.is_err() {
            batch.cancellation.cancel();
            while batch.calls.join_next().await.is_some() {}
        }
        result
    }

    async fn start_read(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &mut ToolStep,
        (index, mut intent): (usize, EffectIntent),
        batch: &mut ReadBatch<'_>,
    ) -> Result<(), EngineError> {
        self.reserve_step_call(grant, runtime, step, &intent)
            .await?;
        let source = intent.call.clone();
        loop {
            let verdict = self
                .read_admission(grant, runtime, step, &intent, batch)
                .await?;
            while let Some(result) = batch.calls.try_join_next() {
                let (index, answer) = result.map_err(|_| EngineError::CoordinatorFailed)?;
                self.settle_read(grant, step, (index, answer, None), batch)
                    .await?;
            }
            if matches!(batch.end, StepEnd::Stopped(_)) {
                return self
                    .stop_read(grant, runtime, step, (index, intent), verdict, batch)
                    .await;
            }
            match verdict {
                Admission::Exhausted => {
                    step.observations
                        .push(self.finish_undispatched(grant, runtime, intent).await?);
                    batch.order.push(index);
                    batch.end = StepEnd::Stopped(StopCause::ActiveTime);
                }
                Admission::Reserved(Verdict::Dispatch {
                    intent,
                    queue_pause,
                }) => {
                    if let Some(pause) = queue_pause {
                        self.settle_active_pause(pause, None)?;
                    }
                    self.spawn_read(grant, runtime, step, (index, intent), batch)
                        .await?;
                }
                Admission::Reserved(Verdict::Deny(answer)) => {
                    self.settle_read(grant, step, (index, answer, None), batch)
                        .await?;
                }
                Admission::Reserved(Verdict::Narrow(answer, call)) => {
                    self.settle_read(grant, step, (index, answer, Some(call.clone())), batch)
                        .await?;
                    if matches!(batch.end, StepEnd::Continue) {
                        match self
                            .start_read_replacement(grant, runtime, step, call, &source)
                            .await?
                        {
                            Ok(replacement) => {
                                intent = replacement;
                                continue;
                            }
                            Err(cause) => batch.end = StepEnd::Stopped(cause),
                        }
                    }
                }
            }
            return Ok(());
        }
    }

    async fn start_read_replacement(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &ToolStep,
        call: ToolCall,
        source: &ToolCall,
    ) -> Result<Result<EffectIntent, StopCause>, EngineError> {
        let intent = match self
            .prepare_step_intent(grant, runtime, step, call, source)
            .await?
        {
            Ok(intent) => intent,
            Err(cause) => return Ok(Err(cause)),
        };
        let narrowable = matches!(grant, Grant::Primary { .. })
            && step.narrowed_observations.len() < MAX_NARROWINGS_PER_STEP;
        Ok(self
            .start_intent(grant, runtime, &intent, source, narrowable)
            .await?
            .map(|()| intent)
            .map_err(StopCause::from))
    }

    async fn stop_read(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &mut ToolStep,
        (index, intent): (usize, EffectIntent),
        verdict: Admission<Verdict>,
        batch: &mut ReadBatch<'_>,
    ) -> Result<(), EngineError> {
        let Admission::Reserved(verdict) = verdict else {
            step.observations
                .push(self.finish_undispatched(grant, runtime, intent).await?);
            batch.order.push(index);
            return Ok(());
        };
        match verdict {
            Verdict::Dispatch {
                queue_pause: Some(pause),
                ..
            } => self.settle_active_pause(pause, None)?,
            Verdict::Deny(answer) | Verdict::Narrow(answer, _) => {
                if let Some(cause) = answer.stop {
                    batch.end = batch.end.merge(StepEnd::Stopped(cause));
                }
            }
            Verdict::Dispatch {
                queue_pause: None, ..
            } => {}
        }
        let answer = Answer {
            checkpoint: None,
            observation: ToolObservation {
                intent,
                disposition: ToolDisposition::Denied,
                output: crate::tools::types::UNDISPATCHED_STEP_OUTPUT.into(),
                attestation: None,
                cache_update: CacheUpdate::NotApplicable,
                approval_cause: None,
                limit_cause: None,
                enforcement_cause: None,
            },
            stop: None,
            active_time: None,
        };
        self.settle_read(grant, step, (index, answer, None), batch)
            .await
    }

    async fn read_admission(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &mut ToolStep,
        intent: &EffectIntent,
        batch: &mut ReadBatch<'_>,
    ) -> Result<Admission<Verdict>, EngineError> {
        let interrupted = RunCancellation::new();
        let permission = self.permission(grant, runtime, &intent.call);
        let narrowable = matches!(grant, Grant::Primary { .. })
            && step.narrowed_observations.len() < MAX_NARROWINGS_PER_STEP;
        let admission = self.before_dispatch(
            grant,
            runtime,
            intent.clone(),
            permission,
            narrowable,
            Some(&interrupted),
        );
        tokio::pin!(admission);
        let mut cancellation = self.controls.cancellation.clone();
        batch.phase = ReadPhase::Admitting;
        let mut completed = false;
        let result = async {
            loop {
            tokio::select! {
                biased;
                result = batch.calls.join_next(), if !batch.calls.is_empty() => {
                    let (index, answer) = result.ok_or(EngineError::CoordinatorFailed)?.map_err(|_| EngineError::CoordinatorFailed)?;
                    if matches!(read_end(&answer, self.controls.cancellation.is_cancelled()), StepEnd::Stopped(_)) {
                        interrupted.cancel();
                    }
                    let verdict = {
                        // Admission may hold the journal writer until its commit and verdict settle.
                        let settling = self.settle_read(grant, step, (index, answer, None), batch);
                        tokio::pin!(settling);
                        tokio::select! {
                            biased;
                            verdict = &mut admission => {
                                completed = true;
                                settling.await?;
                                Some(verdict)
                            }
                            settled = &mut settling => {
                                settled?;
                                None
                            }
                        }
                    };
                    if matches!(batch.end, StepEnd::Stopped(_)) {
                        interrupted.cancel();
                    }
                    if let Some(verdict) = verdict {
                        break verdict;
                    }
                }
                () = cancellation.cancelled(), if !interrupted.is_cancelled() => {
                    batch.end = batch.end.merge(StepEnd::Stopped(StopCause::Cancelled));
                    interrupted.cancel();
                }
                verdict = &mut admission => { completed = true; break verdict },
            }
            }
        }.await;
        if result.is_err() && !completed {
            interrupted.cancel();
            batch.cancellation.cancel();
            let _ = admission.await;
        }
        batch.phase = ReadPhase::Collecting;
        result
    }

    async fn spawn_read(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &mut ToolStep,
        (index, intent): (usize, EffectIntent),
        batch: &mut ReadBatch<'_>,
    ) -> Result<(), EngineError> {
        match self.read_deadline(grant, runtime, &intent)? {
            Admission::Reserved(deadline) => {
                let runtime = runtime.clone();
                let parent = self.controls.cancellation.clone();
                let cancellation = batch.cancellation.clone();
                let active = self.ledger().active_work();
                batch.calls.spawn(async move {
                    let _active = active;
                    (
                        index,
                        execute(runtime, intent, parent, cancellation, deadline).await,
                    )
                });
            }
            Admission::Exhausted => {
                let answer = Answer {
                    checkpoint: None,
                    observation: not_dispatched(intent, &NotDispatched::ActiveTime),
                    stop: Some(StopCause::ActiveTime),
                    active_time: None,
                };
                self.settle_read(grant, step, (index, answer, None), batch)
                    .await?;
            }
        }
        Ok(())
    }

    fn read_deadline(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: &EffectIntent,
    ) -> Result<Admission<ReadDeadline>, EngineError> {
        match grant {
            Grant::Primary { .. } => {
                let segment = match self.reserve_active_segment(
                    tool_action_bound(runtime, intent),
                    crate::session::BudgetAction::Tool,
                    intent.agent_run_id,
                )? {
                    Admission::Reserved(segment) => segment,
                    Admission::Exhausted => return Ok(Admission::Exhausted),
                };
                let deadline = ReadDeadline {
                    wall: segment.deadline(),
                    active: None,
                    cause: match segment.limit {
                        ActiveReservationLimit::Goal => Some(ActiveDeadlineCause::Goal),
                        ActiveReservationLimit::ActionBound => None,
                    },
                };
                // Carry elapsed time forward: overlapping reads share wall time, not ledger reservations.
                self.settle_active_segment(segment)?;
                Ok(Admission::Reserved(deadline))
            }
            Grant::Researcher {
                deadline,
                active_deadline,
                ..
            } => Ok(Admission::Reserved(ReadDeadline {
                wall: *deadline,
                active: active_deadline.clone(),
                cause: Some(
                    active_deadline
                        .as_ref()
                        .map_or(ActiveDeadlineCause::Delegation, |deadline| deadline.cause),
                ),
            })),
        }
    }

    async fn settle_read(
        &self,
        grant: &Grant<'_>,
        step: &mut ToolStep,
        (index, answer, narrowed): (usize, Answer, Option<ToolCall>),
        batch: &mut ReadBatch<'_>,
    ) -> Result<(), EngineError> {
        self.finish_answer(&answer).await?;
        batch.end = batch
            .end
            .merge(read_end(&answer, self.controls.cancellation.is_cancelled()));
        let observation = step.observations.len();
        if narrowed.is_some() {
            step.narrowed_observations.insert(observation);
        }
        if answer.observation.disposition == ToolDisposition::Succeeded {
            batch.post_hooks.push(observation);
        }
        step.observations.push(answer.observation);
        batch.order.push(index);
        let exhausted = match batch.phase {
            ReadPhase::Admitting => self.ledger().spent_active_time_exhausted(),
            ReadPhase::Collecting => self.ledger().active_time_exhausted(),
        };
        if matches!(grant, Grant::Primary { .. }) && exhausted {
            batch.end = batch.end.merge(StepEnd::Stopped(StopCause::ActiveTime));
        }
        Ok(())
    }

    async fn finish_read_hooks(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        step: &mut ToolStep,
        batch: &mut ReadBatch<'_>,
    ) -> Result<(), EngineError> {
        for observation in std::mem::take(&mut batch.post_hooks) {
            match post_hook_progress(
                batch.end,
                self.controls.cancellation.is_cancelled(),
                self.pause_requested(),
            ) {
                StepEnd::Continue => {}
                stopped @ StepEnd::Stopped(_) => {
                    batch.end = batch.end.merge(stopped);
                    break;
                }
            }
            let end = self
                .post_step_call(grant, runtime, step, observation)
                .await?;
            batch.end = batch.end.merge(end);
        }
        Ok(())
    }
}

fn read_end(answer: &Answer, cancelled: bool) -> StepEnd {
    let observed = (answer.observation.stops_run() || cancelled)
        .then(|| RunLoop::observation_stop(&answer.observation, cancelled));
    decide_read_end(
        answer.stop,
        answer.observation.disposition,
        answer
            .active_time
            .as_ref()
            .is_some_and(|time| time.exhausted),
        observed,
    )
}

fn decide_read_end(
    stop: Option<StopCause>,
    disposition: ToolDisposition,
    exhausted: bool,
    observed: Option<StopCause>,
) -> StepEnd {
    [
        stop,
        (disposition == ToolDisposition::Limit).then_some(StopCause::ToolStopped),
        exhausted.then_some(StopCause::ActiveTime),
        observed,
    ]
    .into_iter()
    .flatten()
    .fold(StepEnd::Continue, |end, cause| {
        end.merge(StepEnd::Stopped(cause))
    })
}

fn post_hook_progress(end: StepEnd, cancelled: bool, paused: bool) -> StepEnd {
    let end = if cancelled {
        end.merge(StepEnd::Stopped(StopCause::Cancelled))
    } else if paused {
        end.merge(StepEnd::Stopped(StopCause::Paused))
    } else {
        end
    };
    match end {
        StepEnd::Continue | StepEnd::Stopped(StopCause::ToolStopped) => StepEnd::Continue,
        StepEnd::Stopped(
            StopCause::Uncertain
            | StopCause::Cancelled
            | StopCause::HookStop
            | StopCause::Paused
            | StopCause::ActiveTime
            | StopCause::Budget
            | StopCause::Capacity
            | StopCause::ContextFull
            | StopCause::DelegationDeadline
            | StopCause::Refused(_)
            | StopCause::InvalidCommandResult,
        ) => end,
    }
}

async fn execute(
    runtime: ToolRuntime,
    intent: EffectIntent,
    mut parent: RunCancellation,
    mut batch: RunCancellation,
    deadline: ReadDeadline,
) -> Answer {
    let cancellation = RunCancellation::new();
    let execution = runtime.execute_concurrent_read(intent, cancellation.clone());
    tokio::pin!(execution);
    let mut timed_out = false;
    let mut observation = tokio::select! {
        observation = &mut execution => observation,
        () = parent.cancelled() => { cancellation.cancel(); execution.await },
        () = batch.cancelled() => { cancellation.cancel(); execution.await },
        () = deadline.elapsed() => { timed_out = true; cancellation.cancel(); execution.await },
    };
    if timed_out && observation.disposition == ToolDisposition::Cancelled {
        observation.disposition = ToolDisposition::Limit;
        observation.limit_cause = timeout_limit_cause(deadline.cause);
    }
    Answer {
        checkpoint: None,
        observation,
        active_time: None,
        stop: timed_out.then_some(match deadline.cause {
            Some(ActiveDeadlineCause::Goal) => StopCause::ActiveTime,
            Some(ActiveDeadlineCause::Delegation) => StopCause::DelegationDeadline,
            None => StopCause::ToolStopped,
        }),
    }
}

pub(super) fn model_order(step: &mut ToolStep, order: &[usize]) {
    let mut indices: Vec<_> = (0..step.observations.len()).collect();
    indices.sort_by_key(|&index| order[index]);
    let mut observations: Vec<_> = std::mem::take(&mut step.observations)
        .into_iter()
        .map(Some)
        .collect();
    let narrowed = std::mem::take(&mut step.narrowed_observations);
    let mut feedback = std::mem::take(&mut step.hook_feedback);
    for (next, old) in indices.into_iter().enumerate() {
        step.observations
            .push(observations[old].take().expect("each observation once"));
        if narrowed.contains(&old) {
            step.narrowed_observations.insert(next);
        }
        if let Some(lines) = feedback.remove(&old) {
            step.hook_feedback.insert(next, lines);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_completions_stop_admission_with_the_settled_cause() {
        use StopCause::{ActiveTime, Cancelled, HookStop, ToolStopped, Uncertain};
        use ToolDisposition::{Failed, Limit, Succeeded};
        let rows = [
            (None, Succeeded, false, None, StepEnd::Continue),
            (None, Limit, false, None, StepEnd::Stopped(ToolStopped)),
            (
                Some(Uncertain),
                Limit,
                false,
                None,
                StepEnd::Stopped(Uncertain),
            ),
            (None, Limit, true, None, StepEnd::Stopped(ActiveTime)),
            (
                None,
                Failed,
                false,
                Some(ToolStopped),
                StepEnd::Stopped(ToolStopped),
            ),
            (
                None,
                Succeeded,
                false,
                Some(Cancelled),
                StepEnd::Stopped(Cancelled),
            ),
            (
                Some(Cancelled),
                Failed,
                false,
                Some(Uncertain),
                StepEnd::Stopped(Uncertain),
            ),
            (
                Some(HookStop),
                Succeeded,
                true,
                None,
                StepEnd::Stopped(HookStop),
            ),
        ];
        for (stop, disposition, exhausted, observed, expected) in rows {
            assert_eq!(
                decide_read_end(stop, disposition, exhausted, observed),
                expected
            );
        }
    }

    #[test]
    fn deferred_hooks_continue_after_tool_limits_and_respect_terminal_stops() {
        assert_eq!(
            post_hook_progress(StepEnd::Continue, false, false),
            StepEnd::Continue
        );
        for cause in [
            StopCause::Uncertain,
            StopCause::Cancelled,
            StopCause::HookStop,
            StopCause::Paused,
            StopCause::ActiveTime,
            StopCause::Budget,
            StopCause::Capacity,
            StopCause::ContextFull,
            StopCause::DelegationDeadline,
            StopCause::Refused(crate::session::ProposalRefusalCause::Phase),
            StopCause::ToolStopped,
            StopCause::InvalidCommandResult,
        ] {
            let stopped = StepEnd::Stopped(cause);
            let expected = if cause == StopCause::ToolStopped {
                StepEnd::Continue
            } else {
                stopped
            };
            assert_eq!(
                post_hook_progress(stopped, false, false),
                expected,
                "{cause:?}"
            );
        }
        for (end, cancelled, paused, expected) in [
            (StepEnd::Continue, true, false, StopCause::Cancelled),
            (StepEnd::Continue, false, true, StopCause::Paused),
            (
                StepEnd::Stopped(StopCause::ToolStopped),
                true,
                false,
                StopCause::Cancelled,
            ),
            (
                StepEnd::Stopped(StopCause::ToolStopped),
                false,
                true,
                StopCause::Paused,
            ),
            (
                StepEnd::Stopped(StopCause::Uncertain),
                true,
                false,
                StopCause::Uncertain,
            ),
            (
                StepEnd::Stopped(StopCause::HookStop),
                false,
                true,
                StopCause::HookStop,
            ),
        ] {
            assert_eq!(
                post_hook_progress(end, cancelled, paused),
                StepEnd::Stopped(expected)
            );
        }
    }
}
