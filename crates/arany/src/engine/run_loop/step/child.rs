use super::*;

impl RunLoop<'_> {
    pub(super) async fn dispatch_child(
        &self,
        grant: &Grant<'_>,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
    ) -> Result<Answer, EngineError> {
        let Grant::Researcher {
            child, deadline, ..
        } = grant
        else {
            return Err(EngineError::CoordinatorFailed);
        };
        let deadline = grant.deadline().unwrap_or(*deadline);
        let active_deadline = match grant {
            Grant::Researcher {
                active_deadline, ..
            } => active_deadline.as_ref(),
            Grant::Primary { .. } => None,
        };
        let cancellation = RunCancellation::new();
        let mut parent = self.controls.cancellation.clone();
        let mut timed_out = false;
        let execution = async {
            let _active_work = self.ledger().active_work();
            if child.kind.is_editor() {
                self.dispatch_editor_effect(runtime, intent, queue_pause, &cancellation)
                    .await
            } else {
                Ok(Answer {
                    checkpoint: None,
                    observation: runtime.execute(intent, cancellation.clone()).await,
                    stop: None,
                    active_time: None,
                })
            }
        };
        tokio::pin!(execution);
        let mut answer = tokio::select! {
            biased;
            result = &mut execution => result?,
            () = parent.cancelled() => { cancellation.cancel(); execution.await? },
            () = async {
                match active_deadline {
                    Some(deadline) => deadline.elapsed().await,
                    None => tokio::time::sleep_until(deadline).await,
                }
            } => { timed_out = true; cancellation.cancel(); execution.await? },
        };
        let stop = match completion_deadline(
            answer.observation.disposition,
            timed_out,
            active_deadline.map(|deadline| deadline.cause),
        ) {
            StepEnd::Continue => None,
            StepEnd::Stopped(cause) => {
                answer.observation.disposition = ToolDisposition::Limit;
                answer.observation.limit_cause =
                    timeout_limit_cause(active_deadline.map(|deadline| deadline.cause));
                Some(cause)
            }
        };
        answer.stop = stop;
        Ok(answer)
    }
    async fn dispatch_editor_effect(
        &self,
        runtime: &ToolRuntime,
        intent: EffectIntent,
        queue_pause: Option<ActivePause>,
        cancellation: &RunCancellation,
    ) -> Result<Answer, EngineError> {
        let (observation, segment, exhausted, checkpoint) = self
            .dispatch_effect(runtime, intent, queue_pause, cancellation)
            .await?;
        Ok(Answer {
            checkpoint,
            observation,
            stop: None,
            active_time: Some(ActiveTime { segment, exhausted }),
        })
    }
}
