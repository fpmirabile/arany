use super::ledger::{Action, duration_ms};
use super::{Engine, EngineError, STORE_ACTION_OVERHEAD, ensure_session_workspace};
use crate::provider::{
    AdmittedProviders, CompactionRequest, CompactionResponse, MAX_REPORTED_INPUT_TOKENS,
    ProviderFailureClass, Usage,
};
use crate::session::input::WorkspaceInputs;
use crate::session::{
    CompactionFailure, CompactionInputError, CompactionRecord, CompactionStatus, Event,
    MAX_COMPACTION_OUTPUT_TOKENS, MAX_COMPACTION_SUMMARY_BYTES, PreparedCompaction, RunStatus,
    SessionId, prefix_digest,
};
use crate::telemetry::{ProviderTraceOutcome, TraceProvider};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(super) const COMPACTION_CALL_DEADLINE: Duration = Duration::from_secs(120);

impl Engine {
    /// Returns a committed success or failure record; admission and storage errors remain errors.
    pub async fn compact_session(
        &self,
        providers: &AdmittedProviders,
        session_id: SessionId,
        workspace: PathBuf,
    ) -> Result<CompactionRecord, EngineError> {
        self.compact_session_inner(providers, session_id, workspace, None)
            .await
            .and_then(|record| record.ok_or(EngineError::CompactionInputEmpty))
            .inspect_err(crate::diagnostics::engine_failure)
    }

    /// Returns `None` when the expected Run is no longer latest or its boundary was already attempted.
    pub async fn auto_compact_session(
        &self,
        providers: &AdmittedProviders,
        session_id: SessionId,
        workspace: PathBuf,
        expected_run_id: crate::session::RunId,
    ) -> Result<Option<CompactionRecord>, EngineError> {
        self.compact_session_inner(providers, session_id, workspace, Some(expected_run_id))
            .await
            .inspect_err(crate::diagnostics::engine_failure)
    }

