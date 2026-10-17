use super::ledger::{Action, GoalLedger};
use super::progress::{ProgressPublisher, RunScope, append_scoped};
use super::run_loop::{CallResult, invoke_bounded_with_preview};
use super::{EngineError, RunCancellation};
use crate::provider::{
    AdmittedProviders, MAX_PROVIDER_SWITCHES, Provider, ProviderError, ProviderFallback,
    ProviderIdentity, ProviderRequest, ProviderResponse, ProviderRole, ResponsePreview,
};
use crate::session::{AgentRunId, Event};
use crate::store::Store;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::time::Instant;

#[derive(Clone)]
pub(super) struct CallRouter {
    pub(super) providers: AdmittedProviders,
    pub(super) store: Arc<Store>,
    pub(super) ledger: Arc<GoalLedger>,
    pub(super) publisher: Option<ProgressPublisher>,
}

impl CallRouter {
    pub(super) fn provider(
        &self,
        role: ProviderRole,
        actor: AgentRunId,
    ) -> Result<Arc<dyn Provider>, EngineError> {
        let chain = self.providers.get(role);
        let index = self.ledger.view(|view| {
            match view.runs.last().and_then(|run| {
                run.provider_fallbacks
                    .iter()
                    .rev()
                    .find(|(id, fallback)| *id == actor && fallback.role == role)
            }) {
                Some((_, fallback)) => chain
                    .iter()
                    .position(|provider| provider.identity() == &fallback.to)
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
            let attempt = Attempt {
                provider: provider.as_ref(),
                retryable: AtomicBool::new(false),
            };
            let result = invoke_bounded_with_preview(
                &attempt,
                request.clone(),
                deadline,
                cancellation.clone(),
                Some(preview.clone()),
            )
            .await;
            if !attempt.retryable.load(Ordering::Relaxed)
                || preview.has_output()
                || cancellation.is_cancelled()
                || Instant::now() >= deadline
                || !self
                    .switch(
                        (request.session_id, request.run_id, request.agent_run_id),
                        role,
                        provider.identity(),
                        match &result {
                            CallResult::Failed(super::run_loop::CallFailure::Rejected(_)) => {
                                crate::session::ProviderCallDisposition::Rejected
                            }
                            _ => crate::session::ProviderCallDisposition::Unavailable,
                        },
                    )
                    .await?
            {
                return Ok(result);
            }
        }
    }

    pub(super) async fn compact(
        &self,
        mut request: crate::provider::CompactionRequest,
        actor: AgentRunId,
    ) -> Result<
        (
            Result<crate::provider::CompactionResponse, ProviderError>,
            Arc<dyn Provider>,
        ),
        EngineError,
    > {
        loop {
            let provider = self.provider(ProviderRole::Compaction, actor)?;
            request.model = provider.identity().model.clone();
            request.max_output_tokens = request
                .max_output_tokens
                .min(provider.identity().limits.max_output_tokens);
            let response = match tokio::time::timeout(
                provider.identity().limits.deadline,
                provider.compact(request.clone()),
            )
            .await
            {
                Ok(response) => response,
                Err(_) => Err(ProviderError::CallTimedOut),
            };
            if !response
                .as_ref()
                .is_err_and(|error| error.allows_fallback())
                || !self
                    .switch(
                        (request.session_id, request.covered_run_id, actor),
                        ProviderRole::Compaction,
                        provider.identity(),
                        if response.as_ref().is_err_and(|error| {
                            error.failure_class() == crate::provider::ProviderFailureClass::Rejected
                        }) {
                            crate::session::ProviderCallDisposition::Rejected
                        } else {
                            crate::session::ProviderCallDisposition::Unavailable
                        },
                    )
                    .await?
            {
                return Ok((response, provider));
            }
        }
    }

    async fn switch(
        &self,
        scope: (crate::session::SessionId, crate::session::RunId, AgentRunId),
        role: ProviderRole,
        from: &ProviderIdentity,
        failure: crate::session::ProviderCallDisposition,
    ) -> Result<bool, EngineError> {
        let (session_id, run_id, agent_run_id) = scope;
        let chain = self.providers.get(role);
        let Some(to) = chain
            .iter()
            .position(|provider| provider.identity() == from)
            .and_then(|index| chain.get(index + 1))
        else {
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
                failure,
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
                    .as_ref()
                    .is_some_and(ProgressPublisher::previews_enabled),
                steering: false,
            },
            ProviderRole::Child => Action::Child {
                in_flight: self.ledger.in_flight_children(),
            },
            ProviderRole::Review => Action::Review,
            ProviderRole::Compaction => Action::Compaction,
        };
        self.store
            .admit_operation(
                session_id,
                action.max_events() + 1,
                action.tool_observations(),
            )
            .await?;
        append_scoped(
            &self.store,
            RunScope {
                ledger: &self.ledger,
                publisher: self.publisher.as_ref(),
            },
            None,
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
        self.provider.invoke(request).await
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
