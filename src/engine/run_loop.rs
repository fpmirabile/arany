use super::progress::{ProgressPublisher, append_observed};
use super::{EngineError, OUTPUT_TOKEN_CAP, RunCancellation};
use crate::provider::{
    AgentPhase, Finish, MAX_REPORTED_INPUT_TOKENS, Provider, ProviderFailureClass,
    ProviderFailureReason, ProviderOutcome, ProviderRequest, ProviderResponse,
};
use crate::session::{
    AgentDisposition, AgentRunId, CollaborationPolicy, Event,
    MAX_ASSISTANT_MESSAGE_BYTES as MAX_PRIMARY_RESULT_BYTES, ProviderCallDisposition,
    ProviderCallRecord, RunDisposition, RunId, SessionId,
};
use crate::store::Store;
use crate::telemetry::{AgentTrace, ProviderTraceOutcome, RunTrace, TraceFailure, TraceOutcome};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

mod children;

const MAX_SUMMARY_BYTES: usize = 2 * 1024;
const CALL_DEADLINE: Duration = Duration::from_secs(120);
const RUN_DEADLINE: Duration = Duration::from_secs(300);

enum CallResult {
    Response(ProviderResponse),
    Failed(CallFailure),
    Cancelled,
}

enum CallFailure {
    Timeout,
    Unavailable(Option<ProviderFailureReason>),
    Rejected(Option<ProviderFailureReason>),
    InvalidResponse(Option<ProviderFailureReason>),
    OutputLimit,
    TaskPanic,
}

pub(super) struct RunLoop<'a, P: Provider> {
    store: &'a Store,
    provider: Arc<P>,
    session_id: SessionId,
    run_id: RunId,
    primary_id: AgentRunId,
    controls: RunControls<'a>,
    progress: Option<&'a ProgressPublisher>,
    trace: &'a RunTrace,
}

pub(super) struct RunControls<'a> {
    pub(super) policy: CollaborationPolicy,
    pub(super) concurrency: u8,
    pub(super) cancellation: RunCancellation,
    pub(super) tools: Option<&'a crate::tools::ToolRuntime>,
}

pub(super) struct RunIdentity {
    pub(super) session_id: SessionId,
    pub(super) run_id: RunId,
    pub(super) primary_id: AgentRunId,
}