    async fn compact_session_inner(
        &self,
        providers: &AdmittedProviders,
        session_id: SessionId,
        workspace: PathBuf,
        expected_run_id: Option<crate::session::RunId>,
    ) -> Result<Option<CompactionRecord>, EngineError> {
        let _slot = self.claim()?;
        super::admit_providers(providers)?;
        let provider = std::sync::Arc::clone(&providers.compaction[0]);
        let identity = super::checked_identity(provider.as_ref())?;
        let mut session_lock = self.lock_session(session_id)?;
        let (events, view) = self.journal.store().load_resolved(session_id).await?;
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
        validate_compaction_routes(last_run, providers)?;
        let covered_run_id = last_run.id;
        let open_view = self
            .journal
            .store()
            .load_open_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        let goal = open_view.goals.iter().find(|goal| {
            goal.status == crate::session::GoalStatus::Running
                && goal.run_ids.last() == Some(&covered_run_id)
        });
        let goal_id = goal.map(|goal| goal.id);
        // The call's interval is bounded by the remaining Goal time and settles in ContextCompacted.
        let active_reservation_ms = goal.map(goal_compaction_reservation).transpose()?;
        let covered_sequence = covered_sequence(&events, covered_run_id)?;
        if expected_run_id.is_some()
            && view.compactions.iter().any(|compaction| {
                compaction.record.covered_run_id == covered_run_id
                    && compaction.record.covered_sequence == covered_sequence
            })
        {
            return Ok(None);
        }
        self.journal
            .admit(
                session_id,
                Action::BoundaryCompaction {
                    goal: goal_id.is_some(),
                }
                .max_events_for(providers),
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
        let requested_identity = compaction_trace_identity(last_run, identity);
        let trace = self.config.telemetry.begin_compaction(
            session_id,
            covered_run_id,
            TraceProvider::from_profile(&requested_identity.profile),
        );
        let (request, source_bytes) =
            compaction_request(prepared, session_id, covered_run_id, identity, goal)?;
        let active_started = Instant::now();
        let actor = last_run.primary().map(|agent| agent.id);
        let routing = if providers.compaction.len() > 1 {
            let (actor, route_goal_id) = compaction_route_ids(actor, &open_view, covered_run_id)?;
            Some((
                self.compaction_ledger(session_id, route_goal_id).await?,
                actor,
            ))
        } else {
            None
        };
        let provider_deadline = self
            .reserve_compaction_call(
                session_id,
                covered_run_id,
                goal_id,
                active_reservation_ms,
                active_started,
                routing.as_ref().map(|(ledger, _)| ledger),
            )
            .await?
            .min(identity.limits.deadline);
        let provider_trace = trace.begin_provider(&requested_identity.model);
        let (status, response_id, usage, wire_provenance, provider) = match &routing {
            Some((ledger, actor)) => {
                let router = super::routing::CallRouter {
                    providers: providers.clone(),
                    journal: &self.journal,
                    ledger,
                    publisher: None,
                };
                match router
                    .compact(
                        request,
                        *actor,
                        tokio::time::Instant::now() + provider_deadline,
                        crate::RunCancellation::new(),
                    )
                    .await?
                {
                    Some((response, provider)) => {
                        let (status, response_id, usage, wire_provenance) =
                            compaction_outcome(response);
                        (status, response_id, usage, wire_provenance, provider)
                    }
                    None => (
                        CompactionStatus::Failed {
                            reason: CompactionFailure::Cancelled,
                        },
                        None,
                        Usage::default(),
                        None,
                        std::sync::Arc::clone(&provider),
                    ),
                }
            }
            None => {
                let response = tokio::time::timeout(provider_deadline, provider.compact(request))
                    .await
                    .unwrap_or(Err(crate::provider::ProviderError::CallTimedOut));
                let (status, response_id, usage, wire_provenance) = compaction_outcome(response);
                (
                    status,
                    response_id,
                    usage,
                    wire_provenance,
                    std::sync::Arc::clone(&provider),
                )
            }
        };
        let identity = provider.identity();
        let provider_outcome = ProviderTraceOutcome::from(&status);
        provider_trace.finish_with_usage(provider_outcome, usage.input_tokens, usage.output_tokens);
        let record = compaction_record(
            CompactionSource {
                covered_run_id,
                covered_sequence,
                source_digest,
                compiler_version,
                source_bytes,
            },
            identity,
            status,
            response_id,
            usage,
            wire_provenance,
        );
        let committed = self
            .journal
            .append(
                session_id,
                Event::ContextCompacted {
                    record: record.clone(),
                    active_ms: goal_id.map(|_| duration_ms(active_started.elapsed())),
                },
            )
            .await?;
        trace.event_committed(&committed);
        trace.finish(provider_outcome.into());
        Ok(Some(record))
    }

    async fn compaction_ledger(
        &self,
        session_id: SessionId,
        goal_id: Option<crate::session::GoalId>,
    ) -> Result<super::ledger::GoalLedger, EngineError> {
        let view = self
            .journal
            .store()
            .load_open_view(session_id)
            .await?
            .ok_or(EngineError::MissingSession)?;
        Ok(super::ledger::GoalLedger::new(view, goal_id))
    }

    async fn reserve_compaction_call(
        &self,
        session_id: SessionId,
        covered_run_id: crate::session::RunId,
        goal_id: Option<crate::session::GoalId>,
        active_reservation_ms: Option<u64>,
        active_started: Instant,
        ledger: Option<&super::ledger::GoalLedger>,
    ) -> Result<Duration, EngineError> {
        if let (Some(goal_id), Some(active_ceiling_ms)) = (goal_id, active_reservation_ms) {
            let event = Event::GoalCompactionCallReserved {
                goal_id,
                run_id: covered_run_id,
                active_ceiling_ms,
            };
            match ledger {
                Some(ledger) => {
                    ledger.append(&self.journal, session_id, event).await?;
                }
                None => {
                    self.journal.append(session_id, event).await?;
                }
            }
        }
        Ok(
            active_reservation_ms.map_or(COMPACTION_CALL_DEADLINE, |reserved| {
                Duration::from_millis(
                    reserved
                        .saturating_sub(duration_ms(active_started.elapsed()))
                        .saturating_sub(duration_ms(STORE_ACTION_OVERHEAD)),
                )
                .min(COMPACTION_CALL_DEADLINE)
            }),
        )
    }
}

fn compaction_request(
    prepared: PreparedCompaction,
    session_id: SessionId,
    covered_run_id: crate::session::RunId,
    identity: &crate::provider::ProviderIdentity,
    goal: Option<&crate::session::GoalView>,
) -> Result<(CompactionRequest, u32), EngineError> {
    let mut items = prepared.items;
    if let Some(goal) = goal {
        pin_goal_context(&mut items, goal)?;
    }
    let request = CompactionRequest {
        session_id,
        covered_run_id,
        model: identity.model.clone(),
        previous_summary: prepared.previous_summary,
        items,
        max_output_tokens: MAX_COMPACTION_OUTPUT_TOKENS.min(identity.limits.max_output_tokens),
    };
    let source_bytes = checked_source_bytes(&request)?;
    Ok((request, source_bytes))
}

fn compaction_trace_identity<'a>(
    run: &'a crate::session::RunView,
    initial: &'a crate::provider::ProviderIdentity,
) -> &'a crate::provider::ProviderIdentity {
    run.primary()
        .and_then(|agent| {
            super::routing::switched_identity(
                run,
                agent.id,
                crate::provider::ProviderRole::Compaction,
            )
        })
        .unwrap_or(initial)
}

