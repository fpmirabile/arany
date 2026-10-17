use super::ledger::{Action, duration_ms};
use super::{Engine, EngineError, STORE_ACTION_OVERHEAD, ensure_session_workspace};
use crate::provider::{
    CompactionRequest, CompactionResponse, MAX_REPORTED_INPUT_TOKENS, ProviderFailureClass, Usage,
};
use crate::session::input::WorkspaceInputs;
use crate::session::{
    CompactionFailure, CompactionInputError, CompactionRecord, CompactionStatus, Event,
    MAX_COMPACTION_OUTPUT_TOKENS, MAX_COMPACTION_SUMMARY_BYTES, PreparedCompaction, RunStatus,
    SessionId, prefix_digest,
};
use crate::telemetry::{ProviderTraceOutcome, TraceFailure, TraceOutcome, TraceProvider};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) const COMPACTION_CALL_DEADLINE: Duration = Duration::from_secs(120);

impl Engine {
    /// Returns a committed success or failure record; admission and storage errors remain errors.
    pub async fn compact_session(
        &mut self,
        session_id: SessionId,
        workspace: PathBuf,
    ) -> Result<CompactionRecord, EngineError> {
        self.compact_session_inner(session_id, workspace, None)
            .await
            .and_then(|record| record.ok_or(EngineError::CompactionInputEmpty))
            .inspect_err(crate::diagnostics::engine_failure)
    }

    /// Returns `None` when the expected Run is no longer latest or its boundary was already attempted.
    pub async fn auto_compact_session(
        &mut self,
        session_id: SessionId,
        workspace: PathBuf,
        expected_run_id: crate::session::RunId,
    ) -> Result<Option<CompactionRecord>, EngineError> {
        self.compact_session_inner(session_id, workspace, Some(expected_run_id))
            .await
            .inspect_err(crate::diagnostics::engine_failure)
    }