impl<'a, P: Provider + 'static> RunLoop<'a, P> {
    pub(super) fn new(
        store: &'a Store,
        provider: Arc<P>,
        identity: RunIdentity,
        controls: RunControls<'a>,
        progress: Option<&'a ProgressPublisher>,
        trace: &'a RunTrace,
    ) -> Self {
        Self {
            store,
            provider,
            session_id: identity.session_id,
            run_id: identity.run_id,
            primary_id: identity.primary_id,
            controls,
            progress,
            trace,
        }
    }

    async fn append(&self, event: Event) -> Result<(), EngineError> {
        let _ = append_observed(
            self.store,
            self.progress,
            Some(self.trace),
            self.session_id,
            event,
        )
        .await?;
        Ok(())
    }

    pub(super) async fn execute(
        &self,
        mut root_request: ProviderRequest,
    ) -> Result<(), EngineError> {
        let deadline = Instant::now() + RUN_DEADLINE;
        let primary = self.trace.begin_primary(self.primary_id);
        for step in 0..=crate::tools::MAX_TOOL_CALLS {
            let first_trace = (step == 0).then(|| primary.begin_first_provider());
            let next_trace = (step != 0)
                .then(|| primary.begin_provider(AgentPhase::RootPlan, &root_request.model));
            let first = invoke_bounded(
                self.provider.as_ref(),
                root_request.clone(),
                deadline,
                self.controls.cancellation.clone(),
            )
            .await;
            let (input_tokens, output_tokens) = call_trace_usage(&first);
            let valid_response = match &first {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) => {
                    !matches!(self.controls.policy, CollaborationPolicy::Team { .. })
                        && valid_finish(finish, MAX_PRIMARY_RESULT_BYTES)
                }
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Delegate(delegation),
                    ..
                }) => self.valid_delegation(&delegation.children),
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tool(call),
                    ..
                }) => self.valid_tool(&root_request, call),
                _ => false,
            };
            if let Some(first_trace) = first_trace {
                first_trace.finish(
                    &primary,
                    &root_request.model,
                    matches!(
                        &first,
                        CallResult::Response(ProviderResponse {
                            outcome: ProviderOutcome::Delegate(_),
                            ..
                        })
                    ),
                    call_trace_result(&first, valid_response),
                    input_tokens,
                    output_tokens,
                );
            }
            if let Some(next_trace) = next_trace {
                next_trace.finish_with_usage(
                    call_trace_result(&first, valid_response),
                    input_tokens,
                    output_tokens,
                );
            }
            self.append(Event::ProviderCallRecorded {
                run_id: self.run_id,
                agent_run_id: self.primary_id,
                record: call_record(AgentPhase::RootPlan, &first, valid_response),
            })
            .await?;
            match first {
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Finish(finish),
                    ..
                }) if valid_response => return self.finish_primary(finish, primary).await,
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Delegate(delegation),
                    ..
                }) if valid_response => {
                    return self
                        .run_children(&mut root_request, delegation.children, deadline, primary)
                        .await;
                }
                CallResult::Response(ProviderResponse {
                    outcome: ProviderOutcome::Tool(call),
                    ..
                }) if valid_response => {
                    if !self.execute_tool(&mut root_request, call, deadline).await? {
                        return self.fail_primary(primary).await;
                    }
                }
                CallResult::Cancelled => return self.cancel_primary(primary).await,
                _ => return self.fail_primary(primary).await,
            }
        }
        self.fail_primary(primary).await
    }

    fn valid_tool(&self, request: &ProviderRequest, call: &crate::tools::ToolCall) -> bool {
        self.controls.tools.is_some()
            && call.valid()
            && request
                .tools
                .as_ref()
                .is_some_and(|tools| tools.observations.len() < crate::tools::MAX_TOOL_CALLS)
    }

    async fn execute_tool(
        &self,
        request: &mut ProviderRequest,
        call: crate::tools::ToolCall,
        deadline: Instant,
    ) -> Result<bool, EngineError> {
        let runtime = self.controls.tools.ok_or(EngineError::InvalidRequest)?;
        let mut intent = runtime.intent(self.run_id, self.primary_id, call);
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(Duration::from_secs(15));
        if remaining < Duration::from_secs(1) {
            return Ok(false);
        }
        intent.limits.runtime_ms = remaining.as_millis().min(60_000) as u32;
        intent.expires_at_ms =
            crate::tools::now_ms().saturating_add(u64::from(intent.limits.runtime_ms));
        let mut reserved = request.tools.clone().ok_or(EngineError::InvalidRequest)?;
        reserved.observations.push(crate::tools::ToolObservation {
            intent: intent.clone(),
            disposition: crate::tools::ToolDisposition::Succeeded,
            output: String::new(),
            guard: Some(crate::tools::GuardReceipt {
                contract_version: 1,
                intent_digest: intent.digest(),
                enforcement_digest: intent.enforcement_digest,
                limits: intent.limits.clone(),
            }),
        });
        let overhead = serde_json::to_vec(&reserved)
            .map_err(|_| EngineError::InvalidRequest)?
            .len();
        let capacity =
            crate::tools::MAX_TOOL_CONTEXT_BYTES.saturating_sub(overhead.saturating_add(512)) / 6;
        if capacity < 128 {
            return Ok(false);
        }
        intent.limits.result_bytes =
            capacity.min(crate::tools::types::MAX_TOOL_RESULT_BYTES) as u32;
        self.append(Event::ToolStarted {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            intent: intent.clone(),
        })
        .await?;
        let observation = runtime
            .execute(intent, self.controls.cancellation.clone())
            .await;
        self.append(Event::ToolFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            observation: observation.clone(),
        })
        .await?;
        if matches!(
            observation.disposition,
            crate::tools::ToolDisposition::Uncertain | crate::tools::ToolDisposition::Cancelled
        ) {
            return Ok(false);
        }
        let tools = request.tools.as_mut().ok_or(EngineError::InvalidRequest)?;
        tools.observations.push(observation);
        if !serde_json::to_vec(tools)
            .is_ok_and(|bytes| bytes.len() <= crate::tools::MAX_TOOL_CONTEXT_BYTES)
        {
            return Ok(false);
        }
        Ok(true)
    }

    async fn finish_primary(&self, finish: Finish, primary: AgentTrace) -> Result<(), EngineError> {
        if self.controls.cancellation.is_cancelled() {
            return self.cancel_primary(primary).await;
        }
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::Finished,
            summary: Some(finish.summary),
            result: Some(finish.result.clone()),
        })
        .await?;
        self.append(Event::MessageCommitted {
            run_id: self.run_id,
            text: finish.result,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::Finished,
        })
        .await?;
        primary.finish(TraceOutcome::Finished);
        Ok(())
    }

    async fn fail_primary(&self, primary: AgentTrace) -> Result<(), EngineError> {
        if self.controls.cancellation.is_cancelled() {
            return self.cancel_primary(primary).await;
        }
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::Failed,
            summary: None,
            result: None,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::Failed,
        })
        .await?;
        primary.finish(TraceOutcome::Failed);
        Ok(())
    }

    async fn cancel_primary(&self, primary: AgentTrace) -> Result<(), EngineError> {
        self.append(Event::AgentFinished {
            run_id: self.run_id,
            agent_run_id: self.primary_id,
            disposition: AgentDisposition::Cancelled,
            summary: None,
            result: None,
        })
        .await?;
        self.append(Event::RunFinished {
            run_id: self.run_id,
            disposition: RunDisposition::Cancelled,
        })
        .await?;
        primary.finish(TraceOutcome::Cancelled);
        Ok(())
    }
}

