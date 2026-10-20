use super::hints::HintPublisher;
use super::ledger::{Action, GoalLedger};
use super::run_loop::{CallResult, invoke_bounded_with_preview};
use super::{EngineError, RunCancellation, RunSteering};
use crate::journal::Journal;
use crate::provider::{
    AdmittedProviders, MAX_PROVIDER_RETRIES, MAX_PROVIDER_RETRY_DELAY_MS, MAX_PROVIDER_SWITCHES,
    Provider, ProviderError, ProviderFallback, ProviderIdentity, ProviderRequest, ProviderRetry,
    ProviderRole, ResponsePreview, RetryCause,
};
use crate::session::{AgentRole, AgentRunId, Event, RunId, SessionId};
use crate::store::StoreError;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Transient {
    pub(super) cause: RetryCause,
    pub(super) retry_after: Option<Duration>,
}

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptDecision {
    // The attempt's result is the call's outcome.
    Accept,
    // Same route, after a committed retry fact and the wait.
    Retry { cause: RetryCause, delay: Duration },
    // The next configured fallback route.
    Switch,
}

// Facts about one failed attempt, read once from the ledger view and the attempt.
#[derive(Clone, Copy, Debug)]
pub(super) struct AttemptFacts {
    pub(super) retries: u8,
    // Before the action deadline, which already ends inside the Goal's active time.
    pub(super) remaining: Duration,
    pub(super) stopping: bool,
    pub(super) displayed: bool,
    pub(super) visible: bool,
    pub(super) admitted: bool,
}

pub(super) fn decide_attempt(failure: Option<Transient>, facts: AttemptFacts) -> AttemptDecision {
    let Some(Transient { cause, retry_after }) = failure else {
        return AttemptDecision::Accept;
    };
    if facts.stopping || facts.displayed || !facts.admitted || facts.remaining.is_zero() {
        return AttemptDecision::Accept;
    }
    if facts.retries < MAX_PROVIDER_RETRIES {
        let backoff = ProviderRetry::backoff(facts.retries + 1);
        let delay = retry_after.map_or(backoff, |after| after.max(backoff));
        // A Retry-After that cannot fit is not waited out: the call fails now.
        if delay < facts.remaining
            && delay <= Duration::from_millis(u64::from(MAX_PROVIDER_RETRY_DELAY_MS))
        {
            return AttemptDecision::Retry { cause, delay };
        }
    }
    match cause {
        RetryCause::ServerError if !facts.visible => AttemptDecision::Switch,
        RetryCause::ServerError
        | RetryCause::Transport
        | RetryCause::RateLimited
        | RetryCause::StreamCut => AttemptDecision::Accept,
    }
}

#[must_use]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Backoff {
    Elapsed,
    Paused,
    Cancelled,
}

// The one wait both attempt loops use. A pause or cancellation ends it and is checked again
// just before the re-dispatch, so neither allows another reservation or call.
pub(super) async fn back_off(
    session_id: SessionId,
    delay: Duration,
    cancellation: &mut RunCancellation,
    steering: Option<&RunSteering>,
) -> Backoff {
    let paused = async {
        match steering {
            Some(steering) => steering.paused().await,
            None => std::future::pending().await,
        }
    };
    let ended = tokio::select! {
        biased;
        () = cancellation.cancelled() => Backoff::Cancelled,
        () = paused => Backoff::Paused,
        () = wait(session_id, delay) => Backoff::Elapsed,
    };
    if cancellation.is_cancelled() {
        Backoff::Cancelled
    } else if steering.is_some_and(RunSteering::pause_requested) {
        Backoff::Paused
    } else {
        ended
    }
}

async fn wait(session_id: SessionId, delay: Duration) {
    #[cfg(any(test, feature = "test-support"))]
    match crate::fault::FaultPlan::retry_backoff(session_id) {
        Some(crate::fault::RetryBackoff::Immediate) => return,
        Some(crate::fault::RetryBackoff::Gated(entered, release)) => {
            let _ = entered.send(());
            let _ = release.await;
            return;
        }
        None => {}
    }
    #[cfg(not(any(test, feature = "test-support")))]
    let _ = session_id;
    tokio::time::sleep(delay).await;
}

