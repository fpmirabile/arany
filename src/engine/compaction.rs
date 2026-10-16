use super::{Engine, EngineError, ensure_session_workspace};
use crate::provider::{
    CompactionRequest, CompactionResponse, MAX_REPORTED_INPUT_TOKENS, ProviderFailureClass, Usage,
};
use crate::session::input::WorkspaceInputs;
use crate::session::{
    CompactionFailure, CompactionInputError, CompactionRecord, CompactionStatus, Event,
    MAX_COMPACTION_OUTPUT_TOKENS, PreparedCompaction, RunStatus, SessionId, prefix_digest,
};
use crate::telemetry::{ProviderTraceOutcome, TraceFailure, TraceOutcome, TraceProvider};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) const COMPACTION_CALL_DEADLINE: Duration = Duration::from_secs(120);
const MAX_SUMMARY_BYTES: usize = 8 * 1024;
const STORE_ACTION_OVERHEAD: Duration = Duration::from_secs(5);

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
        let selection = self.select_provider()?;
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
        let active_reservation_ms = goal
            .map(|goal| {
                if goal.active_run.is_some()
                    || goal.reserved_active_run.is_some()
                    || goal.reserved_active_ms.is_some()
                    || goal.compaction_call_reserved.is_some()
                {
                    return Err(EngineError::CompactionNotIdle);
                }
                let action_ms = duration_ms(COMPACTION_CALL_DEADLINE + STORE_ACTION_OVERHEAD);
                let active_limit = goal
                    .plan_contract
                    .as_ref()
                    .map_or(goal.budget.max_active_ms, |plan| plan.budget.max_active_ms);
                let remaining =
                    active_limit.map_or(action_ms, |limit| limit.saturating_sub(goal.active_ms));
                let reserved_ms = action_ms.min(remaining);
                let store_tail_ms = duration_ms(STORE_ACTION_OVERHEAD);
                if reserved_ms <= store_tail_ms
                    || goal.active_ms.checked_add(reserved_ms).is_none()
                    || active_limit
                        .is_some_and(|limit| goal.active_ms.saturating_add(reserved_ms) > limit)
                {
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
            .admit_operation(session_id, if goal_id.is_some() { 4 } else { 1 })
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
            TraceProvider::from_profile(&selection.name),
        );
        let mut items = prepared.items;
        if let Some(goal) = goal {
            let facts = crate::provider::GoalContextFacts {
                provider_calls: goal.provider_calls,
                known_tokens: goal.known_tokens,
                token_usage_unknown: goal.token_usage_unknown,
                active_ms: goal.active_ms,
                reserved_active_ms: goal.reserved_active_ms.unwrap_or_default(),
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
            model: selection.model.clone(),
            previous_summary: prepared.previous_summary,
            items,
            max_output_tokens: MAX_COMPACTION_OUTPUT_TOKENS,
        };
        let source_bytes = crate::provider::compaction_input_bytes(&request)
            .map_err(|_| EngineError::CompactionInputTooLarge)?;
        if source_bytes > crate::session::MAX_COMPACTION_INPUT_BYTES {
            return Err(EngineError::CompactionInputTooLarge);
        }
        let source_bytes =
            u32::try_from(source_bytes).map_err(|_| EngineError::CompactionInputTooLarge)?;
        let active_started = active_reservation_ms.map(|_| Instant::now());
        if let Some(goal_id) = goal_id {
            let reserved = self
                .store
                .append(
                    session_id,
                    Event::GoalCompactionCallReserved {
                        goal_id,
                        run_id: covered_run_id,
                    },
                )
                .await?;
            crate::diagnostics::event_failure(&reserved.event);
            let reserved = self
                .store
                .append(
                    session_id,
                    Event::GoalActiveTimeReserved {
                        goal_id,
                        run_id: covered_run_id,
                        reserved_ms: active_reservation_ms.ok_or(EngineError::InvalidHistory)?,
                    },
                )
                .await?;
            crate::diagnostics::event_failure(&reserved.event);
        }
        let provider_trace = trace.begin_provider(&selection.model);
        let provider_deadline =
            active_reservation_ms.map_or(COMPACTION_CALL_DEADLINE, |reserved| {
                let elapsed = active_started.map_or(0, |started| duration_ms(started.elapsed()));
                Duration::from_millis(
                    reserved
                        .saturating_sub(elapsed)
                        .saturating_sub(duration_ms(STORE_ACTION_OVERHEAD)),
                )
                .min(COMPACTION_CALL_DEADLINE)
            });
        let response =
            tokio::time::timeout(provider_deadline, self.provider.compact(request)).await;
        let (status, response_id, usage, wire_provenance) = match response {
            Ok(Ok(response)) => classify_response(response),
            Ok(Err(error)) => (
                CompactionStatus::Failed {
                    reason: match error.failure_class() {
                        ProviderFailureClass::Unavailable => CompactionFailure::ProviderUnavailable,
                        ProviderFailureClass::Rejected => CompactionFailure::ProviderRejected,
                        ProviderFailureClass::InvalidOutcome => CompactionFailure::InvalidOutcome,
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
            provider: selection.name,
            model: selection.model,
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
            chatgpt_provenance: selection.chatgpt_provenance,
            output_token_bound: selection.output_token_bound,
            status,
        };
        let committed = self
            .store
            .append(
                session_id,
                Event::ContextCompacted {
                    record: record.clone(),
                },
            )
            .await?;
        crate::diagnostics::event_failure(&committed.event);
        trace.event_committed(committed.sequence);
        if let (Some(goal_id), Some(started), Some(_reserved_ms)) =
            (goal_id, active_started, active_reservation_ms)
        {
            let active_ms = duration_ms(started.elapsed());
            if active_ms > crate::session::MAX_GOAL_ACTION_ACTIVE_MS {
                return Err(EngineError::GoalBudgetExhausted);
            }
            let settled = self
                .store
                .append(
                    session_id,
                    Event::GoalActiveTimeSettled {
                        goal_id,
                        run_id: covered_run_id,
                        active_ms,
                    },
                )
                .await?;
            crate::diagnostics::event_failure(&settled.event);
        }
        trace.finish(trace_outcome);
        Ok(Some(record))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
        && response.summary.len() <= MAX_SUMMARY_BYTES
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