async fn invoke_bounded<P: Provider>(
    provider: &P,
    request: ProviderRequest,
    run_deadline: Instant,
    mut cancellation: RunCancellation,
) -> CallResult {
    if cancellation.is_cancelled() {
        return CallResult::Cancelled;
    }
    let remaining = run_deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return CallResult::Failed(CallFailure::Timeout);
    }
    let timeout = CALL_DEADLINE.min(remaining);
    tokio::select! {
        biased;
        () = cancellation.cancelled() => CallResult::Cancelled,
        result = tokio::time::timeout(timeout, provider.invoke(request)) => match result {
            Ok(Ok(response)) if response.input_tokens.is_some_and(|count| count > MAX_REPORTED_INPUT_TOKENS) => {
                CallResult::Failed(CallFailure::InvalidResponse(None))
            }
            Ok(Ok(response)) if response.output_tokens.is_some_and(|count| count > OUTPUT_TOKEN_CAP) => {
                CallResult::Failed(CallFailure::OutputLimit)
            }
            Ok(Ok(response)) if response.response_id.as_ref().is_some_and(|id| {
                id.is_empty() || id.len() > 128 || !id.bytes().all(|byte| byte.is_ascii_graphic())
            }) => CallResult::Failed(CallFailure::InvalidResponse(None)),
            Ok(Ok(response)) if response.wire_provenance.is_some_and(|provenance| {
                !provenance.valid_for(
                    response.response_id.as_deref(),
                    response.input_tokens,
                    response.output_tokens,
                )
            }) => CallResult::Failed(CallFailure::InvalidResponse(None)),
            Ok(Ok(response)) => CallResult::Response(response),
            Ok(Err(error)) => match error.failure_class() {
                ProviderFailureClass::Unavailable => CallResult::Failed(CallFailure::Unavailable(error.failure_reason())),
                ProviderFailureClass::Rejected => CallResult::Failed(CallFailure::Rejected(error.failure_reason())),
                ProviderFailureClass::InvalidOutcome => {
                    CallResult::Failed(CallFailure::InvalidResponse(error.failure_reason()))
                }
            },
            Err(_) => CallResult::Failed(CallFailure::Timeout),
        }
    }
}

