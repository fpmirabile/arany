use super::ledger::{Action, GoalLedger};
use super::progress::ProgressPublisher;
use super::run_loop::{CallResult, invoke_bounded_with_preview};
use super::{EngineError, RunCancellation};
use crate::journal::Journal;
use crate::provider::{
    AdmittedProviders, MAX_PROVIDER_SWITCHES, Provider, ProviderError, ProviderFallback,
    ProviderIdentity, ProviderRequest, ProviderResponse, ProviderRole, ResponsePreview,
};
use crate::session::{AgentRunId, Event};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::time::Instant;

pub(super) type CompactionOutcome = (
    Result<crate::provider::CompactionResponse, ProviderError>,
    Arc<dyn Provider>,
);

pub(super) struct CallRouter<'a> {
    pub(super) providers: AdmittedProviders,
    pub(super) journal: &'a Journal,
    pub(super) ledger: &'a GoalLedger,
    pub(super) publisher: Option<&'a ProgressPublisher>,
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
        let chain = self.providers.get(role);
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

    pub(super) async fn invoke(
        &self,
        mut request: ProviderRequest,
        deadline: Instant,
        cancellation: RunCancellation,
        preview: Option<ResponsePreview>,
    ) -> Result<CallResult, EngineError> {
        let role = ProviderRole::for_phase(request.phase);
        if self.providers.get(role).len() == 1 {
            let provider = self.provider(role, request.agent_run_id)?;
            request.model = provider.identity().model.clone();
            request.max_output_tokens = request
                .max_output_tokens
                .min(provider.identity().limits.max_output_tokens);
            return Ok(invoke_bounded_with_preview(
                provider.as_ref(),
                request,
                deadline,
                cancellation,
                preview,
            )
            .await);
        }
        let preview = preview.unwrap_or_else(|| ResponsePreview::new().0);
        loop {
            let provider = self.provider(role, request.agent_run_id)?;
            request.model = provider.identity().model.clone();
            request.max_output_tokens = request
                .max_output_tokens
                .min(provider.identity().limits.max_output_tokens);
            let attempt = invoke_attempt(
                provider.as_ref(),
                request.clone(),
                deadline,
                cancellation.clone(),
                Some(preview.clone()),
            )
            .await;
            let result = attempt.result;
            if !attempt.eligible
                || preview.has_output()
                || cancellation.is_cancelled()
                || Instant::now() >= deadline
                || !self
                    .switch(
                        (request.session_id, request.run_id, request.agent_run_id),
                        role,
                        provider.identity(),
                        Some(&request),
                    )
                    .await?
            {
                return Ok(result);
            }
        }
    }

    pub(super) async fn compact(
        &self,
        request: crate::provider::CompactionRequest,
        actor: AgentRunId,
    ) -> Result<CompactionOutcome, EngineError> {
        self.compact_loop(request, actor, None)
            .await?
            .ok_or(EngineError::CoordinatorFailed)
    }

    /// Like `compact`, but cancellation and `bound` interrupt only the Provider wait; a durable
    /// fallback switch always completes first. `None` means the Run was cancelled mid-wait.
    pub(super) async fn compact_interruptible(
        &self,
        request: crate::provider::CompactionRequest,
        actor: AgentRunId,
        bound: Duration,
        cancellation: RunCancellation,
    ) -> Result<Option<CompactionOutcome>, EngineError> {
        self.compact_loop(request, actor, Some((bound, cancellation)))
            .await
    }

    async fn compact_loop(
        &self,
        mut request: crate::provider::CompactionRequest,
        actor: AgentRunId,
        interrupt: Option<(Duration, RunCancellation)>,
    ) -> Result<Option<CompactionOutcome>, EngineError> {
        loop {
            let provider = self.provider(ProviderRole::Compaction, actor)?;
            request.model = provider.identity().model.clone();
            request.max_output_tokens = request
                .max_output_tokens
                .min(provider.identity().limits.max_output_tokens);
            let limit = provider.identity().limits.deadline;
            let call = provider.compact(request.clone());
            let response = match &interrupt {
                None => match tokio::time::timeout(limit, call).await {
                    Ok(response) => response,
                    Err(_) => Err(ProviderError::CallTimedOut),
                },
                Some((bound, cancellation)) => {
                    let mut cancellation = cancellation.clone();
                    tokio::select! {
                        biased;
                        () = cancellation.cancelled() => return Ok(None),
                        response = tokio::time::timeout(limit.min(*bound), call) => {
                            response.unwrap_or(Err(ProviderError::CallTimedOut))
                        }
                    }
                }
            };
            if !response
                .as_ref()
                .is_err_and(|error| error.allows_fallback())
                || !self
                    .switch(
                        (request.session_id, request.covered_run_id, actor),
                        ProviderRole::Compaction,
                        provider.identity(),
                        None,
                    )
                    .await?
            {
                return Ok(Some((response, provider)));
            }
        }
    }

    fn can_switch(
        &self,
        role: ProviderRole,
        from: &ProviderIdentity,
        request: &ProviderRequest,
    ) -> bool {
        let history = request
            .input
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.steps.is_empty());
        self.next(role, from)
            .is_some_and(|to| !history || from.shares_account(to.identity()))
    }

    fn next(&self, role: ProviderRole, from: &ProviderIdentity) -> Option<&Arc<dyn Provider>> {
        let chain = self.providers.get(role);
        chain
            .iter()
            .position(|provider| provider.identity() == from)
            .and_then(|index| chain.get(index + 1))
    }

    pub(super) async fn switch(
        &self,
        scope: (crate::session::SessionId, crate::session::RunId, AgentRunId),
        role: ProviderRole,
        from: &ProviderIdentity,
        request: Option<&ProviderRequest>,
    ) -> Result<bool, EngineError> {
        if request.is_some_and(|request| !self.can_switch(role, from, request)) {
            return Ok(false);
        }
        let (session_id, run_id, agent_run_id) = scope;
        let Some(to) = self.next(role, from) else {
            return Ok(false);
        };
        let candidate = self.ledger.view(|view| {
            let run = view.runs.last()?;
            let goal = view
                .goals
                .iter()
                .find(|goal| goal.run_ids.contains(&run_id))?;
            if run.provider_fallbacks.len() >= MAX_PROVIDER_SWITCHES
                || goal.status == crate::session::GoalStatus::Running
                    && goal.effective_budget().max_tokens.is_some()
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
                previews: self
                    .publisher
                    .is_some_and(ProgressPublisher::previews_enabled),
                steering: false,
            },
            ProviderRole::Child => Action::Child {
                in_flight: self.ledger.in_flight_children(),
                research: request.is_none_or(|request| request.input.tools.is_some()),
            },
            ProviderRole::Review => Action::Review,
            ProviderRole::Compaction => Action::Compaction,
        };
        self.journal
            .admit(
                session_id,
                action.max_events_for(&self.providers) + 1,
                action.tool_observations(),
            )
            .await?;
        self.ledger
            .append(
                self.journal,
                session_id,
                Event::ProviderFallbackUsed {
                    run_id,
                    agent_run_id,
                    fallback,
                },
            )
            .await?;
        if let Some(reservation) = reservation {
            reservation.commit()?;
        }
        Ok(true)
    }
}