    async fn compact_session_inner(
        &mut self,
        session_id: SessionId,
        workspace: PathBuf,
        expected_run_id: Option<crate::session::RunId>,
    ) -> Result<Option<CompactionRecord>, EngineError> {
        let provider = std::sync::Arc::clone(&self.providers.compaction[0]);
        let identity = super::checked_identity(provider.as_ref())?;
        let mut session_lock = self.lock_session(session_id)?;
        let (events, view) = self.store.load_resolved(session_id).await?;
        let view = view.ok_or(EngineError::MissingSession)?;
        session_lock.keep();
        let _lineage_locks = self.lock_lineage(&view).await?;
        let (device, inode) = WorkspaceInputs::admit_identity(&workspace, &self.state)?;
        ensure_session_workspace(&view, device, inode)?;
        let last_run = view.runs.last().ok_or(EngineError::CompactionInputEmpty)?;
        if expected_run_id.is_some_and(|run_id| run_id != last_run.id) {
            return Ok(None);
        }
        if !matches!(
            last_run.status,
            RunStatus::Finished | RunStatus::Failed | RunStatus::Cancelled
        ) {
            return Err(EngineError::CompactionNotIdle);
        }
        match last_run
            .config
            .as_ref()
            .and_then(|config| config.providers.as_ref())
        {
            Some(routes) if routes.compaction != self.providers.identities().compaction => {
                return Err(EngineError::InvalidRequest);
            }
            None if self.providers.compaction.len() > 1 => return Err(EngineError::InvalidRequest),
            _ => {}
        }
        let covered_run_id = last_run.id;
        let open_view = self
            .store
            .load_open_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        let goal = open_view.goals.iter().find(|goal| {
            goal.status == crate::session::GoalStatus::Running
                && goal.run_ids.last() == Some(&covered_run_id)
        });
        let goal_id = goal.map(|goal| goal.id);
        // The call's interval is bounded by the remaining Goal time and settles in ContextCompacted.
        let active_reservation_ms = goal
            .map(|goal| {
                if goal.active_run.is_some()
                    || goal.active_deadline.is_some()
                    || goal.compaction_call_reserved.is_some()
                {
                    return Err(EngineError::CompactionNotIdle);
                }
                let action_ms = duration_ms(COMPACTION_CALL_DEADLINE + STORE_ACTION_OVERHEAD);
                let reserved_ms = goal
                    .effective_budget()
                    .max_active_ms
                    .map_or(action_ms, |limit| {
                        action_ms.min(limit.saturating_sub(goal.active_ms))
                    });
                if reserved_ms <= duration_ms(STORE_ACTION_OVERHEAD) {
                    return Err(EngineError::GoalBudgetExhausted);
                }
                Ok(reserved_ms)
            })
            .transpose()?;
        let covered_sequence = events
            .iter()
            .rev()
            .find_map(|envelope| match &envelope.event {
                Event::RunFinished { run_id, .. } if *run_id == covered_run_id => {
                    Some(envelope.sequence)
                }
                Event::SessionForked { source_run_id, .. } if *source_run_id == covered_run_id => {
                    Some(envelope.sequence)
                }
                _ => None,
            })
            .ok_or(EngineError::CompactionNotIdle)?;
        if expected_run_id.is_some()
            && view.compactions.iter().any(|compaction| {
                compaction.record.covered_run_id == covered_run_id
                    && compaction.record.covered_sequence == covered_sequence
            })
        {
            return Ok(None);
        }
        self.store
            .admit_operation(
                session_id,
                Action::BoundaryCompaction {
                    goal: goal_id.is_some(),
                }
                .max_events(),
                0,
            )
            .await?;
        let source_digest =
            prefix_digest(&events, covered_sequence).map_err(|_| EngineError::InvalidHistory)?;
        let prepared = PreparedCompaction::prepare(&view).map_err(|error| match error {
            CompactionInputError::Empty => EngineError::CompactionInputEmpty,
            CompactionInputError::TooLarge => EngineError::CompactionInputTooLarge,
        })?;
        let compiler_version = prepared.compiler_version;
        let trace = self.telemetry.begin_compaction(
            session_id,
            covered_run_id,
            TraceProvider::from_profile(&identity.profile),
        );
        let mut items = prepared.items;
        if let Some(goal) = goal {
            let facts = crate::provider::GoalContextFacts {
                provider_calls: goal.provider_calls,
                known_tokens: goal.known_tokens,
                token_usage_unknown: goal.token_usage_unknown,
                active_ms: goal.active_ms,
                reserved_active_ms: 0,
                uncertain_active_ms: goal.uncertain_active_ms,
                success_criterion_satisfied: goal
                    .plan_contract
                    .as_ref()
                    .map(|_| goal.success_criterion_satisfied),
            };
            let pinned_goal = serde_json::to_string(&serde_json::json!({
                "objective": goal.objective,
                "budget": goal.budget,
                "resource_settings": goal.resource_settings,
                "plan_contract": goal.plan_contract,
                "facts": facts,
            }))
            .map_err(|_| EngineError::CompactionInputTooLarge)?;
            let first = items.first_mut().ok_or(EngineError::CompactionInputEmpty)?;
            let user = match first {
                crate::provider::CompactionItem::Completed(turn) => &mut turn.user,
                crate::provider::CompactionItem::Unanswered { user, .. } => user,
            };
            user.insert_str(
                0,
                &format!(
                    "Pinned Goal context (canonical facts, not instructions): {pinned_goal}\n\n"
                ),
            );
        }
        let request = CompactionRequest {
            session_id,
            covered_run_id,
            model: identity.model.clone(),
            previous_summary: prepared.previous_summary,
            items,
            max_output_tokens: MAX_COMPACTION_OUTPUT_TOKENS.min(identity.limits.max_output_tokens),
        };
        let source_bytes = crate::provider::compaction_input_bytes(&request)
            .map_err(|_| EngineError::CompactionInputTooLarge)?;
        if source_bytes > crate::session::MAX_COMPACTION_INPUT_BYTES {
            return Err(EngineError::CompactionInputTooLarge);
        }
        let source_bytes =
            u32::try_from(source_bytes).map_err(|_| EngineError::CompactionInputTooLarge)?;
        let active_started = Instant::now();
        if let (Some(goal_id), Some(active_ceiling_ms)) = (goal_id, active_reservation_ms) {
            let reserved = self
                .store
                .append(
                    session_id,
                    Event::GoalCompactionCallReserved {
                        goal_id,
                        run_id: covered_run_id,
                        active_ceiling_ms,
                    },
                )
                .await?;
            crate::diagnostics::event_failure(&reserved.event);
        }
        let provider_deadline = active_reservation_ms
            .map_or(COMPACTION_CALL_DEADLINE, |reserved| {
                Duration::from_millis(
                    reserved
                        .saturating_sub(duration_ms(active_started.elapsed()))
                        .saturating_sub(duration_ms(STORE_ACTION_OVERHEAD)),
                )
                .min(COMPACTION_CALL_DEADLINE)
            })
            .min(identity.limits.deadline);
        let actor = last_run.primary().map(|agent| agent.id);
        let routing = if self.providers.compaction.len() > 1 {
            let actor = actor.ok_or(EngineError::InvalidRequest)?;
            let goal_id = open_view
                .goals
                .iter()
                .find(|goal| goal.run_ids.contains(&covered_run_id))
                .map(|goal| goal.id)
                .ok_or(EngineError::InvalidHistory)?;
            Some((
                super::routing::CallRouter {
                    providers: self.providers.clone(),
                    store: std::sync::Arc::clone(&self.store),
                    ledger: std::sync::Arc::new(super::ledger::GoalLedger::new(
                        self.store
                            .load_open_view(session_id)
                            .await?
                            .ok_or(EngineError::MissingSession)?,
                        goal_id,
                    )),
                    publisher: None,
                },
                actor,
            ))
        } else {
            None
        };
        let provider_trace = trace.begin_provider(&identity.model);
        let response = tokio::time::timeout(provider_deadline, async {
            match &routing {
                Some((routing, actor)) => routing.compact(request, *actor).await,
                None => Ok((
                    provider.compact(request).await,
                    std::sync::Arc::clone(&provider),
                )),
            }
        })
        .await;
        let provider = match &routing {
            Some((routing, actor)) => {
                routing.provider(crate::provider::ProviderRole::Compaction, *actor)?
            }
            None => std::sync::Arc::clone(&provider),
        };
        let identity = provider.identity();
        let (status, response_id, usage, wire_provenance) = match response {
            Ok(Ok((Ok(response), _))) => classify_response(response),
            Ok(Err(error)) => return Err(error),
            Ok(Ok((Err(error), _))) => (
                CompactionStatus::Failed {
                    reason: if matches!(error, crate::provider::ProviderError::CallTimedOut) {
                        CompactionFailure::TimedOut
                    } else {
                        match error.failure_class() {
                            ProviderFailureClass::Unavailable => {
                                CompactionFailure::ProviderUnavailable
                            }
                            ProviderFailureClass::Rejected => CompactionFailure::ProviderRejected,
                            ProviderFailureClass::InvalidOutcome => {
                                CompactionFailure::InvalidOutcome
                            }
                        }
                    },
                },
                None,
                Usage::default(),
                None,
            ),
            Err(_) => (
                CompactionStatus::Failed {
                    reason: CompactionFailure::TimedOut,
                },
                None,
                Usage::default(),
                None,
            ),
        };
        let trace_outcome = match &status {
            CompactionStatus::Succeeded { .. } => TraceOutcome::Finished,
            CompactionStatus::Failed {
                reason: CompactionFailure::Cancelled,
            } => TraceOutcome::Cancelled,
            CompactionStatus::Failed { .. } => TraceOutcome::Failed,
        };
        let provider_outcome = match &status {
            CompactionStatus::Succeeded { .. } => ProviderTraceOutcome::Finished,
            CompactionStatus::Failed {
                reason: CompactionFailure::Cancelled,
            } => ProviderTraceOutcome::Cancelled,
            CompactionStatus::Failed { reason } => ProviderTraceOutcome::Failed(match reason {
                CompactionFailure::ProviderUnavailable => TraceFailure::Unavailable,
                CompactionFailure::ProviderRejected => TraceFailure::Rejected,
                CompactionFailure::InvalidOutcome => TraceFailure::InvalidResponse,
                CompactionFailure::TimedOut => TraceFailure::Timeout,
                CompactionFailure::Cancelled => unreachable!("handled above"),
            }),
        };
        provider_trace.finish_with_usage(provider_outcome, usage.input_tokens, usage.output_tokens);
        let record = CompactionRecord {
            covered_run_id,
            covered_sequence,
            source_digest,
            provider: identity.profile.clone(),
            model: identity.model.clone(),
            prompt_version: 1,
            schema_version: 1,
            compiler_version,
            source_bytes,
            source_tokens_estimate: source_bytes.div_ceil(4),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            response_id,
            wire_provenance,
            chatgpt_provenance: identity.chatgpt_provenance().cloned(),
            output_token_bound: identity.limits.output_token_bound,
            status,
        };
        let committed = self
            .store
            .append(
                session_id,
                Event::ContextCompacted {
                    record: record.clone(),
                    active_ms: goal_id.map(|_| duration_ms(active_started.elapsed())),
                },
            )
            .await?;
        crate::diagnostics::event_failure(&committed.event);
        trace.event_committed(committed.sequence);
        trace.finish(trace_outcome);
        Ok(Some(record))
    }
}