fn call_trace_result(result: &CallResult, valid_response: bool) -> ProviderTraceOutcome {
    match result {
        CallResult::Response(_) if valid_response => ProviderTraceOutcome::Finished,
        CallResult::Response(_) => ProviderTraceOutcome::Failed(TraceFailure::InvalidResponse),
        CallResult::Failed(failure) => ProviderTraceOutcome::Failed(match failure {
            CallFailure::Timeout => TraceFailure::Timeout,
            CallFailure::Unavailable(_) => TraceFailure::Unavailable,
            CallFailure::Rejected(_) => TraceFailure::Rejected,
            CallFailure::InvalidResponse(_) => TraceFailure::InvalidResponse,
            CallFailure::OutputLimit => TraceFailure::OutputLimit,
            CallFailure::TaskPanic => TraceFailure::TaskPanic,
        }),
        CallResult::Cancelled => ProviderTraceOutcome::Cancelled,
    }
}

fn call_trace_usage(result: &CallResult) -> (Option<u32>, Option<u32>) {
    match result {
        CallResult::Response(response) => (response.input_tokens, response.output_tokens),
        CallResult::Failed(_) | CallResult::Cancelled => (None, None),
    }
}

fn call_record(phase: AgentPhase, result: &CallResult, valid_response: bool) -> ProviderCallRecord {
    let disposition = match result {
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Finish(_),
            ..
        }) if valid_response => ProviderCallDisposition::Finished,
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Delegate(_),
            ..
        }) if valid_response => ProviderCallDisposition::Delegated,
        CallResult::Response(ProviderResponse {
            outcome: ProviderOutcome::Tool(_),
            ..
        }) if valid_response => ProviderCallDisposition::ToolRequested,
        CallResult::Response(_) => ProviderCallDisposition::InvalidResponse,
        CallResult::Failed(failure) => match failure {
            CallFailure::Timeout => ProviderCallDisposition::TimedOut,
            CallFailure::Unavailable(_) => ProviderCallDisposition::Unavailable,
            CallFailure::Rejected(_) => ProviderCallDisposition::Rejected,
            CallFailure::InvalidResponse(_) => ProviderCallDisposition::InvalidResponse,
            CallFailure::OutputLimit => ProviderCallDisposition::OutputLimit,
            CallFailure::TaskPanic => ProviderCallDisposition::TaskPanic,
        },
        CallResult::Cancelled => ProviderCallDisposition::Cancelled,
    };
    let (response_id, input_tokens, output_tokens, wire_provenance) = match result {
        CallResult::Response(response) => (
            response.response_id.clone(),
            response.input_tokens,
            response.output_tokens,
            response.wire_provenance,
        ),
        CallResult::Failed(_) | CallResult::Cancelled => (None, None, None, None),
    };
    ProviderCallRecord {
        phase,
        disposition,
        response_id,
        input_tokens,
        output_tokens,
        wire_provenance,
        failure_reason: match result {
            CallResult::Failed(
                CallFailure::Unavailable(reason)
                | CallFailure::Rejected(reason)
                | CallFailure::InvalidResponse(reason),
            ) => *reason,
            _ => None,
        },
    }
}

fn valid_finish(finish: &Finish, result_limit: usize) -> bool {
    !finish.summary.is_empty()
        && finish.summary.len() <= MAX_SUMMARY_BYTES
        && !finish.result.is_empty()
        && finish.result.len() <= result_limit
        && Event::finished_payload_size(&finish.summary, &finish.result)
            .is_ok_and(|bytes| bytes <= crate::store::MAX_EVENT_BYTES)
}
