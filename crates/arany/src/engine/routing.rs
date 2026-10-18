use super::hints::HintPublisher;
use super::ledger::{Action, GoalLedger};
use super::run_loop::{CallFailure, CallResult, invoke_bounded_with_preview};
use super::{EngineError, RunCancellation};
use crate::journal::Journal;
use crate::provider::{
    AdmittedProviders, MAX_PROVIDER_SWITCHES, Provider, ProviderError, ProviderFallback,
    ProviderIdentity, ProviderRequest, ProviderRole, ResponsePreview,
};
use crate::session::{AgentRunId, Event, RunId, SessionId};
use crate::store::StoreError;
use std::sync::Arc;
use tokio::time::Instant;

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
        cancellation: RunCancellation,
        preview: Option<ResponsePreview>,
    ) -> Result<CallResult, EngineError> {
        let role = ProviderRole::for_phase(request.phase);
        if self.chain(role, request.agent_run_id)?.len() == 1 {
            let provider = self.bind(
                role,
                request.agent_run_id,
                &mut request.model,
                &mut request.max_output_tokens,
            )?;
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
                        CallScope {
                            session_id: request.session_id,
                            run_id: request.run_id,
                            agent_run_id: request.agent_run_id,
                        },
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
            if !response
                .as_ref()
                .is_err_and(|error| error.allows_fallback())
                || Instant::now() >= deadline
                || !self
                    .switch(
                        CallScope {
                            session_id: request.session_id,
                            run_id: request.covered_run_id,
                            agent_run_id: actor,
                        },
                        ProviderRole::Compaction,
                        provider.identity(),
                        None,
                    )
                    .await?
            {
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
            session_id,
            run_id,
            agent_run_id,
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
        // A refused admission declines the fallback, so the known failure settles as its own outcome.
        match self
            .journal
            .admit(
                session_id,
                action.max_events_for_admitted(&self.providers) + 1,
                action.tool_observations(),
            )
            .await
        {
            Ok(()) => {}
            Err(StoreError::ReplayLimit | StoreError::StorageFull) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
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
    let result = invoke_bounded_with_preview(
        provider,
        request,
        deadline,
        cancellation.clone(),
        preview.clone(),
    )
    .await;
    AttemptResult {
        eligible: matches!(
            result,
            CallResult::Failed(CallFailure::Unavailable { fallback: true, .. })
        ) && preview.as_ref().is_none_or(|preview| !preview.has_output())
            && !cancellation.is_cancelled()
            && Instant::now() < deadline,
        result,
    }
}