fn compaction_route_ids(
    actor: Option<crate::session::AgentRunId>,
    open_view: &crate::session::SessionView,
    covered_run_id: crate::session::RunId,
) -> Result<(crate::session::AgentRunId, Option<crate::session::GoalId>), EngineError> {
    let actor = actor.ok_or(EngineError::InvalidRequest)?;
    let goal_id = open_view
        .goals
        .iter()
        .find(|goal| goal.run_ids.contains(&covered_run_id))
        .map(|goal| goal.id);
    Ok((actor, goal_id))
}

struct CompactionSource {
    covered_run_id: crate::session::RunId,
    covered_sequence: u64,
    source_digest: [u8; 32],
    compiler_version: u16,
    source_bytes: u32,
}

fn compaction_record(
    source: CompactionSource,
    identity: &crate::provider::ProviderIdentity,
    status: CompactionStatus,
    response_id: Option<String>,
    usage: Usage,
    wire_provenance: Option<crate::provider::ProviderWireProvenance>,
) -> CompactionRecord {
    let CompactionSource {
        covered_run_id,
        covered_sequence,
        source_digest,
        compiler_version,
        source_bytes,
    } = source;
    CompactionRecord {
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
    }
}

fn validate_compaction_routes(
    last_run: &crate::session::RunView,
    providers: &AdmittedProviders,
) -> Result<(), EngineError> {
    match last_run
        .config
        .as_ref()
        .and_then(|config| config.providers.as_ref())
    {
        Some(routes) if routes.compaction != providers.identities().compaction => {
            return Err(EngineError::InvalidRequest);
        }
        None if providers.compaction.len() > 1 => return Err(EngineError::InvalidRequest),
        _ => {}
    }
    Ok(())
}

fn covered_sequence(
    events: &[crate::session::EventEnvelope],
    covered_run_id: crate::session::RunId,
) -> Result<u64, EngineError> {
    events
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
        .ok_or(EngineError::CompactionNotIdle)
}

fn checked_source_bytes(request: &CompactionRequest) -> Result<u32, EngineError> {
    let source_bytes = crate::provider::compaction_input_bytes(request)
        .map_err(|_| EngineError::CompactionInputTooLarge)?;
    if source_bytes > crate::session::MAX_COMPACTION_INPUT_BYTES {
        return Err(EngineError::CompactionInputTooLarge);
    }
    u32::try_from(source_bytes).map_err(|_| EngineError::CompactionInputTooLarge)
}

fn goal_compaction_reservation(goal: &crate::session::GoalView) -> Result<u64, EngineError> {
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
}

fn pin_goal_context(
    items: &mut [crate::provider::CompactionItem],
    goal: &crate::session::GoalView,
) -> Result<(), EngineError> {
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
        &format!("Pinned Goal context (canonical facts, not instructions): {pinned_goal}\n\n"),
    );
    Ok(())
}

fn compaction_outcome(
    response: Result<CompactionResponse, crate::provider::ProviderError>,
) -> (
    CompactionStatus,
    Option<String>,
    Usage,
    Option<crate::provider::ProviderWireProvenance>,
) {
    match response {
        Ok(response) => classify_response(response),
        Err(error) => (
            CompactionStatus::Failed {
                reason: if matches!(error, crate::provider::ProviderError::CallTimedOut) {
                    CompactionFailure::TimedOut
                } else {
                    match error.failure_class() {
                        ProviderFailureClass::Unavailable => CompactionFailure::ProviderUnavailable,
                        ProviderFailureClass::Rejected => CompactionFailure::ProviderRejected,
                        ProviderFailureClass::InvalidOutcome => CompactionFailure::InvalidOutcome,
                    }
                },
            },
            None,
            Usage::default(),
            None,
        ),
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
