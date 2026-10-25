use crate::journal::{Subscription, Tapped};
use crate::provider::{
    AgentPhase, MAX_REPORTED_INPUT_TOKENS, ProviderIdentity, ProviderKind, ProviderRole,
    ProviderRoutes, Providers,
};
use crate::session::{
    AgentDisposition, AgentRole, AgentRunId, CompactionFailure, CompactionStatus, Event,
    EventEnvelope, GoalId, GoalStatus, GoalView, ProviderCallDisposition, ProviderCallRecord,
    RunDisposition, RunId, SessionId,
};
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
    collections::HashMap,
    env,
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

mod config;
mod http;

const EXPORT_TIMEOUT: Duration = Duration::from_millis(500);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_MODEL_BYTES: usize = 256;
const MAX_QUEUED_SPANS: usize = 256;
const MAX_EXPORT_BATCH_SPANS: usize = 32;
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
    tracer: SdkTracer,
    session_id: SessionId,
    goal_id: GoalId,
    routes: Option<ProviderRoutes>,
    roster: Option<Box<crate::RosterSnapshot>>,
    switched: HashMap<(AgentRunId, ProviderRole), ProviderIdentity>,
    context: Option<(SystemTime, SystemTime)>,
    goal: Option<TraceSpan>,
    run: Option<(RunId, TraceSpan)>,
    primary: Option<AgentRunId>,
    agents: HashMap<AgentRunId, AgentSpan>,
    primary_called: bool,
    compaction_started: Option<SystemTime>,
    compaction_requested: Option<ProviderIdentity>,
    run_end: Option<(TraceOutcome, SystemTime)>,
    goal_end: Option<(GoalStatus, SystemTime)>,
    unguarded_pending: HashMap<[u8; 32], bool>,
    unguarded_completed: u64,
}

struct AgentSpan {
    child_route: Option<ProviderIdentity>,
    span: TraceSpan,
    call_started: Option<SystemTime>,
    requested: Option<ProviderIdentity>,
}