pub(super) struct CompactionOutcome {
    pub(super) response: Result<crate::provider::CompactionResponse, ProviderError>,
    pub(super) provider: Arc<dyn Provider>,
}

#[derive(Clone, Copy)]
pub(super) struct CallScope {
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) agent_run_id: AgentRunId,
}

pub(super) struct CallRouter<'a> {
    pub(super) providers: AdmittedProviders,
    pub(super) journal: &'a Journal,
    pub(super) ledger: &'a GoalLedger,
    pub(super) publisher: Option<&'a HintPublisher>,
    pub(super) steering: Option<RunSteering>,
}

pub(super) fn switched_identity(
    run: &crate::session::RunView,
    actor: AgentRunId,
    role: ProviderRole,
) -> Option<&ProviderIdentity> {
    run.provider_fallbacks
        .iter()
        .rev()
        .find(|(id, fallback)| *id == actor && fallback.role == role)
        .map(|(_, fallback)| &fallback.to)
}

impl CallRouter<'_> {
    pub(super) fn provider(
        &self,
        role: ProviderRole,
        actor: AgentRunId,
    ) -> Result<Arc<dyn Provider>, EngineError> {
        let chain = self.chain(role, actor)?;
        let index = self.ledger.view(|view| {
            match view
                .runs
                .last()
                .and_then(|run| switched_identity(run, actor, role))
            {
                Some(identity) => chain
                    .iter()
                    .position(|provider| provider.identity() == identity)
                    .ok_or(EngineError::InvalidHistory),
                None => Ok(0),
            }
        })??;
        Ok(Arc::clone(&chain[index]))
    }

    pub(super) fn chain(
        &self,
        role: ProviderRole,
        actor: AgentRunId,
    ) -> Result<Vec<Arc<dyn Provider>>, EngineError> {
        if role == ProviderRole::Child
            && let Some(roster) = &self.providers.roster
        {
            let profile = self
                .ledger
                .view(|view| {
                    view.runs
                        .last()?
                        .agents
                        .iter()
                        .find(|agent| agent.id == actor)?
                        .profile
                        .clone()
                })?
                .ok_or(EngineError::InvalidHistory)?;
            return roster
                .profiles
                .iter()
                .find(|(name, _)| name == &profile)
                .map(|(_, chain)| chain.clone())
                .ok_or(EngineError::InvalidHistory);
        }
        Ok(self.providers.get(role).clone())
    }

    pub(super) fn bind(
        &self,
        role: ProviderRole,
        actor: AgentRunId,
        model: &mut String,
        max_output_tokens: &mut u32,
    ) -> Result<Arc<dyn Provider>, EngineError> {
        let provider = self.provider(role, actor)?;
        let identity = provider.identity();
        *model = identity.model.clone();
        *max_output_tokens = (*max_output_tokens).min(identity.limits.max_output_tokens);
        Ok(provider)
    }

    pub(super) async fn invoke(
        &self,
        mut request: ProviderRequest,
        deadline: Instant,
        mut cancellation: RunCancellation,
        preview: Option<ResponsePreview>,
    ) -> Result<CallResult, EngineError> {
        let role = ProviderRole::for_phase(request.phase);
        let scope = CallScope {
            session_id: request.session_id,
            run_id: request.run_id,
            agent_run_id: request.agent_run_id,
        };
        // A private observer guards a fallback chain against visible output.
        let preview = match preview {
            None if self.chain(role, request.agent_run_id)?.len() > 1 => {
                Some(ResponsePreview::new().0)
            }
            preview => preview,
        };
        loop {
            let provider = self.bind(
                role,
                request.agent_run_id,
                &mut request.model,
                &mut request.max_output_tokens,
            )?;
            let attempt = invoke_attempt(
                provider.as_ref(),
                request.clone(),
                deadline,
                cancellation.clone(),
                preview.clone(),
            )
            .await;
            let facts = self.attempt_facts(
                scope.agent_run_id,
                role,
                deadline,
                cancellation.is_cancelled() || self.pause_requested(),
                attempt.visible,
            )?;
            match decide_attempt(attempt.transient, facts) {
                AttemptDecision::Accept => return Ok(attempt.result),
                AttemptDecision::Retry { cause, delay } => {
                    // Cleared before any await, so no stale text is journaled for the call.
                    if let Some(preview) = &preview {
                        preview.clear();
                    }
                    if !self
                        .retry(scope, role, cause, delay, Some(&request))
                        .await?
                    {
                        return Ok(attempt.result);
                    }
                    // A pause ends the call unsent, as a pause during an approval does.
                    match back_off(
                        scope.session_id,
                        delay,
                        &mut cancellation,
                        self.steering.as_ref(),
                    )
                    .await
                    {
                        Backoff::Elapsed => {}
                        Backoff::Paused | Backoff::Cancelled => return Ok(CallResult::Cancelled),
                    }
                }
                AttemptDecision::Switch => {
                    if !self
                        .switch(scope, role, provider.identity(), Some(&request))
                        .await?
                    {
                        return Ok(attempt.result);
                    }
                }
            }
        }
    }

    pub(super) fn pause_requested(&self) -> bool {
        self.steering
            .as_ref()
            .is_some_and(RunSteering::pause_requested)
    }

    pub(super) fn attempt_facts(
        &self,
        actor: AgentRunId,
        role: ProviderRole,
        deadline: Instant,
        stopping: bool,
        visible: bool,
    ) -> Result<AttemptFacts, EngineError> {
        let (retries, displayed, running) = self.ledger.view(|view| {
            let running = view.active_goal().is_some();
            let current = view.runs.last().and_then(|run| {
                let agent = run.agents.iter().find(|agent| agent.id == actor)?;
                let index = call_index(view, run, agent, role)?;
                let retries = earlier_retries(run, actor, role, index);
                let displayed = role == ProviderRole::Primary
                    && agent.role == AgentRole::Primary
                    && run.display.iter().any(|chunk| chunk.call_index == index);
                Some((u8::try_from(retries).unwrap_or(u8::MAX), displayed))
            });
            let (retries, displayed) = current.unwrap_or((MAX_PROVIDER_RETRIES, true));
            (retries, displayed, running)
        })?;
        // Without a running Goal only a between-Run compaction can retry, with nothing to charge.
        let admitted = if running {
            self.ledger.can_start_provider_call()
                && self.ledger.remaining_calls() != Some(0)
                && self
                    .ledger
                    .effective_budget()
                    .is_ok_and(|budget| budget.max_tokens.is_none())
        } else {
            role == ProviderRole::Compaction
        };
        Ok(AttemptFacts {
            retries,
            remaining: deadline.saturating_duration_since(Instant::now()),
            stopping,
            displayed,
            visible,
            admitted,
        })
    }

    pub(super) async fn compact(
        &self,
        mut request: crate::provider::CompactionRequest,
        actor: AgentRunId,
        deadline: Instant,
        mut cancellation: RunCancellation,
    ) -> Result<Option<CompactionOutcome>, EngineError> {
        loop {
            let provider = self.bind(
                ProviderRole::Compaction,
                actor,
                &mut request.model,
                &mut request.max_output_tokens,
            )?;
            if cancellation.is_cancelled() {
                return Ok(None);
            }
            let attempt_deadline =
                deadline.min(Instant::now() + provider.identity().limits.deadline);
            let response = if Instant::now() >= attempt_deadline {
                Err(ProviderError::CallTimedOut)
            } else {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Ok(None),
                    response = tokio::time::timeout_at(attempt_deadline, provider.compact(request.clone())) => {
                        response.unwrap_or(Err(ProviderError::CallTimedOut))
                    }
                }
            };
            // A failure under cancellation or pause ends the compaction unsent, with no further attempt.
            if response.is_err() && (cancellation.is_cancelled() || self.pause_requested()) {
                return Ok(None);
            }
            let scope = CallScope {
                session_id: request.session_id,
                run_id: request.covered_run_id,
                agent_run_id: actor,
            };
            let transient = response
                .as_ref()
                .err()
                .and_then(ProviderError::retry_cause)
                .map(|(cause, retry_after)| Transient { cause, retry_after });
            let facts = self.attempt_facts(
                actor,
                ProviderRole::Compaction,
                deadline,
                cancellation.is_cancelled() || self.pause_requested(),
                false,
            )?;
            let continued = match decide_attempt(transient, facts) {
                AttemptDecision::Accept => false,
                AttemptDecision::Retry { cause, delay } => {
                    if !self
                        .retry(scope, ProviderRole::Compaction, cause, delay, None)
                        .await?
                    {
                        false
                    } else {
                        match back_off(
                            scope.session_id,
                            delay,
                            &mut cancellation,
                            self.steering.as_ref(),
                        )
                        .await
                        {
                            Backoff::Elapsed => true,
                            Backoff::Paused | Backoff::Cancelled => return Ok(None),
                        }
                    }
                }
                AttemptDecision::Switch => {
                    self.switch(scope, ProviderRole::Compaction, provider.identity(), None)
                        .await?
                }
            };
            if !continued {
                return Ok(Some(CompactionOutcome { response, provider }));
            }
        }
    }

    fn next(
        &self,
        role: ProviderRole,
        actor: AgentRunId,
        from: &ProviderIdentity,
    ) -> Result<Option<Arc<dyn Provider>>, EngineError> {
        let chain = self.chain(role, actor)?;
        Ok(chain
            .iter()
            .position(|provider| provider.identity() == from)
            .and_then(|index| chain.get(index + 1))
            .cloned())
    }

    pub(super) async fn switch(
        &self,
        scope: CallScope,
        role: ProviderRole,
        from: &ProviderIdentity,
        request: Option<&ProviderRequest>,
    ) -> Result<bool, EngineError> {
        let CallScope {
            run_id,
            agent_run_id,
            ..
        } = scope;
        let Some(to) = self.next(role, agent_run_id, from)? else {
            return Ok(false);
        };
        if request.is_some_and(|request| {
            request
                .input
                .tools
                .as_ref()
                .is_some_and(|tools| !tools.steps.is_empty())
                && !from.shares_account(to.identity())
        }) {
            return Ok(false);
        }
        let candidate = self.ledger.view(|view| {
            let run = view.runs.last()?;
            let goal = view
                .goals
                .iter()
                .find(|goal| goal.run_ids.contains(&run_id));
            if run.provider_fallbacks.len() >= MAX_PROVIDER_SWITCHES
                || goal.is_some_and(|goal| {
                    goal.status == crate::session::GoalStatus::Running
                        && goal.effective_budget().max_tokens.is_some()
                })
            {
                return None;
            }
            let agent = run.agents.iter().find(|agent| agent.id == agent_run_id)?;
            Some(ProviderFallback {
                role,
                failure: crate::session::ProviderCallDisposition::Unavailable,
                call_index: u32::try_from(if role == ProviderRole::Compaction {
                    if run.status == crate::session::RunStatus::Active {
                        run.tools.len()
                    } else {
                        view.compactions.len()
                    }
                } else {
                    agent.provider_calls.len()
                })
                .ok()?,
                from: from.clone(),
                to: to.identity().clone(),
            })
        })?;
        let Some(fallback) = candidate else {
            return Ok(false);
        };
        self.commit_attempt(
            scope,
            role,
            request,
            Event::ProviderFallbackUsed {
                run_id,
                agent_run_id,
                fallback,
            },
        )
        .await
    }

    pub(super) async fn retry(
        &self,
        scope: CallScope,
        role: ProviderRole,
        cause: RetryCause,
        delay: Duration,
        request: Option<&ProviderRequest>,
    ) -> Result<bool, EngineError> {
        let retry = self.ledger.view(|view| {
            let run = view.runs.last()?;
            let agent = run
                .agents
                .iter()
                .find(|agent| agent.id == scope.agent_run_id)?;
            let call_index = call_index(view, run, agent, role)?;
            let earlier = earlier_retries(run, scope.agent_run_id, role, call_index);
            Some(ProviderRetry {
                role,
                call_index,
                attempt: u8::try_from(earlier + 1).ok()?,
                cause,
                delay_ms: u32::try_from(delay.as_millis()).ok()?,
            })
        })?;
        let Some(retry) = retry else {
            return Ok(false);
        };
        self.commit_attempt(
            scope,
            role,
            request,
            Event::ProviderRetryScheduled {
                run_id: scope.run_id,
                agent_run_id: scope.agent_run_id,
                retry,
            },
        )
        .await
    }

    // Charges the failed attempt and admits the next one before its fact commits; a refusal declines it.
    async fn commit_attempt(
        &self,
        scope: CallScope,
        role: ProviderRole,
        request: Option<&ProviderRequest>,
        fact: Event,
    ) -> Result<bool, EngineError> {
        let running = self.ledger.view(|view| view.active_goal().is_some())?;
        let reservation = if running {
            Some(
                match self
                    .ledger
                    .reserve(Action::ProviderCall {
                        previews: false,
                        steering: false,
                    })
                    .await
                {
                    Ok(reservation) => reservation,
                    Err(EngineError::GoalBudgetExhausted) => return Ok(false),
                    Err(error) => return Err(error),
                },
            )
        } else {
            None
        };
        let action = match role {
            ProviderRole::Primary => Action::ProviderCall {
                previews: self.publisher.is_some_and(HintPublisher::previews_enabled),
                steering: false,
            },
            ProviderRole::Child => Action::Child {
                in_flight: self.ledger.in_flight_children(),
                research: request.is_none_or(|request| request.input.tools.is_some()),
            },
            ProviderRole::Review => Action::Review,
            ProviderRole::Compaction => Action::Compaction,
        };
        // A refused admission declines the attempt, so the known failure settles as its own outcome.
        match self
            .journal
            .admit(
                scope.session_id,
                action.max_events_for_admitted(&self.providers)
                    + self.ledger.verdicts(action)
                    + 1
                    + if role == ProviderRole::Primary {
                        self.ledger.drain_headroom()
                    } else {
                        0
                    },
                action.tool_observations(),
            )
            .await
        {
            Ok(()) => {}
            Err(StoreError::ReplayLimit | StoreError::StorageFull) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        self.ledger
            .append(self.journal, scope.session_id, fact)
            .await?;
        if let Some(reservation) = reservation {
            reservation.commit()?;
        }
        Ok(true)
    }
}