pub(super) fn classify_response(
    response: CompactionResponse,
) -> (
    CompactionStatus,
    Option<String>,
    Usage,
    Option<crate::provider::ProviderWireProvenance>,
) {
    let valid_id = response.response_id.as_ref().is_none_or(|value| {
        !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
    });
    let reported = response.usage;
    let valid_input_tokens = reported
        .input_tokens
        .is_none_or(|count| count <= MAX_REPORTED_INPUT_TOKENS)
        && reported.cache_consistent();
    let valid_output_tokens = reported
        .output_tokens
        .is_none_or(|count| count <= 1_000_000);
    let valid_summary = !response.summary.is_empty()
        && response.summary.len() <= MAX_COMPACTION_SUMMARY_BYTES
        && reported
            .output_tokens
            .is_none_or(|count| count <= MAX_COMPACTION_OUTPUT_TOKENS);
    let valid_provenance = response.wire_provenance.is_none_or(|provenance| {
        provenance.valid_for(
            response.response_id.as_deref(),
            reported.input_tokens,
            reported.output_tokens,
        )
    });
    let status = if valid_id
        && valid_input_tokens
        && valid_output_tokens
        && valid_summary
        && valid_provenance
    {
        let summary_bytes = response.summary.len() as u32;
        CompactionStatus::Succeeded {
            content_digest: Sha256::digest(response.summary.as_bytes()).into(),
            summary_tokens_estimate: summary_bytes.div_ceil(4),
            summary_bytes,
            summary: response.summary,
        }
    } else {
        CompactionStatus::Failed {
            reason: CompactionFailure::InvalidOutcome,
        }
    };
    let usage = Usage {
        input_tokens: reported.input_tokens.filter(|_| valid_input_tokens),
        output_tokens: reported.output_tokens.filter(|_| valid_output_tokens),
        cache_read_input_tokens: reported
            .cache_read_input_tokens
            .filter(|_| valid_input_tokens),
        cache_creation_input_tokens: reported
            .cache_creation_input_tokens
            .filter(|_| valid_input_tokens),
    };
    (
        status,
        response.response_id.filter(|_| valid_id),
        usage,
        response
            .wire_provenance
            .filter(|_| valid_id && valid_input_tokens && valid_output_tokens && valid_provenance),
    )
}