pub(crate) struct ProviderTrace(TraceSpan);

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
    StoppedByHook,
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
    Goal(&'static str),
    Run(&'static str),
    Unmarked,
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

    pub(crate) fn trace_run(
        &self,
        session_id: SessionId,
        goal_id: GoalId,
        context: (SystemTime, SystemTime),
    ) -> Option<RunTrace> {
        let inner = self.inner.as_ref()?;
        Some(RunTrace {
            tracer: inner.tracer.clone(),
            session_id,
            goal_id,
            routes: None,
            roster: None,
            switched: HashMap::new(),
            context: Some(context),
            goal: None,
            run: None,
            primary: None,
            agents: HashMap::new(),
            primary_called: false,
            compaction_started: None,
            compaction_requested: None,
            run_end: None,
            goal_end: None,
            unguarded_pending: HashMap::new(),
            unguarded_completed: 0,
        })
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
            span: TraceSpan::root_at(
                &inner.tracer,
                "compact arany.session",
                SpanKind::Internal,
                vec![
                    KeyValue::new("arany.operation", "compact"),
                    KeyValue::new("arany.session.id", session_id.to_string()),
                    KeyValue::new("arany.run.id", covered_run_id.to_string()),
                ],
                None,
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
        match ProviderKind::parse(profile) {
            Some(ProviderKind::OpenAi) => Self::OpenAi,
            Some(ProviderKind::Anthropic) => Self::Anthropic,
            Some(ProviderKind::ChatGpt) => Self::ChatGpt,
            Some(ProviderKind::Custom) => Self::Custom,
            None => Self::Other,
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

    fn compaction_attributes(self, model: &str) -> Vec<KeyValue> {
        let mut attributes = vec![
            KeyValue::new("arany.provider.phase", "compaction"),
            KeyValue::new("gen_ai.provider.name", self.name()),
        ];
        if let Some(address) = self.server_address() {
            attributes.push(KeyValue::new("server.address", address));
        }
        if let Some(model) = safe_model(model) {
            attributes.push(KeyValue::new("gen_ai.request.model", model.to_owned()));
        }
        attributes
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::disabled()
    }
}

impl RunTrace {
    pub(crate) async fn follow(
        mut self,
        subscription: Subscription,
        end: impl Future<Output = ()>,
        goal: impl FnOnce() -> Option<GoalView>,
    ) {
        subscription
            .follow(end, |tapped| {
                // Lost Events leave their spans open; they export as abandoned.
                if let Tapped::Committed(envelope) = tapped {
                    self.committed(&envelope);
                }
            })
            .await;
        self.finish(goal().as_ref());
    }

    fn committed(&mut self, envelope: &EventEnvelope) {
        if envelope.session_id != self.session_id {
            return;
        }
        let at = commit_time(envelope.created_at_ms);
        self.open(&envelope.event, at);
        self.mark(envelope, at);
        self.close(envelope, at);
    }

    fn unguarded_effects(&self) -> i64 {
        saturating_i64(
            self.unguarded_completed.saturating_add(
                self.unguarded_pending
                    .values()
                    .filter(|approved| **approved)
                    .count() as u64,
            ),
        )
    }

    fn run_id(&self) -> Option<RunId> {
        self.run.as_ref().map(|(run_id, _)| *run_id)
    }

    fn open(&mut self, event: &Event, at: SystemTime) {
        match event {
            Event::GoalStarted { goal_id, .. }
            | Event::GoalResumed { goal_id }
            | Event::GoalRunLinked { goal_id, .. }
                if *goal_id == self.goal_id && self.goal.is_none() =>
            {
                self.goal = Some(TraceSpan::root_at(
                    &self.tracer,
                    "invoke_workflow arany.goal",
                    SpanKind::Internal,
                    vec![
                        KeyValue::new("gen_ai.operation.name", "invoke_workflow"),
                        KeyValue::new("gen_ai.workflow.name", "arany.goal"),
                        KeyValue::new("arany.session.id", self.session_id.to_string()),
                        KeyValue::new("arany.goal.id", self.goal_id.to_string()),
                    ],
                    Some(at),
                ));
            }
            Event::MessageAccepted { run_id, .. } if self.run.is_none() => {
                let span = child_of(
                    self.goal.as_ref(),
                    "invoke_workflow arany.run",
                    SpanKind::Internal,
                    vec![
                        KeyValue::new("gen_ai.operation.name", "invoke_workflow"),
                        KeyValue::new("gen_ai.workflow.name", "arany.run"),
                        KeyValue::new("arany.session.id", self.session_id.to_string()),
                        KeyValue::new("arany.run.id", run_id.to_string()),
                        KeyValue::new("arany.goal.id", self.goal_id.to_string()),
                    ],
                    at,
                );
                self.run = Some((*run_id, span));
            }
            Event::AgentSpawned {
                run_id,
                agent_run_id,
                role,
                profile,
                ..
            } if self.run_id() == Some(*run_id) => {
                let mut attributes = vec![
                    KeyValue::new("gen_ai.operation.name", "invoke_agent"),
                    KeyValue::new("arany.session.id", self.session_id.to_string()),
                    KeyValue::new("arany.run.id", run_id.to_string()),
                    KeyValue::new("arany.agent_run.id", agent_run_id.to_string()),
                ];
                let (name, parent) = match role {
                    AgentRole::Primary => {
                        self.primary = Some(*agent_run_id);
                        attributes.push(KeyValue::new("gen_ai.agent.name", "primary"));
                        attributes.push(KeyValue::new("arany.agent.role", "primary"));
                        (
                            "invoke_agent arany.primary",
                            self.run.as_ref().map(|(_, span)| span),
                        )
                    }
                    AgentRole::Child => {
                        attributes.push(KeyValue::new("gen_ai.agent.name", "child"));
                        if let Some(primary) = self.primary {
                            attributes.push(KeyValue::new(
                                "arany.agent_run.parent_id",
                                primary.to_string(),
                            ));
                        }
                        attributes.push(KeyValue::new("arany.agent.role", "child"));
                        (
                            "invoke_agent arany.child",
                            self.primary
                                .and_then(|primary| self.agents.get(&primary))
                                .map(|agent| &agent.span),
                        )
                    }
                };
                let span = child_of(parent, name, SpanKind::Internal, attributes, at);
                self.agents.insert(
                    *agent_run_id,
                    AgentSpan {
                        child_route: profile
                            .as_ref()
                            .and_then(|name| self.roster.as_ref()?.chain(name)?.first())
                            .cloned(),
                        span,
                        call_started: None,
                        requested: None,
                    },
                );
            }
            Event::RunStarted { run_id, config, .. } if self.run_id() == Some(*run_id) => {
                self.roster = config.roster.clone();
                self.routes = config.providers.clone().or_else(|| {
                    config
                        .identity()
                        .map(|identity| Providers::all(vec![identity]))
                });
            }
            Event::GoalProviderCallReserved { agent_run_id, .. } => {
                if let Some(agent) = self.agents.get_mut(agent_run_id) {
                    agent.call_started = Some(at);
                    agent.requested = None;
                }
            }
            Event::GoalCompactionCallReserved { .. } => {
                self.compaction_started = Some(at);
                self.compaction_requested = None;
            }
            Event::ProviderFallbackUsed {
                run_id,
                agent_run_id,
                fallback,
            } if self.run_id() == Some(*run_id) => {
                // A logical call's span keeps the route it requested; the switch is its own marker.
                let requested = if fallback.role == ProviderRole::Compaction {
                    Some(&mut self.compaction_requested)
                } else {
                    self.agents
                        .get_mut(agent_run_id)
                        .map(|agent| &mut agent.requested)
                };
                if let Some(requested) = requested {
                    requested.get_or_insert_with(|| fallback.from.clone());
                }
                self.switched
                    .insert((*agent_run_id, fallback.role), fallback.to.clone());
            }
            _ => {}
        }
    }

    fn mark(&mut self, envelope: &EventEnvelope, at: SystemTime) {
        if let Event::SupervisorVerdictRecorded {
            run_id, verdict, ..
        } = &envelope.event
        {
            use crate::supervisor::{Level, SignalKind};
            if let Some((_, span)) = self.run.as_ref().filter(|(id, _)| id == run_id)
                && let Some(context) = &span.context
            {
                let signals = verdict
                    .signals
                    .iter()
                    .map(|signal| match signal.signal {
                        SignalKind::Loop => "loop".into(),
                        SignalKind::Probing => "probing".into(),
                        SignalKind::Disproportion => "disproportion".into(),
                    })
                    .collect::<Vec<opentelemetry::StringValue>>();
                let levels = verdict
                    .signals
                    .iter()
                    .map(|signal| match signal.level {
                        Level::Notice => "notice".into(),
                        Level::WouldNarrow => "would_narrow".into(),
                    })
                    .collect::<Vec<opentelemetry::StringValue>>();
                context.span().add_event_with_timestamp(
                    "arany.supervisor.verdict",
                    at,
                    vec![
                        KeyValue::new(
                            "arany.supervisor.signals",
                            opentelemetry::Value::Array(opentelemetry::Array::String(signals)),
                        ),
                        KeyValue::new(
                            "arany.supervisor.levels",
                            opentelemetry::Value::Array(opentelemetry::Array::String(levels)),
                        ),
                    ],
                );
            }
            return;
        }
        let kind = TraceEventKind::from(&envelope.event);
        let span = match kind {
            TraceEventKind::Goal(_) => self.goal.as_ref(),
            TraceEventKind::Run(_) => self.run.as_ref().map(|(_, span)| span),
            TraceEventKind::Unmarked => None,
        };
        let Some(span) = span.filter(|span| span.context.is_some()) else {
            return;
        };
        let context = self
            .context
            .take()
            .map(|(started, ended)| {
                [
                    KeyValue::new("arany.context.started_unix_nano", unix_nanos(started)),
                    KeyValue::new("arany.context.ended_unix_nano", unix_nanos(ended)),
                ]
            })
            .into_iter()
            .flatten();
        span.marker(envelope.sequence, kind, at, context);
    }

    fn close(&mut self, envelope: &EventEnvelope, at: SystemTime) {
        match &envelope.event {
            Event::ProviderCallRecorded {
                agent_run_id,
                record,
                ..
            } => self.provider_call(*agent_run_id, record, at),
            Event::ToolStarted { intent, .. } if intent.call.is_unguarded() => {
                self.unguarded_pending.insert(
                    crate::tools::EffectIntent::from(intent.clone()).digest(),
                    false,
                );
            }
            Event::ToolPermissionDecided { verdict, .. } => {
                if let Some(approved) = self.unguarded_pending.get_mut(&verdict.intent_digest) {
                    *approved = verdict.approved;
                }
            }
            Event::ToolFinished { observation, .. } => {
                if observation.intent.call.is_unguarded()
                    && self
                        .unguarded_pending
                        .remove(&observation.intent.digest())
                        .is_some()
                    && (matches!(
                        observation.attestation,
                        Some(crate::tools::Attestation::Unguarded(_))
                    ) || observation.disposition == crate::tools::ToolDisposition::Uncertain)
                {
                    self.unguarded_completed = self.unguarded_completed.saturating_add(1);
                }
            }
            Event::AgentFinished {
                agent_run_id,
                disposition,
                ..
            } => {
                if let Some(agent) = self.agents.remove(agent_run_id) {
                    agent.span.finish_at(TraceOutcome::from(*disposition), at);
                }
            }
            Event::GoalContextCompacted { record, .. } => {
                let started = self.compaction_started.take().unwrap_or(at);
                let requested = self.compaction_requested.take();
                let (Some((run_id, run)), Some(_)) = (&self.run, &self.routes) else {
                    return;
                };
                let (provider, model) = requested.as_ref().map_or(
                    (TraceProvider::from_profile(&record.provider), &record.model),
                    |identity| {
                        (
                            TraceProvider::from_profile(&identity.profile),
                            &identity.model,
                        )
                    },
                );
                let span = run.child(
                    "compact arany.goal_tool_context",
                    SpanKind::Internal,
                    vec![
                        KeyValue::new("arany.operation", "compact_goal_tool_context"),
                        KeyValue::new("arany.session.id", self.session_id.to_string()),
                        KeyValue::new("arany.run.id", run_id.to_string()),
                    ],
                    Some(started),
                );
                let outcome = ProviderTraceOutcome::from(&record.status);
                ProviderTrace(span.child(
                    "chat arany.compaction",
                    SpanKind::Client,
                    provider.compaction_attributes(model),
                    Some(started),
                ))
                .finish_at(outcome, at, record.input_tokens, record.output_tokens);
                span.marker(
                    envelope.sequence,
                    TraceEventKind::from(&envelope.event),
                    at,
                    [],
                );
                span.finish_at(outcome.into(), at);
            }
            Event::RunFinished {
                run_id,
                disposition,
                ..
            } if self.run_id() == Some(*run_id) => {
                let outcome = TraceOutcome::from(*disposition);
                if let Some((_, span)) = self.run.take() {
                    if let Some(context) = &span.context {
                        context.span().set_attribute(KeyValue::new(
                            "arany.run.unguarded_effects",
                            self.unguarded_effects(),
                        ));
                    }
                    span.finish_at(outcome, at);
                }
                self.run_end = Some((outcome, at));
            }
            Event::GoalFinished {
                goal_id, status, ..
            } if *goal_id == self.goal_id => self.goal_end = Some((*status, at)),
            _ => {}
        }
    }

    fn provider_call(
        &mut self,
        agent_run_id: AgentRunId,
        record: &ProviderCallRecord,
        at: SystemTime,
    ) {
        let Some(run_id) = self.run_id() else {
            return;
        };
        // The primary's first call opens a plan span when it delegates.
        let planned = self.primary == Some(agent_run_id)
            && !std::mem::replace(&mut self.primary_called, true)
            && record.disposition == ProviderCallDisposition::Delegated;
        let Some(agent) = self.agents.get_mut(&agent_run_id) else {
            return;
        };
        let started = agent.call_started.take().unwrap_or(at);
        let requested = agent.requested.take();
        let role = ProviderRole::for_phase(record.phase);
        let Some(route) = requested
            .as_ref()
            .or_else(|| self.switched.get(&(agent_run_id, role)))
            .or_else(|| {
                (role == ProviderRole::Child)
                    .then_some(agent.child_route.as_ref())
                    .flatten()
            })
            .or_else(|| {
                self.routes
                    .as_ref()
                    .and_then(|routes| routes.get(role).first())
            })
        else {
            return;
        };
        let identity = |operation: &'static str| {
            vec![
                KeyValue::new("gen_ai.operation.name", operation),
                KeyValue::new("arany.session.id", self.session_id.to_string()),
                KeyValue::new("arany.run.id", run_id.to_string()),
                KeyValue::new("arany.agent_run.id", agent_run_id.to_string()),
            ]
        };
        let provider = TraceProvider::from_profile(&route.profile);
        let mut attributes = identity("chat");
        attributes.push(KeyValue::new(
            "arany.provider.phase",
            phase_name(record.phase),
        ));
        attributes.push(KeyValue::new("gen_ai.provider.name", provider.name()));
        if let Some(address) = provider.server_address() {
            attributes.push(KeyValue::new("server.address", address));
        }
        if matches!(provider, TraceProvider::OpenAi) {
            attributes.push(KeyValue::new("openai.api.type", "responses"));
        }
        if let Some(model) = safe_model(&route.model) {
            attributes.push(KeyValue::new("gen_ai.request.model", model.to_owned()));
        }
        let outcome = ProviderTraceOutcome::from(record.disposition);
        let plan = planned.then(|| {
            agent.span.child(
                "plan arany.primary",
                SpanKind::Internal,
                identity("plan"),
                Some(started),
            )
        });
        ProviderTrace(plan.as_ref().unwrap_or(&agent.span).child(
            "chat arany.provider",
            SpanKind::Client,
            attributes,
            Some(started),
        ))
        .finish_at(outcome, at, record.input_tokens, record.output_tokens);
        if let Some(plan) = plan {
            plan.finish_at(outcome.into(), at);
        }
    }

    fn finish(mut self, goal: Option<&GoalView>) {
        let (Some((run, run_ended)), Some(span)) = (self.run_end, self.goal.take()) else {
            return;
        };
        let (status, ended) = self.goal_end.unwrap_or((GoalStatus::Running, run_ended));
        if let Some(context) = &span.context {
            context
                .span()
                .set_attribute(KeyValue::new("arany.goal.status", status.as_str()));
            if let Some(goal) = goal {
                for attribute in [
                    KeyValue::new("arany.goal.provider_calls", i64::from(goal.provider_calls)),
                    KeyValue::new("arany.goal.known_tokens", saturating_i64(goal.known_tokens)),
                    KeyValue::new("arany.goal.token_usage_unknown", goal.token_usage_unknown),
                    KeyValue::new("arany.goal.active_ms", saturating_i64(goal.active_ms)),
                    KeyValue::new(
                        "arany.goal.uncertain_active_ms",
                        saturating_i64(goal.uncertain_active_ms),
                    ),
                    KeyValue::new("arany.goal.unguarded_effects", self.unguarded_effects()),
                ] {
                    context.span().set_attribute(attribute);
                }
            }
        }
        let outcome = match status {
            GoalStatus::Completed => TraceOutcome::Finished,
            GoalStatus::Failed => TraceOutcome::Failed,
            GoalStatus::Running | GoalStatus::Interrupted => run,
        };
        span.finish_at(outcome, ended);
    }
}

impl ProviderTrace {
    pub(crate) fn finish_with_usage(
        self,
        outcome: ProviderTraceOutcome,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        self.finish_at(outcome, SystemTime::now(), input_tokens, output_tokens);
    }

    fn finish_at(
        self,
        outcome: ProviderTraceOutcome,
        ended: SystemTime,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    ) {
        if let Some(context) = &self.0.context {
            if let Some(error_type) = outcome.error_type() {
                context
                    .span()
                    .set_attribute(KeyValue::new("error.type", error_type));
            }
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
        self.0.finish_at(outcome.into(), ended);
    }
}

impl CompactionTrace {
    pub(crate) fn begin_provider(&self, model: &str) -> ProviderTrace {
        if self.span.context.is_none() {
            return ProviderTrace(TraceSpan::disabled());
        }
        ProviderTrace(self.span.child(
            "chat arany.compaction",
            SpanKind::Client,
            self.provider.compaction_attributes(model),
            None,
        ))
    }

    pub(crate) fn event_committed(&self, envelope: &EventEnvelope) {
        self.span.marker(
            envelope.sequence,
            TraceEventKind::from(&envelope.event),
            SystemTime::now(),
            [],
        );
    }

    pub(crate) fn finish(self, outcome: TraceOutcome) {
        self.span.finish_at(outcome, SystemTime::now());
    }
}

fn child_of(
    parent: Option<&TraceSpan>,
    name: &'static str,
    kind: SpanKind,
    attributes: Vec<KeyValue>,
    started: SystemTime,
) -> TraceSpan {
    parent.map_or_else(TraceSpan::disabled, |parent| {
        parent.child(name, kind, attributes, Some(started))
    })
}

impl TraceSpan {
    fn disabled() -> Self {
        Self {
            tracer: None,
            context: None,
        }
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

    fn marker(
        &self,
        sequence: u64,
        kind: TraceEventKind,
        at: SystemTime,
        extra: impl IntoIterator<Item = KeyValue>,
    ) {
        let (Some(context), Some(name)) = (&self.context, kind.name()) else {
            return;
        };
        let mut attributes = vec![
            KeyValue::new("arany.event.sequence", saturating_i64(sequence)),
            KeyValue::new("arany.event.kind", name),
        ];
        attributes.extend(extra);
        context
            .span()
            .add_event_with_timestamp("arany.event.committed", at, attributes);
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
            Self::StoppedByHook => "stopped_by_hook",
        }
    }
}

impl From<AgentDisposition> for TraceOutcome {
    fn from(disposition: AgentDisposition) -> Self {
        match disposition {
            AgentDisposition::Finished => Self::Finished,
            AgentDisposition::Failed => Self::Failed,
            AgentDisposition::Cancelled => Self::Cancelled,
            AgentDisposition::StoppedByHook => Self::StoppedByHook,
        }
    }
}

impl From<RunDisposition> for TraceOutcome {
    fn from(disposition: RunDisposition) -> Self {
        match disposition {
            RunDisposition::Finished => Self::Finished,
            RunDisposition::Failed => Self::Failed,
            RunDisposition::Cancelled => Self::Cancelled,
            RunDisposition::StoppedByHook => Self::StoppedByHook,
        }
    }
}

impl From<ProviderTraceOutcome> for TraceOutcome {
    fn from(outcome: ProviderTraceOutcome) -> Self {
        match outcome {
            ProviderTraceOutcome::Finished => Self::Finished,
            ProviderTraceOutcome::Failed(_) => Self::Failed,
            ProviderTraceOutcome::Cancelled => Self::Cancelled,
        }
    }
}

impl From<ProviderCallDisposition> for ProviderTraceOutcome {
    fn from(disposition: ProviderCallDisposition) -> Self {
        match disposition {
            ProviderCallDisposition::Finished
            | ProviderCallDisposition::Delegated
            | ProviderCallDisposition::PlanRecorded
            | ProviderCallDisposition::ToolRequested => Self::Finished,
            ProviderCallDisposition::Unavailable => Self::Failed(TraceFailure::Unavailable),
            ProviderCallDisposition::Rejected => Self::Failed(TraceFailure::Rejected),
            ProviderCallDisposition::InvalidResponse => Self::Failed(TraceFailure::InvalidResponse),
            ProviderCallDisposition::OutputLimit => Self::Failed(TraceFailure::OutputLimit),
            ProviderCallDisposition::TimedOut => Self::Failed(TraceFailure::Timeout),
            ProviderCallDisposition::TaskPanic => Self::Failed(TraceFailure::TaskPanic),
            ProviderCallDisposition::Cancelled => Self::Cancelled,
        }
    }
}

impl From<&CompactionStatus> for ProviderTraceOutcome {
    fn from(status: &CompactionStatus) -> Self {
        match status {
            CompactionStatus::Succeeded { .. } => Self::Finished,
            CompactionStatus::Failed { reason } => match reason {
                CompactionFailure::Cancelled => Self::Cancelled,
                CompactionFailure::ProviderUnavailable => Self::Failed(TraceFailure::Unavailable),
                CompactionFailure::ProviderRejected => Self::Failed(TraceFailure::Rejected),
                CompactionFailure::InvalidOutcome => Self::Failed(TraceFailure::InvalidResponse),
                CompactionFailure::TimedOut => Self::Failed(TraceFailure::Timeout),
            },
        }
    }
}

impl ProviderTraceOutcome {
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

impl From<&Event> for TraceEventKind {
    fn from(event: &Event) -> Self {
        let kind = event.kind();
        match event {
            Event::GoalStarted { .. }
            | Event::GoalRunLinked { .. }
            | Event::GoalResumed { .. }
            | Event::GoalPlanContractRecorded { .. }
            | Event::GoalFinished { .. }
            | Event::GoalProviderCallReserved { .. }
            | Event::GoalCompactionCallReserved { .. }
            | Event::GoalContextCompacted { .. } => Self::Goal(kind),
            Event::ToolResultsCleared { .. } => Self::Run(kind),
            Event::MessageAccepted { .. }
            | Event::RunStarted { .. }
            | Event::AgentSpawned { .. }
            | Event::ProviderFallbackUsed { .. }
            | Event::ProviderRetryScheduled { .. }
            | Event::ProviderCallRecorded { .. }
            | Event::ToolStarted { .. }
            | Event::ToolPermissionDecided { .. }
            | Event::HookStarted { .. }
            | Event::HookFinished { .. }
            | Event::ToolFinished { .. }
            | Event::AgentFinished { .. }
            | Event::MessageCommitted { .. }
            | Event::RunFinished { .. }
            | Event::ContextCompacted { .. } => Self::Run(kind),
            Event::SupervisorVerdictRecorded { .. }
            | Event::WorkerDataChunk { .. }
            | Event::WorkerDataSealed { .. }
            | Event::WorkerStopObserved { .. }
            | Event::WorkerHandoverRecorded { .. }
            | Event::WorkerIntegrationRecorded { .. }
            | Event::WorkerHandoverDiscarded { .. }
            | Event::WorkerLeaseReleased { .. }
            | Event::WorkerVerificationRecorded { .. }
            | Event::AssistantDisplayRecorded { .. }
            | Event::GoalSteeringNoteRecorded { .. }
            | Event::AgentSteeringNoteRecorded { .. }
            | Event::SessionStarted { .. }
            | Event::SessionDefaultChanged { .. }
            | Event::SessionForked { .. }
            | Event::WorkspaceRewindStarted { .. }
            | Event::WorkspaceRewindFinished { .. }
            | Event::SessionRenamed { .. } => Self::Unmarked,
            Event::ProviderSetupEditStarted { .. } | Event::ProviderSetupEditFinished { .. } => {
                Self::Unmarked
            }
        }
    }
}

impl TraceEventKind {
    fn name(self) -> Option<&'static str> {
        match self {
            Self::Goal(name) | Self::Run(name) => Some(name),
            Self::Unmarked => None,
        }
    }
}

fn commit_time(created_at_ms: i64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_millis(u64::try_from(created_at_ms).unwrap_or(0))
}

fn unix_nanos(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
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