// The semantic call an attempt belongs to; compaction numbers its calls as a switch does.
fn call_index(
    view: &crate::session::SessionView,
    run: &crate::session::RunView,
    agent: &crate::session::AgentView,
    role: ProviderRole,
) -> Option<u32> {
    let index = match role {
        ProviderRole::Compaction if run.status == crate::session::RunStatus::Active => {
            run.tools.len()
        }
        ProviderRole::Compaction => view.compactions.len(),
        ProviderRole::Primary | ProviderRole::Child | ProviderRole::Review => {
            agent.provider_calls.len()
        }
    };
    u32::try_from(index).ok()
}

fn earlier_retries(
    run: &crate::session::RunView,
    actor: AgentRunId,
    role: ProviderRole,
    call_index: u32,
) -> usize {
    run.provider_retries
        .iter()
        .filter(|(id, retry)| *id == actor && retry.role == role && retry.call_index == call_index)
        .count()
}

pub(super) struct AttemptResult {
    pub(super) result: CallResult,
    pub(super) transient: Option<Transient>,
    pub(super) visible: bool,
}

pub(super) async fn invoke_attempt(
    provider: &dyn Provider,
    request: ProviderRequest,
    deadline: Instant,
    cancellation: RunCancellation,
    preview: Option<ResponsePreview>,
) -> AttemptResult {
    let (result, transient) =
        invoke_bounded_with_preview(provider, request, deadline, cancellation, preview.clone())
            .await;
    AttemptResult {
        result,
        transient,
        visible: preview.as_ref().is_some_and(ResponsePreview::has_output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> AttemptFacts {
        AttemptFacts {
            retries: 0,
            remaining: Duration::from_secs(600),
            stopping: false,
            displayed: false,
            visible: false,
            admitted: true,
        }
    }

    fn failure(cause: RetryCause, retry_after: Option<u64>) -> Option<Transient> {
        Some(Transient {
            cause,
            retry_after: retry_after.map(Duration::from_secs),
        })
    }

    #[test]
    fn attempt_decision_retries_three_times_inside_the_deadline_then_switches_or_fails() {
        use AttemptDecision::{Accept, Retry, Switch};
        use RetryCause::{RateLimited, ServerError, StreamCut};
        let retry = |cause, secs| Retry {
            cause,
            delay: Duration::from_secs(secs),
        };
        let cases: [(&str, Option<Transient>, AttemptFacts, AttemptDecision); 16] = [
            ("no transient failure", None, facts(), Accept),
            (
                "first rate limit",
                failure(RateLimited, None),
                facts(),
                retry(RateLimited, 2),
            ),
            (
                "second gap doubles",
                failure(StreamCut, None),
                AttemptFacts {
                    retries: 1,
                    ..facts()
                },
                retry(StreamCut, 4),
            ),
            (
                "third gap doubles",
                failure(ServerError, None),
                AttemptFacts {
                    retries: 2,
                    ..facts()
                },
                retry(ServerError, 8),
            ),
            (
                "server error after the last retry",
                failure(ServerError, None),
                AttemptFacts {
                    retries: 3,
                    ..facts()
                },
                Switch,
            ),
            (
                "rate limit after the last retry",
                failure(RateLimited, None),
                AttemptFacts {
                    retries: 3,
                    ..facts()
                },
                Accept,
            ),
            (
                "cut after the last retry",
                failure(StreamCut, None),
                AttemptFacts {
                    retries: 3,
                    ..facts()
                },
                Accept,
            ),
            (
                "longer Retry-After is honored",
                failure(RateLimited, Some(30)),
                facts(),
                retry(RateLimited, 30),
            ),
            (
                "shorter Retry-After keeps the backoff",
                failure(RateLimited, Some(0)),
                AttemptFacts {
                    retries: 2,
                    ..facts()
                },
                retry(RateLimited, 8),
            ),
            (
                "hostile Retry-After past the deadline",
                failure(RateLimited, Some(u64::from(u32::MAX))),
                facts(),
                Accept,
            ),
            (
                "backoff past the deadline switches",
                failure(ServerError, None),
                AttemptFacts {
                    remaining: Duration::from_secs(2),
                    ..facts()
                },
                Switch,
            ),
            (
                "cancelled or paused",
                failure(RateLimited, None),
                AttemptFacts {
                    stopping: true,
                    ..facts()
                },
                Accept,
            ),
            (
                "committed display text",
                failure(StreamCut, None),
                AttemptFacts {
                    displayed: true,
                    ..facts()
                },
                Accept,
            ),
            (
                "visible preview still retries",
                failure(StreamCut, None),
                AttemptFacts {
                    visible: true,
                    ..facts()
                },
                retry(StreamCut, 2),
            ),
            (
                "visible preview never switches",
                failure(ServerError, None),
                AttemptFacts {
                    retries: 3,
                    visible: true,
                    ..facts()
                },
                Accept,
            ),
            (
                "call or token budget refuses",
                failure(ServerError, None),
                AttemptFacts {
                    admitted: false,
                    ..facts()
                },
                Accept,
            ),
        ];
        for (case, failure, facts, expected) in cases {
            assert_eq!(decide_attempt(failure, facts), expected, "{case}");
        }
        assert_eq!(
            decide_attempt(
                failure(ServerError, None),
                AttemptFacts {
                    remaining: Duration::ZERO,
                    ..facts()
                }
            ),
            Accept
        );
        assert_eq!(
            (1..=8).map(ProviderRetry::backoff).collect::<Vec<_>>(),
            [2, 4, 8, 10, 10, 10, 10, 10].map(Duration::from_secs)
        );
    }
}
