use super::{Engine, EngineError, ensure_session_workspace};
use crate::provider::{
    CompactionRequest, CompactionResponse, MAX_REPORTED_INPUT_TOKENS, Provider,
    ProviderFailureClass,
};
use crate::session::input::WorkspaceInputs;
use crate::session::{
    CompactionFailure, CompactionInputError, CompactionRecord, CompactionStatus, Event,
    MAX_COMPACTION_OUTPUT_TOKENS, PreparedCompaction, RunStatus, SessionId, prefix_digest,
};
use crate::telemetry::{ProviderTraceOutcome, TraceFailure, TraceOutcome, TraceProvider};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;

const COMPACTION_CALL_DEADLINE: Duration = Duration::from_secs(120);
const MAX_SUMMARY_BYTES: usize = 8 * 1024;

impl<P: Provider + 'static> Engine<P> {
    /// Returns a committed success or failure record; admission and storage errors remain errors.
    pub async fn compact_session(
        &mut self,
        session_id: SessionId,
        workspace: PathBuf,
    ) -> Result<CompactionRecord, EngineError> {
        self.compact_session_inner(session_id, workspace, None)
            .await?
            .ok_or(EngineError::CompactionInputEmpty)
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
        if events
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::ContextCompacted { .. }))
            .count()
            >= crate::session::MAX_COMPACTIONS
        {
            return Err(EngineError::CompactionLimit);
        }
        self.store.admit_operation(session_id, 1).await?;
        let source_digest =
            prefix_digest(&events, covered_sequence).map_err(|_| EngineError::InvalidHistory)?;
        let prepared = PreparedCompaction::prepare(&view).map_err(|error| match error {
            CompactionInputError::Empty => EngineError::CompactionInputEmpty,
            CompactionInputError::TooLarge => EngineError::CompactionInputTooLarge,
        })?;
        let source_bytes = prepared.source_bytes;
        let compiler_version = prepared.compiler_version;
        let trace = self.telemetry.begin_compaction(
            session_id,
            covered_run_id,
            TraceProvider::from_profile(&selection.name),
        );
        let request = CompactionRequest {
            session_id,
            covered_run_id,
            model: selection.model.clone(),
            previous_summary: prepared.previous_summary,
            items: prepared.items,
            max_output_tokens: MAX_COMPACTION_OUTPUT_TOKENS,
        };
        let provider_trace = trace.begin_provider(&selection.model);
        let response =
            tokio::time::timeout(COMPACTION_CALL_DEADLINE, self.provider.compact(request)).await;
        let (status, response_id, input_tokens, output_tokens, wire_provenance) = match response {
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
                None,
                None,
                None,
            ),
            Err(_) => (
                CompactionStatus::Failed {
                    reason: CompactionFailure::TimedOut,
                },
                None,
                None,
                None,
                None,
            ),
        };
        let trace_outcome = if matches!(status, CompactionStatus::Succeeded { .. }) {
            TraceOutcome::Finished
        } else {
            TraceOutcome::Failed
        };
        let provider_outcome = match &status {
            CompactionStatus::Succeeded { .. } => ProviderTraceOutcome::Finished,
            CompactionStatus::Failed { reason } => ProviderTraceOutcome::Failed(match reason {
                CompactionFailure::ProviderUnavailable => TraceFailure::Unavailable,
                CompactionFailure::ProviderRejected => TraceFailure::Rejected,
                CompactionFailure::InvalidOutcome => TraceFailure::InvalidResponse,
                CompactionFailure::TimedOut => TraceFailure::Timeout,
            }),
        };
        provider_trace.finish_with_usage(provider_outcome, input_tokens, output_tokens);
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
            input_tokens,
            output_tokens,
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
        trace.event_committed(committed.sequence);
        trace.finish(trace_outcome);
        Ok(Some(record))
    }
}

fn classify_response(
    response: CompactionResponse,
) -> (
    CompactionStatus,
    Option<String>,
    Option<u32>,
    Option<u32>,
    Option<crate::provider::ProviderWireProvenance>,
) {
    let valid_id = response.response_id.as_ref().is_none_or(|value| {
        !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
    });
    let valid_input_tokens = response
        .input_tokens
        .is_none_or(|count| count <= MAX_REPORTED_INPUT_TOKENS);
    let valid_output_tokens = response
        .output_tokens
        .is_none_or(|count| count <= 1_000_000);
    let valid_summary = !response.summary.is_empty()
        && response.summary.len() <= MAX_SUMMARY_BYTES
        && response
            .output_tokens
            .is_none_or(|count| count <= MAX_COMPACTION_OUTPUT_TOKENS);
    let valid_provenance = response.wire_provenance.is_none_or(|provenance| {
        provenance.valid_for(
            response.response_id.as_deref(),
            response.input_tokens,
            response.output_tokens,
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
    (
        status,
        response.response_id.filter(|_| valid_id),
        response.input_tokens.filter(|_| valid_input_tokens),
        response.output_tokens.filter(|_| valid_output_tokens),
        response
            .wire_provenance
            .filter(|_| valid_id && valid_input_tokens && valid_output_tokens && valid_provenance),
    )
}