struct Attempt<'a> {
    provider: &'a dyn Provider,
    retryable: AtomicBool,
}

#[async_trait::async_trait]
impl Provider for Attempt<'_> {
    fn identity(&self) -> &ProviderIdentity {
        self.provider.identity()
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let result = self.provider.invoke(request).await;
        self.retryable.store(
            result.as_ref().is_err_and(ProviderError::allows_fallback),
            Ordering::Relaxed,
        );
        result
    }

    async fn invoke_with_preview(
        &self,
        request: ProviderRequest,
        preview: ResponsePreview,
    ) -> Result<ProviderResponse, ProviderError> {
        let result = self.provider.invoke_with_preview(request, preview).await;
        self.retryable.store(
            result.as_ref().is_err_and(|error| error.allows_fallback()),
            Ordering::Relaxed,
        );
        result
    }
}

pub(super) struct AttemptResult {
    pub(super) result: CallResult,
    pub(super) eligible: bool,
}

pub(super) async fn invoke_attempt(
    provider: &dyn Provider,
    request: ProviderRequest,
    deadline: Instant,
    cancellation: RunCancellation,
    preview: Option<ResponsePreview>,
) -> AttemptResult {
    let attempt = Attempt {
        provider,
        retryable: AtomicBool::new(false),
    };
    let result = invoke_bounded_with_preview(
        &attempt,
        request,
        deadline,
        cancellation.clone(),
        preview.clone(),
    )
    .await;
    AttemptResult {
        result,
        eligible: attempt.retryable.load(Ordering::Relaxed)
            && preview.as_ref().is_none_or(|preview| !preview.has_output())
            && !cancellation.is_cancelled()
            && Instant::now() < deadline,
    }
}
