use crate::provider::{AgentPhase, MAX_REPORTED_INPUT_TOKENS};
use crate::session::{AgentRunId, RunId, SessionId};
use opentelemetry::{
    Context, KeyValue,
    trace::{SpanKind, Status, TraceContextExt, Tracer, TracerProvider},
};
use opentelemetry_otlp::{RetryPolicy, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::{
    Resource,
    trace::{BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider},
};
use std::{
    env,
    sync::Arc,
    time::{Duration, SystemTime},
};

mod config;
mod http;

const EXPORT_TIMEOUT: Duration = Duration::from_millis(500);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_MODEL_BYTES: usize = 256;
const MAX_QUEUED_SPANS: usize = 256;
const MAX_EXPORT_BATCH_SPANS: usize = 64;
const MAX_TRACE_BODY_BYTES: usize = 256 * 1024;
const MAX_EVENTS_PER_SPAN: u32 = 32;

#[derive(Debug, thiserror::Error)]
pub enum TelemetryConfigError {
    #[error("invalid local OTLP trace endpoint")]
    InvalidEndpoint,
    #[error("unsupported OTLP setting {0}; configure the local Collector instead")]
    UnsupportedVariable(&'static str),
    #[error("OTLP endpoint variable {0} is not UTF-8")]
    InvalidEncoding(&'static str),
    #[error("local OTLP trace exporter unavailable")]
    ExporterUnavailable,
}

#[derive(Clone)]
pub struct Telemetry {
    inner: Option<Arc<TelemetryInner>>,
}

struct TelemetryInner {
    provider: SdkTracerProvider,
    tracer: SdkTracer,
}

struct TraceSpan {
    tracer: Option<SdkTracer>,
    context: Option<Context>,
}

pub(crate) struct RunTrace {
    span: TraceSpan,
    session_id: SessionId,
    run_id: RunId,
    provider: TraceProvider,
}

pub(crate) struct AgentTrace {
    span: TraceSpan,
    session_id: SessionId,
    run_id: RunId,
    agent_run_id: AgentRunId,
    provider: TraceProvider,
}

pub(crate) struct ProviderTrace(TraceSpan);

pub(crate) struct PlanTrace(TraceSpan);

pub(crate) struct FirstProviderCall(Option<SystemTime>);

pub(crate) struct CompactionTrace {
    span: TraceSpan,
    provider: TraceProvider,
}

#[derive(Clone, Copy)]
pub(crate) enum TraceProvider {
    OpenAi,
    Anthropic,
    ChatGpt,
    Custom,
    Other,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum TraceOutcome {
    Finished,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy)]
pub(crate) enum ProviderTraceOutcome {
    Finished,
    Failed(TraceFailure),
    Cancelled,
}

#[derive(Clone, Copy)]
pub(crate) enum TraceFailure {
    Timeout,
    Unavailable,
    Rejected,
    InvalidResponse,
    OutputLimit,
    TaskPanic,
}

#[derive(Clone, Copy)]
pub(crate) enum TraceEventKind {
    MessageAccepted,
    RunStarted,
    AgentSpawned,
    ProviderCallRecorded,
    ToolStarted,
    ToolFinished,
    AgentFinished,
    MessageCommitted,
    RunFinished,
    ContextCompacted,
}

impl Telemetry {
    pub fn from_process(cli_endpoint: Option<&str>) -> Result<Self, TelemetryConfigError> {
        let endpoint = config::resolve_endpoint(cli_endpoint, |name| env::var_os(name))?;
        let Some(endpoint) = endpoint else {
            return Ok(Self::disabled());
        };
        Self::from_endpoint(endpoint)
    }

    fn from_endpoint(endpoint: config::LocalEndpoint) -> Result<Self, TelemetryConfigError> {
        let client = http::BoundedHttpClient::new()?;
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint.0)
            .with_timeout(EXPORT_TIMEOUT)
            .with_http_client(client)
            .with_retry_policy(RetryPolicy::disabled())
            .with_max_request_body_size(MAX_TRACE_BODY_BYTES)
            .build()
            .map_err(|_| TelemetryConfigError::ExporterUnavailable)?;
        let batch = BatchSpanProcessor::builder(exporter)
            .with_batch_config(
                BatchConfigBuilder::default()
                    .with_max_queue_size(MAX_QUEUED_SPANS)
                    .with_max_export_batch_size(MAX_EXPORT_BATCH_SPANS)
                    .with_scheduled_delay(Duration::from_millis(250))
                    .build(),
            )
            .build();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(batch)
            .with_resource(telemetry_resource())
            .with_sampler(Sampler::AlwaysOn)
            .with_max_attributes_per_span(32)
            .with_max_events_per_span(MAX_EVENTS_PER_SPAN)
            .with_max_attributes_per_event(4)
            .build();
        let tracer = provider.tracer("arany");
        Ok(Self {
            inner: Some(Arc::new(TelemetryInner { provider, tracer })),
        })
    }

    #[must_use]
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn shutdown(self) {
        if let Some(inner) = self.inner {
            let _ = inner.provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
        }
    }

    pub(crate) fn begin_run(
        &self,
        session_id: SessionId,
        run_id: RunId,
        provider: TraceProvider,
        context_timing: Option<(SystemTime, SystemTime)>,
    ) -> RunTrace {
        let span = self
            .inner
            .as_ref()
            .map_or_else(TraceSpan::disabled, |inner| {
                TraceSpan::root_at(
                    &inner.tracer,
                    "invoke_workflow arany.run",
                    SpanKind::Internal,
                    vec![
                        KeyValue::new("gen_ai.operation.name", "invoke_workflow"),
                        KeyValue::new("gen_ai.workflow.name", "arany.run"),
                        KeyValue::new("arany.session.id", session_id.to_string()),
                        KeyValue::new("arany.run.id", run_id.to_string()),
                    ],
                    context_timing.map(|(started, _)| started),
                )
            });
        let trace = RunTrace {
            span,
            session_id,
            run_id,
            provider,
        };
        if let Some((started, ended)) = context_timing {
            trace.context_compiled(started, ended);
        }
        trace
    }

    pub(crate) fn begin_compaction(
        &self,
        session_id: SessionId,
        covered_run_id: RunId,
        provider: TraceProvider,
    ) -> CompactionTrace {
        let Some(inner) = &self.inner else {
            return CompactionTrace {
                span: TraceSpan::disabled(),
                provider,
            };
        };
        CompactionTrace {
            span: TraceSpan::root(
                &inner.tracer,
                "compact arany.session",
                SpanKind::Internal,
                vec![
                    KeyValue::new("arany.operation", "compact"),
                    KeyValue::new("arany.session.id", session_id.to_string()),
                    KeyValue::new("arany.run.id", covered_run_id.to_string()),
                ],
            ),
            provider,
        }
    }
}

fn telemetry_resource() -> Resource {
    Resource::builder_empty()
        .with_detector(Box::new(
            opentelemetry_sdk::resource::TelemetryResourceDetector,
        ))
        .with_service_name("arany")
        .with_attributes([
            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
            KeyValue::new("service.instance.id", uuid::Uuid::now_v7().to_string()),
        ])
        .build()
}

impl TraceProvider {
    pub(crate) fn from_profile(profile: &str) -> Self {
        match profile {
            "openai" => Self::OpenAi,
            "anthropic" => Self::Anthropic,
            "chatgpt" => Self::ChatGpt,
            value if value.starts_with("custom:") => Self::Custom,
            _ => Self::Other,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::ChatGpt => "chatgpt",
            Self::Custom => "custom",
            Self::Other => "other",
        }
    }

    fn server_address(self) -> Option<&'static str> {
        match self {
            Self::OpenAi | Self::ChatGpt => Some("api.openai.com"),
            Self::Anthropic => Some("api.anthropic.com"),
            Self::Custom | Self::Other => None,
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::disabled()
    }
}

impl RunTrace {
    fn context_compiled(&self, started: SystemTime, ended: SystemTime) {
        if self.span.context.is_none() {
            return;
        }
        self.span
            .child(
                "compile_context arany.run",
                SpanKind::Internal,
                vec![
                    KeyValue::new("arany.operation", "compile_context"),
                    KeyValue::new("arany.session.id", self.session_id.to_string()),
                    KeyValue::new("arany.run.id", self.run_id.to_string()),
                ],
                Some(started),
            )
            .finish_at(TraceOutcome::Finished, ended);
    }

    pub(crate) fn begin_primary(&self, agent_run_id: AgentRunId) -> AgentTrace {
        if self.span.context.is_none() {
            return AgentTrace {
                span: TraceSpan::disabled(),
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id,
                provider: self.provider,
            };
        }
        let span = self.span.child(
            "invoke_agent arany.primary",
            SpanKind::Internal,
            vec![
                KeyValue::new("gen_ai.operation.name", "invoke_agent"),
                KeyValue::new("gen_ai.agent.name", "primary"),
                KeyValue::new("arany.session.id", self.session_id.to_string()),
                KeyValue::new("arany.run.id", self.run_id.to_string()),
                KeyValue::new("arany.agent_run.id", agent_run_id.to_string()),
                KeyValue::new("arany.agent.role", "primary"),
            ],
            None,
        );
        AgentTrace {
            span,
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id,
            provider: self.provider,
        }
    }

    pub(crate) fn event_committed(&self, sequence: u64, kind: TraceEventKind) {
        self.span.event_committed(sequence, kind);
    }

    pub(crate) fn finish(self, outcome: TraceOutcome) {
        self.span.finish(outcome);
    }
}

impl AgentTrace {
    pub(crate) fn begin_child(&self, agent_run_id: AgentRunId) -> AgentTrace {
        if self.span.context.is_none() {
            return AgentTrace {
                span: TraceSpan::disabled(),
                session_id: self.session_id,
                run_id: self.run_id,
                agent_run_id,
                provider: self.provider,
            };
        }
        AgentTrace {
            span: self.span.child(
                "invoke_agent arany.child",
                SpanKind::Internal,
                vec![
                    KeyValue::new("gen_ai.operation.name", "invoke_agent"),
                    KeyValue::new("gen_ai.agent.name", "child"),
                    KeyValue::new("arany.session.id", self.session_id.to_string()),
                    KeyValue::new("arany.run.id", self.run_id.to_string()),
                    KeyValue::new("arany.agent_run.id", agent_run_id.to_string()),
                    KeyValue::new("arany.agent_run.parent_id", self.agent_run_id.to_string()),
                    KeyValue::new("arany.agent.role", "child"),
                ],
                None,
            ),
            session_id: self.session_id,
            run_id: self.run_id,
            agent_run_id,
            provider: self.provider,
        }
    }

    pub(crate) fn begin_first_provider(&self) -> FirstProviderCall {
        FirstProviderCall(self.span.context.as_ref().map(|_| SystemTime::now()))
    }

    fn begin_plan_at(&self, started: SystemTime) -> PlanTrace {
        PlanTrace(self.span.child(
            "plan arany.primary",
            SpanKind::Internal,
            self.attributes("plan"),
            Some(started),
        ))
    }

    pub(crate) fn begin_provider(&self, phase: AgentPhase, model: &str) -> ProviderTrace {
        if self.span.context.is_none() {
            return ProviderTrace(TraceSpan::disabled());
        }
        self.begin_provider_at(phase, model, None)
    }

    fn begin_provider_at(
        &self,
        phase: AgentPhase,
        model: &str,
        started: Option<SystemTime>,
    ) -> ProviderTrace {
        ProviderTrace(self.span.child(
            "chat arany.provider",
            SpanKind::Client,
            self.provider_attributes(phase, model),
            started,
        ))
    }

    fn attributes(&self, operation: &'static str) -> Vec<KeyValue> {
        vec![
            KeyValue::new("gen_ai.operation.name", operation),
            KeyValue::new("arany.session.id", self.session_id.to_string()),
            KeyValue::new("arany.run.id", self.run_id.to_string()),
            KeyValue::new("arany.agent_run.id", self.agent_run_id.to_string()),
        ]
    }

    fn provider_attributes(&self, phase: AgentPhase, model: &str) -> Vec<KeyValue> {
        let mut attributes = self.attributes("chat");
        attributes.push(KeyValue::new("arany.provider.phase", phase_name(phase)));
        attributes.push(KeyValue::new("gen_ai.provider.name", self.provider.name()));
        if let Some(address) = self.provider.server_address() {
            attributes.push(KeyValue::new("server.address", address));
        }
        if matches!(self.provider, TraceProvider::OpenAi) {
            attributes.push(KeyValue::new("openai.api.type", "responses"));
        }
        if let Some(model) = safe_model(model) {
            attributes.push(KeyValue::new("gen_ai.request.model", model.to_owned()));
        }
        attributes
    }

    pub(crate) fn finish(self, outcome: TraceOutcome) {
        self.span.finish(outcome);
    }
}

impl PlanTrace {
    fn begin_provider_at(
        &self,
        agent: &AgentTrace,
        phase: AgentPhase,
        model: &str,
        started: SystemTime,
    ) -> ProviderTrace {
        ProviderTrace(self.0.child(
            "chat arany.provider",
            SpanKind::Client,
            agent.provider_attributes(phase, model),
            Some(started),
        ))
    }

    fn finish_at(self, outcome: TraceOutcome, ended: SystemTime) {
        self.0.finish_at(outcome, ended);
    }
}

impl ProviderTrace {
    pub(crate) fn finish_with_usage(
        self,
        outcome: ProviderTraceOutcome,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        self.set_usage(input_tokens, output_tokens);
        self.set_error_type(outcome);
        self.0.finish(outcome.trace_outcome());
    }

    fn finish_at(
        self,
        outcome: ProviderTraceOutcome,
        ended: SystemTime,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        self.set_usage(input_tokens, output_tokens);
        self.set_error_type(outcome);
        self.0.finish_at(outcome.trace_outcome(), ended);
    }

    fn set_error_type(&self, outcome: ProviderTraceOutcome) {
        if let (Some(context), Some(error_type)) = (&self.0.context, outcome.error_type()) {
            context
                .span()
                .set_attribute(KeyValue::new("error.type", error_type));
        }
    }

    fn set_usage(&self, input_tokens: Option<u32>, output_tokens: Option<u32>) {
        if let Some(context) = &self.0.context {
            if let Some(count) = input_tokens.filter(|count| *count <= MAX_REPORTED_INPUT_TOKENS) {
                context
                    .span()
                    .set_attribute(KeyValue::new("gen_ai.usage.input_tokens", i64::from(count)));
            }
            if let Some(count) = output_tokens.filter(|count| *count <= 1_000_000) {
                context.span().set_attribute(KeyValue::new(
                    "gen_ai.usage.output_tokens",
                    i64::from(count),
                ));
            }
        }
    }
}

impl FirstProviderCall {
    pub(crate) fn finish(
        self,
        agent: &AgentTrace,
        model: &str,
        delegated: bool,
        outcome: ProviderTraceOutcome,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        let Some(started) = self.0 else {
            return;
        };
        let ended = SystemTime::now();
        if delegated {
            let plan = agent.begin_plan_at(started);
            plan.begin_provider_at(agent, AgentPhase::RootPlan, model, started)
                .finish_at(outcome, ended, input_tokens, output_tokens);
            plan.finish_at(outcome.trace_outcome(), ended);
        } else {
            agent
                .begin_provider_at(AgentPhase::RootPlan, model, Some(started))
                .finish_at(outcome, ended, input_tokens, output_tokens);
        }
    }
}

impl CompactionTrace {
    pub(crate) fn begin_provider(&self, model: &str) -> ProviderTrace {
        if self.span.context.is_none() {
            return ProviderTrace(TraceSpan::disabled());
        }
        let mut attributes = vec![
            KeyValue::new("arany.provider.phase", "compaction"),
            KeyValue::new("gen_ai.provider.name", self.provider.name()),
        ];
        if let Some(address) = self.provider.server_address() {
            attributes.push(KeyValue::new("server.address", address));
        }
        if let Some(model) = safe_model(model) {
            attributes.push(KeyValue::new("gen_ai.request.model", model.to_owned()));
        }
        ProviderTrace(
            self.span
                .child("chat arany.compaction", SpanKind::Client, attributes, None),
        )
    }

    pub(crate) fn event_committed(&self, sequence: u64) {
        self.span
            .event_committed(sequence, TraceEventKind::ContextCompacted);
    }

    pub(crate) fn finish(self, outcome: TraceOutcome) {
        self.span.finish(outcome);
    }
}

impl TraceSpan {
    fn disabled() -> Self {
        Self {
            tracer: None,
            context: None,
        }
    }

    fn root(
        tracer: &SdkTracer,
        name: &'static str,
        kind: SpanKind,
        attributes: Vec<KeyValue>,
    ) -> Self {
        Self::root_at(tracer, name, kind, attributes, None)
    }

    fn root_at(
        tracer: &SdkTracer,
        name: &'static str,
        kind: SpanKind,
        attributes: Vec<KeyValue>,
        started: Option<SystemTime>,
    ) -> Self {
        let mut builder = tracer
            .span_builder(name)
            .with_kind(kind)
            .with_attributes(attributes);
        if let Some(started) = started {
            builder = builder.with_start_time(started);
        }
        let span = builder.start_with_context(tracer, &Context::new());
        Self {
            tracer: Some(tracer.clone()),
            context: Some(Context::new().with_span(span)),
        }
    }

    fn child(
        &self,
        name: &'static str,
        kind: SpanKind,
        attributes: Vec<KeyValue>,
        started: Option<SystemTime>,
    ) -> Self {
        let (Some(tracer), Some(parent)) = (&self.tracer, &self.context) else {
            return Self::disabled();
        };
        let mut builder = tracer
            .span_builder(name)
            .with_kind(kind)
            .with_attributes(attributes);
        if let Some(started) = started {
            builder = builder.with_start_time(started);
        }
        let span = builder.start_with_context(tracer, parent);
        Self {
            tracer: Some(tracer.clone()),
            context: Some(parent.with_span(span)),
        }
    }

    fn event_committed(&self, sequence: u64, kind: TraceEventKind) {
        if let Some(context) = &self.context {
            context.span().add_event(
                "arany.event.committed",
                vec![
                    KeyValue::new("arany.event.sequence", sequence as i64),
                    KeyValue::new("arany.event.kind", kind.name()),
                ],
            );
        }
    }

    fn finish(self, outcome: TraceOutcome) {
        self.finish_at(outcome, SystemTime::now());
    }

    fn finish_at(mut self, outcome: TraceOutcome, ended: SystemTime) {
        if let Some(context) = self.context.take() {
            context
                .span()
                .set_attribute(KeyValue::new("arany.outcome", outcome.name()));
            if outcome != TraceOutcome::Finished {
                context.span().set_status(Status::error(outcome.name()));
            }
            context.span().end_with_timestamp(ended);
        }
    }
}

impl Drop for TraceSpan {
    fn drop(&mut self) {
        if let Some(context) = self.context.take() {
            context
                .span()
                .set_attribute(KeyValue::new("arany.outcome", "abandoned"));
            context.span().end();
        }
    }
}

impl TraceOutcome {
    fn name(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl ProviderTraceOutcome {
    fn trace_outcome(self) -> TraceOutcome {
        match self {
            Self::Finished => TraceOutcome::Finished,
            Self::Failed(_) => TraceOutcome::Failed,
            Self::Cancelled => TraceOutcome::Cancelled,
        }
    }

    fn error_type(self) -> Option<&'static str> {
        match self {
            Self::Finished => None,
            Self::Failed(failure) => Some(failure.name()),
            Self::Cancelled => Some("cancelled"),
        }
    }
}

impl TraceFailure {
    fn name(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Unavailable => "provider_unavailable",
            Self::Rejected => "provider_rejected",
            Self::InvalidResponse => "invalid_response",
            Self::OutputLimit => "output_limit",
            Self::TaskPanic => "task_panic",
        }
    }
}

impl TraceEventKind {
    fn name(self) -> &'static str {
        match self {
            Self::MessageAccepted => "MessageAccepted",
            Self::RunStarted => "RunStarted",
            Self::AgentSpawned => "AgentSpawned",
            Self::ProviderCallRecorded => "ProviderCallRecorded",
            Self::ToolStarted => "ToolStarted",
            Self::ToolFinished => "ToolFinished",
            Self::AgentFinished => "AgentFinished",
            Self::MessageCommitted => "MessageCommitted",
            Self::RunFinished => "RunFinished",
            Self::ContextCompacted => "ContextCompacted",
        }
    }
}

fn phase_name(phase: AgentPhase) -> &'static str {
    match phase {
        AgentPhase::RootPlan => "primary_plan",
        AgentPhase::ToolReview => "tool_review",
        AgentPhase::ChildWork => "child_work",
        AgentPhase::RootSynthesis => "primary_synthesis",
    }
}

fn safe_model(model: &str) -> Option<&str> {
    (model.len() <= MAX_MODEL_BYTES && !model.chars().any(char::is_control)).then_some(model)
}

#[cfg(test)]
mod tests;
