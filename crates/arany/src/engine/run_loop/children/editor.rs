use super::decide::decide_child_call;
use super::*;

impl RunLoop<'_> {
    pub(super) async fn classify_editor_call(
        &self,
        child: &ChildWork,
        result: CallResult,
        limits: Option<crate::provider::RateLimits>,
        valid_response: bool,
        abort: Option<WaveStop>,
        deadline: Instant,
    ) -> Result<ChildDecision, EngineError> {
        let gate_failed = valid_response
            && matches!(
                &result,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Answer(_),
                    ..
                })
            )
            && self.criterion_unmet(child.agent_run_id)?;
        let mut record = call_record(
            AgentPhase::ChildWork,
            &result,
            valid_response && !gate_failed,
            None,
            None,
            limits,
        );
        if gate_failed {
            record.failure_reason = Some(RepromptReason::CriterionNotMetInView.failure_reason());
        }
        self.append(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id: child.agent_run_id,
            record,
        })
        .await?;
        self.ledger().child_call_finished()?;
        let tools = valid_response
            && matches!(
                &result,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tools(_),
                    ..
                })
            );
        // The stop boundary commits before a continuation or Tool.
        let worker_stopped =
            abort.is_none() && (gate_failed || tools) && self.observe_worker_stop().await?;
        let gate = GateFacts {
            unmet: gate_failed,
            worker_stopped,
            stop_room: self
                .controls
                .tools
                .zip(child.tools.as_ref())
                .zip(match &result {
                    CallResult::Response(ProviderResponse { turn, .. }) => Some(turn),
                    CallResult::Failed(_) | CallResult::Cancelled => None,
                })
                .is_some_and(|((runtime, tools), turn)| {
                    tools.has_stop_room(turn, runtime.context_limit_bytes())
                }),
            continuations_left: gate_failed
                && self.ledger().view(|v| {
                    v.runs
                        .last()
                        .expect("active Run")
                        .agents
                        .iter()
                        .find(|a| a.id == child.agent_run_id)
                        .and_then(|a| a.assignment.as_ref())
                        .is_some_and(|w| {
                            w.gate_continuations < crate::worker::MAX_EDITOR_CONTINUATIONS
                        })
                })?,
        };
        let facts = self.child_facts(child, deadline)?;
        Ok(decide_child_call(
            result,
            valid_response,
            Some(gate),
            abort,
            &facts,
        ))
    }

    pub(super) async fn apply_editor_call(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        decision: ChildDecision,
        abort: &mut Option<WaveStop>,
    ) -> Result<RunProgress<ChildProgress>, EngineError> {
        let mut step_stop = None;
        let (answer, failure) = match decision {
            ChildDecision::Reprompt(reason, turn) => {
                Self::append_editor_reprompt(&mut wave.children[index], reason, turn);
                return self.refill_editor_child(wave, index).await;
            }
            ChildDecision::Finish { answer, turn, .. } => {
                let deadline = wave.deadline();
                match self
                    .subagent_stop(&mut wave.children[index], &answer, turn, abort, deadline)
                    .await?
                {
                    ChildStop::Proceed => (Some(answer), WorkerCause::ToolFailed),
                    ChildStop::Reprompt => {
                        return self.refill_editor_child(wave, index).await;
                    }
                    ChildStop::End { cause, .. } => (None, cause),
                }
            }
            ChildDecision::ToolStep(requests, turn) => {
                let (stop, can_continue) =
                    self.run_editor_tool(wave, index, requests, turn).await?;
                step_stop = stop;
                if step_stop.is_none() && can_continue {
                    wave.queue.pending.push_back(index);
                    match self.refill_wave(wave).await {
                        Ok(RunProgress::Ready(())) => {}
                        Ok(RunProgress::Stopped(cause)) => {
                            wave.queue.abort_and_drain().await;
                            return Ok(RunProgress::Stopped(cause));
                        }
                        Err(error) => return Err(wave.queue.abort_with(error).await),
                    }
                    return Ok(RunProgress::Ready(ChildProgress::Refilled));
                }
                (None, WorkerCause::ToolFailed)
            }
            ChildDecision::End(ChildEnd::Editor(cause)) => (None, cause),
            ChildDecision::RefusedTools(..)
            | ChildDecision::End(ChildEnd::Researcher(_) | ChildEnd::Skipped { .. }) => {
                return Err(wave.queue.abort_with(EngineError::CoordinatorFailed).await);
            }
        };
        self.settle_editor_call(
            wave,
            index,
            EditorCompletion {
                answer,
                failure,
                step_stop,
            },
            *abort,
        )
        .await
        .map(|admission| {
            RunProgress::from(admission)
                .map(|disposition| ChildProgress::Settled(Some(disposition)))
        })
    }

    async fn refill_editor_child(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
    ) -> Result<RunProgress<ChildProgress>, EngineError> {
        wave.queue.pending.push_back(index);
        Ok(self
            .refill_wave(wave)
            .await?
            .map(|()| ChildProgress::Refilled))
    }

    fn append_editor_reprompt(child: &mut ChildWork, reason: RepromptReason, turn: ProviderTurn) {
        let text = reason.text().to_owned();
        if let Some(tools) = child.tools.as_mut() {
            tools.add_stop_feedback(&turn, &text);
        }
        child
            .stop_hook_feedback
            .push(crate::provider::StopHookFeedback {
                after_step: child.tools.as_ref().map_or(0, |t| t.steps.len()),
                turn,
                text,
                facts: None,
                steering_notes: Vec::new(),
            });
    }

    async fn settle_editor_call(
        &self,
        wave: &mut ChildWave<'_>,
        index: usize,
        completion: EditorCompletion,
        abort: Option<WaveStop>,
    ) -> Result<Admission<AgentDisposition>, EngineError> {
        let EditorCompletion {
            answer,
            failure,
            step_stop,
        } = completion;
        if let Some(s) = wave.segments.take() {
            self.settle_active_segment(s)?;
        }
        {
            let _active_work = self.ledger().active_work();
            let deadline = wave.deadline();
            let integration = if answer.is_some() && abort.is_none() {
                self.integrate_editor(&mut wave.children[index], deadline)
                    .await?
            } else {
                None
            };
            let completion = self
                .complete_worker(
                    &mut wave.children[index],
                    WorkerEnd {
                        answer,
                        torn_down: abort.is_some_and(WaveStop::tears_down),
                        failure,
                        step_stop,
                        integration,
                    },
                )
                .await?;
            if let WorkerCompletion::Uncertain = completion {
                wave.queue.stop = Some(StopCause::Uncertain);
            }
        }
        self.resume_delegation(wave)?;
        Ok(Admission::Reserved(
            if self.actor_hook_requested_stop(wave.children[index].agent_run_id)? {
                AgentDisposition::StoppedByHook
            } else {
                AgentDisposition::Finished
            },
        ))
    }
}
