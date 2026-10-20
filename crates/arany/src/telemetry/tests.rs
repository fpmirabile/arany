use super::config::{parse_endpoint, resolve_endpoint};
use super::*;
use crate::engine::{Engine, EngineConfig, RunControls, RunRequest};
use crate::provider::{
    Answer, Delegate, Provenance, Provider, ProviderError, ProviderIdentity, ProviderOutcome,
    ProviderRequest, ProviderResponse,
};
use crate::session::{AgentStatus, CollaborationPolicy, RunStatus, WorkspaceIdentity};
use crate::store::StateRoot;
use opentelemetry::Value;
use opentelemetry::trace::SpanId;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as ProtoValue;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::trace::InMemorySpanExporter;
use opentelemetry_sdk::trace::SpanData;
use prost::Message;
use std::{
    collections::HashMap,
    ffi::OsString,
    future::pending,
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::Instant,
};
use tokio::sync::{mpsc, oneshot};

mod performance;

fn in_memory() -> (Telemetry, InMemorySpanExporter) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .with_sampler(Sampler::AlwaysOn)
        .with_max_events_per_span(MAX_EVENTS_PER_SPAN)
        .build();
    let tracer = provider.tracer("arany");
    (
        Telemetry {
            inner: Some(Arc::new(TelemetryInner { provider, tracer })),
        },
        exporter,
    )
}

struct GatedFailureProvider {
    started: mpsc::UnboundedSender<(String, oneshot::Sender<()>)>,
    failure: ChildFailureCase,
    requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
}

#[derive(Clone, Copy)]
enum ChildFailureCase {
    Rejected,
    NestedDelegation,
    OutputLimit,
    InputUsageOverflow,
}

struct WaitingTraceProvider {
    started: mpsc::UnboundedSender<()>,
    requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
}

#[async_trait::async_trait]
impl Provider for WaitingTraceProvider {
    fn identity(&self) -> &crate::provider::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<crate::provider::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                crate::provider::ProviderIdentity::new(
                    "scripted",
                    "test-model",
                    crate::provider::Provenance::native(),
                )
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        self.started.send(()).expect("Provider start gate");
        pending().await
    }
}

#[async_trait::async_trait]
impl Provider for GatedFailureProvider {
    fn identity(&self) -> &crate::provider::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<crate::provider::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                let mut identity = crate::provider::ProviderIdentity::new(
                    "scripted",
                    "test-model",
                    crate::provider::Provenance::native(),
                );
                identity.limits = crate::provider::CallLimits {
                    concurrency: 2,
                    ..Default::default()
                };
                identity
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let outcome = match request.phase {
            AgentPhase::RootPlan => ProviderOutcome::Delegate(Delegate {
                agent: None,
                children: vec![
                    crate::ChildAssignment {
                        editor: None,
                        objective: "A".into(),
                        profile: None,
                    },
                    crate::ChildAssignment {
                        editor: None,
                        objective: "B".into(),
                        profile: None,
                    },
                ],
            }),
            AgentPhase::ChildWork => {
                let (release, gate) = oneshot::channel();
                self.started
                    .send((request.input.objective.clone(), release))
                    .map_err(|_| ProviderError::Unavailable)?;
                gate.await.map_err(|_| ProviderError::Unavailable)?;
                if request.input.objective == "B" {
                    match self.failure {
                        ChildFailureCase::Rejected => return Err(ProviderError::Rejected),
                        ChildFailureCase::NestedDelegation => {
                            return Ok(ProviderResponse {
                                turn: crate::provider::ProviderTurn::scripted(),
                                outcome: ProviderOutcome::Delegate(Delegate {
                                    agent: None,
                                    children: vec![crate::ChildAssignment {
                                        editor: None,
                                        objective: "not permitted".into(),
                                        profile: None,
                                    }],
                                }),
                                response_id: None,
                                usage: crate::provider::Usage {
                                    input_tokens: Some(10),
                                    output_tokens: Some(5),
                                    ..Default::default()
                                },
                                wire_provenance: None,
                            });
                        }
                        ChildFailureCase::OutputLimit => {
                            return Ok(ProviderResponse {
                                turn: crate::provider::ProviderTurn::scripted(),
                                outcome: ProviderOutcome::Answer(Answer {
                                    text: "done".into(),
                                }),
                                response_id: None,
                                usage: crate::provider::Usage {
                                    input_tokens: Some(10),
                                    output_tokens: Some(request.max_output_tokens + 1),
                                    ..Default::default()
                                },
                                wire_provenance: None,
                            });
                        }
                        ChildFailureCase::InputUsageOverflow => {
                            return Ok(ProviderResponse {
                                turn: crate::provider::ProviderTurn::scripted(),
                                outcome: ProviderOutcome::Answer(Answer {
                                    text: "done".into(),
                                }),
                                response_id: None,
                                usage: crate::provider::Usage {
                                    input_tokens: Some(1_000_001),
                                    output_tokens: Some(5),
                                    ..Default::default()
                                },
                                wire_provenance: None,
                            });
                        }
                    }
                }
                ProviderOutcome::Answer(Answer {
                    text: "done".into(),
                })
            }
            AgentPhase::RootSynthesis | AgentPhase::ToolReview => {
                return Err(ProviderError::InvalidOutcome);
            }
        };
        Ok(ProviderResponse {
            turn: crate::provider::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: crate::provider::Usage {
                input_tokens: Some(10),
                output_tokens: Some(5),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

#[test]
fn failed_child_aborts_sibling_provider_span_as_cancelled() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("trace runtime");
    runtime.block_on(async {
        for (failure, failed_error_type) in [
            (ChildFailureCase::Rejected, "provider_rejected"),
            (ChildFailureCase::NestedDelegation, "invalid_response"),
            (ChildFailureCase::OutputLimit, "output_limit"),
            (ChildFailureCase::InputUsageOverflow, "invalid_response"),
        ] {
            let temp = tempfile::tempdir().expect("private trace root");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).expect("Workspace");
            let (telemetry, exporter) = in_memory();
            let (started, mut started_rx) = mpsc::unbounded_channel();
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let provider = GatedFailureProvider {
                started,
                failure,
                requests: Arc::clone(&requests),
            };
            let provider = Arc::new(provider);
            let engine = Engine::open(
                StateRoot::admit(&temp.path().join("state")).expect("private State"),
                EngineConfig {
                    telemetry: telemetry.clone(),
                    ..Default::default()
                },
            )
            .expect("traced Engine");
            let request = RunRequest {
                skill: None,
                session_id: None,
                title: None,
                objective: "synthetic task".into(),
                images: Vec::new(),
                workspace,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Team {
                    max_active_children: 2,
                },
                goal: crate::engine::GoalRequest::default(),
            };
            let mut run = tokio::spawn(async move {
                let outcome = match engine.submit(
                    crate::provider::AdmittedProviders::single(provider),
                    request,
                    RunControls::default(),
                ) {
                    Ok(handle) => handle.await.map(|result| result.run),
                    Err(error) => Err(error),
                };
                engine.close().await.expect("Engine close");
                outcome
            });
            let mut gates = HashMap::new();
            for _ in 0..2 {
                let (objective, release) =
                    tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                        .await
                        .expect("child start deadline")
                        .expect("child start");
                gates.insert(objective, release);
            }
            gates
                .remove("B")
                .expect("failing child gate")
                .send(())
                .expect("release B");
            let outcome = match tokio::time::timeout(Duration::from_secs(5), &mut run).await {
                Ok(joined) => joined.expect("Run task").expect("durable failed Run"),
                Err(_) => {
                    run.abort();
                    let _ = run.await;
                    panic!("failed-child Run did not terminate");
                }
            };
            assert_eq!(outcome.run.status, RunStatus::Failed);
            {
                let calls = requests.lock().expect("requests");
                assert_eq!(calls.len(), 3, "planning and two child calls only");
                assert_trace_call(&calls[0], AgentPhase::RootPlan, "synthetic task");
                assert_eq!(calls[0].agent_run_id, outcome.run.agents[0].id);
                let mut children = calls[1..].iter().collect::<Vec<_>>();
                children.sort_by(|left, right| left.input.objective.cmp(&right.input.objective));
                for (call, (objective, agent)) in children
                    .into_iter()
                    .zip([("A", &outcome.run.agents[1]), ("B", &outcome.run.agents[2])])
                {
                    assert_trace_call(call, AgentPhase::ChildWork, objective);
                    assert_eq!(call.agent_run_id, agent.id);
                }
                assert!(calls.iter().all(|call| call.run_id == outcome.run.id));
            }
            assert_eq!(
                outcome
                    .run
                    .agents
                    .iter()
                    .map(|agent| agent.status)
                    .collect::<Vec<_>>(),
                [
                    AgentStatus::Failed,
                    AgentStatus::Cancelled,
                    AgentStatus::Failed
                ]
            );
            assert!(
                gates
                    .remove("A")
                    .expect("aborted child gate")
                    .send(())
                    .is_err()
            );
            telemetry
                .inner
                .as_ref()
                .expect("enabled")
                .provider
                .force_flush()
                .expect("span flush");
            let spans = exporter.get_finished_spans().expect("finished spans");
            assert_eq!(spans.len(), 9);
            for (index, expected, error_type) in [
                (1, "cancelled", "cancelled"),
                (2, "failed", failed_error_type),
            ] {
                let agent_id = outcome.run.agents[index].id.to_string();
                let agent = spans
                    .iter()
                    .find(|span| {
                        span.name == "invoke_agent arany.child"
                            && span.attributes.iter().any(|attribute| {
                                attribute.key.as_str() == "arany.agent_run.id"
                                    && attribute.value == Value::String(agent_id.clone().into())
                            })
                    })
                    .expect("child Agent span");
                let provider = spans
                    .iter()
                    .find(|span| {
                        span.name == "chat arany.provider"
                            && span.parent_span_id == agent.span_context.span_id()
                    })
                    .expect("child Provider span");
                assert!(provider.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "arany.outcome"
                        && attribute.value == Value::String(expected.into())
                }));
                assert!(provider.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == "error.type"
                        && attribute.value == Value::String(error_type.into())
                }));
            }
        }
    });
}

#[test]
fn timed_out_provider_call_exports_closed_error_type() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("trace runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("private trace root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace).expect("Workspace");
        let (telemetry, exporter) = in_memory();
        let (started, mut started_rx) = mpsc::unbounded_channel();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let provider = WaitingTraceProvider {
            started,
            requests: Arc::clone(&requests),
        };
        let provider = Arc::new(provider);
        let engine = Engine::open(
            StateRoot::admit(&temp.path().join("state")).expect("private State"),
            EngineConfig {
                telemetry: telemetry.clone(),
                ..Default::default()
            },
        )
        .expect("traced Engine");
        let request = RunRequest {
            skill: None,
            session_id: None,
            title: None,
            objective: "synthetic task".into(),
            images: Vec::new(),
            workspace,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
            goal: crate::engine::GoalRequest::default(),
        };
        let mut run = tokio::spawn(async move {
            let outcome = match engine.submit(
                crate::provider::AdmittedProviders::single(provider),
                request,
                RunControls::default(),
            ) {
                Ok(handle) => handle.await.map(|result| result.run),
                Err(error) => Err(error),
            };
            engine.close().await.expect("Engine close");
            outcome
        });
        tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("Provider start deadline")
            .expect("Provider start");
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(121)).await;
        tokio::time::resume();
        let outcome = match tokio::time::timeout(Duration::from_secs(5), &mut run).await {
            Ok(joined) => joined.expect("Run task").expect("durable failed Run"),
            Err(_) => {
                run.abort();
                let _ = run.await;
                panic!("timed-out Provider Run did not terminate");
            }
        };
        assert_eq!(outcome.run.status, RunStatus::Failed);
        {
            let calls = requests.lock().expect("requests");
            assert_eq!(calls.len(), 1, "exactly one timed-out trace call");
            assert_trace_call(&calls[0], AgentPhase::RootPlan, "synthetic task");
            assert_eq!(calls[0].run_id, outcome.run.id);
            assert_eq!(calls[0].agent_run_id, outcome.run.agents[0].id);
        }
        telemetry
            .inner
            .as_ref()
            .expect("enabled")
            .provider
            .force_flush()
            .expect("span flush");
        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(spans.len(), 4);
        let provider = spans
            .iter()
            .find(|span| span.name == "chat arany.provider")
            .expect("Provider span");
        assert!(provider.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "error.type"
                && attribute.value == Value::String("timeout".into())
        }));
    });
}

fn assert_trace_call(actual: &ProviderRequest, phase: AgentPhase, objective: &str) {
    assert_eq!(actual.phase, phase, "trace call phase");
    assert!(actual.model == "test-model", "trace call model");
    assert!(actual.input.objective == objective, "trace call objective");
    assert!(actual.input.images.is_empty(), "trace call images");
    assert!(
        actual.input.workspace_guidance.is_none(),
        "trace call instructions"
    );
    assert!(actual.input.includes.is_empty(), "trace call includes");
    assert!(actual.input.history.is_empty(), "trace call history");
    assert!(actual.input.context_summary.is_none(), "trace call summary");
    assert!(actual.input.child_results.is_empty(), "trace call children");
    assert_eq!(actual.max_output_tokens, 4096, "trace call output cap");
}

struct Segment {
    trace: RunTrace,
    session_id: SessionId,
    goal_id: GoalId,
    run_id: RunId,
    sequence: u64,
}

impl Segment {
    fn begin(telemetry: &Telemetry, context: (SystemTime, SystemTime)) -> Self {
        Self::routed(telemetry, context, None)
    }

    fn routed(
        telemetry: &Telemetry,
        context: (SystemTime, SystemTime),
        routes: Option<ProviderRoutes>,
    ) -> Self {
        let mut segment = Self::opened(telemetry, context);
        segment.commit(Event::RunStarted {
            run_id: segment.run_id,
            config: run_config(routes),
            active_deadline_ms: None,
        });
        segment
    }

    fn opened(telemetry: &Telemetry, context: (SystemTime, SystemTime)) -> Self {
        let session_id = SessionId::new();
        let goal_id = GoalId::new();
        let mut segment = Self {
            trace: telemetry
                .trace_run(session_id, goal_id, context)
                .expect("enabled telemetry"),
            session_id,
            goal_id,
            run_id: RunId::new(),
            sequence: 0,
        };
        segment.commit(Event::GoalRunLinked {
            goal_id,
            run_id: segment.run_id,
            phase: crate::session::GoalPhase::Implementation,
        });
        segment.commit(Event::MessageAccepted {
            skill: None,
            run_id: segment.run_id,
            text: "objective".into(),
            images: Vec::new(),
        });
        segment
    }

    fn commit(&mut self, event: Event) -> u64 {
        self.sequence += 1;
        self.trace.committed(&EventEnvelope {
            sequence: self.sequence,
            session_id: self.session_id,
            run_id: None,
            agent_run_id: None,
            event,
            created_at_ms: commit_ms(self.sequence),
        });
        self.sequence
    }

    fn spawn(&mut self, role: AgentRole) -> AgentRunId {
        self.spawn_as(role, None)
    }

    fn spawn_as(&mut self, role: AgentRole, profile: Option<&str>) -> AgentRunId {
        let agent_run_id = AgentRunId::new();
        self.commit(Event::AgentSpawned {
            profile: profile.map(str::to_owned),
            assignment: None,
            definition: None,
            run_id: self.run_id,
            agent_run_id,
            role,
            ordinal: 0,
            objective: None,
            grant: None,
        });
        agent_run_id
    }

    fn call(
        &mut self,
        agent_run_id: AgentRunId,
        phase: AgentPhase,
        disposition: ProviderCallDisposition,
        usage: (Option<u32>, Option<u32>),
    ) -> (u64, u64) {
        let reserved = self.commit(Event::GoalProviderCallReserved {
            goal_id: self.goal_id,
            run_id: self.run_id,
            agent_run_id,
            active_ceiling_ms: 1,
        });
        let recorded = self.commit(Event::ProviderCallRecorded {
            run_id: self.run_id,
            agent_run_id,
            record: provider_record(phase, disposition, usage),
        });
        (reserved, recorded)
    }

    fn finish_agent(&mut self, agent_run_id: AgentRunId, disposition: AgentDisposition) {
        self.commit(Event::AgentFinished {
            worker: None,
            run_id: self.run_id,
            agent_run_id,
            disposition,
            summary: None,
            result: None,
        });
    }

    fn finish(mut self, disposition: RunDisposition, goal: Option<&GoalView>) {
        self.commit(Event::RunFinished {
            failure: (disposition == RunDisposition::Failed)
                .then_some(crate::session::RunFailure::ProviderCall),
            run_id: self.run_id,
            disposition,
            active_ms: 1,
        });
        self.trace.finish(goal);
    }
}

fn identity(profile: &str, model: &str) -> ProviderIdentity {
    ProviderIdentity::new(profile, model, Provenance::native())
}

fn child_routes() -> ProviderRoutes {
    let mut routes = Providers::all(vec![identity("other", "model-1")]);
    routes.child = vec![identity("anthropic", "child-model")];
    routes
}

fn run_config(providers: Option<ProviderRoutes>) -> crate::session::RunConfig {
    crate::session::RunConfig {
        supervisor: None,
        worker_policy: None,
        agent_definitions: None,
        roster: None,
        providers,
        provider: "other".into(),
        model: "model-1".into(),
        effort: None,
        custom_profile_provenance: None,
        saved_api_account_id: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        context_window_tokens: None,
        policy: CollaborationPolicy::Single,
        output_token_cap: 4096,
        provider_concurrency: 1,
        workspace_device: 1,
        workspace_inode: 1,
        instruction_digest: None,
        include_digests: Vec::new(),
        history_run_ids: Vec::new(),
        excluded_history_runs: 0,
        context_usage: None,
        compaction_event_sequence: None,
        compaction_content_digest: None,
        tool_policy: None,
    }
}

fn provider_record(
    phase: AgentPhase,
    disposition: ProviderCallDisposition,
    usage: (Option<u32>, Option<u32>),
) -> ProviderCallRecord {
    ProviderCallRecord {
        delegation: None,
        phase,
        disposition,
        requested_tools: 0,
        response_id: None,
        input_tokens: usage.0,
        output_tokens: usage.1,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        wire_provenance: None,
        refusal: None,
        failure_reason: None,
    }
}

fn commit_ms(sequence: u64) -> i64 {
    1_000 + 10 * i64::try_from(sequence).expect("bounded sequence")
}

fn committed_at(sequence: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH
        + Duration::from_millis(u64::try_from(commit_ms(sequence)).expect("positive time"))
}

fn finished_spans(telemetry: &Telemetry, exporter: &InMemorySpanExporter) -> Vec<SpanData> {
    telemetry
        .inner
        .as_ref()
        .expect("enabled")
        .provider
        .force_flush()
        .expect("in-memory flush");
    exporter.get_finished_spans().expect("finished spans")
}

#[test]
fn unguarded_counts_retain_only_pending_digests_and_approval_flags() {
    use crate::tools::permissions::{Decision, PermissionSource, PermissionVerdict};
    use crate::tools::{Attestation, CacheUpdate, EffectIntent, ToolCall, ToolDisposition};

    for (approved, outcome, expected) in [
        (false, None, 0),
        (true, None, 1),
        (true, Some((ToolDisposition::Succeeded, true)), 1),
        (true, Some((ToolDisposition::Failed, true)), 1),
        (true, Some((ToolDisposition::Uncertain, false)), 1),
        (true, Some((ToolDisposition::Denied, false)), 0),
        (true, Some((ToolDisposition::Cancelled, false)), 0),
        (false, Some((ToolDisposition::Denied, false)), 0),
    ] {
        let (telemetry, exporter) = in_memory();
        let mut segment = Segment::begin(&telemetry, unit_context());
        let agent_run_id = segment.spawn(AgentRole::Primary);
        let intent = EffectIntent {
            target: crate::session::worker::EffectTarget::Workspace,
            id: uuid::Uuid::now_v7(),
            run_id: segment.run_id,
            agent_run_id,
            policy_digest: [1; 32],
            enforcement_digest: [2; 32],
            workspace_device: 1,
            workspace_inode: 1,
            call: ToolCall::CommandUnguarded {
                program: "cargo".into(),
                args: vec!["tool-argument-canary".repeat(1024)],
                cwd: String::new(),
                justification: "Run the failed command once".into(),
            },
            limits: crate::tools::ToolLimits::default(),
            expires_at_ms: 1,
            use_count: 1,
        };
        let digest = intent.digest();
        segment.commit(Event::ToolStarted {
            run_id: segment.run_id,
            agent_run_id,
            intent: intent.clone(),
            active_ceiling_ms: 1,
        });
        assert_eq!(segment.trace.unguarded_effects(), 0);
        segment.commit(Event::ToolPermissionDecided {
            run_id: segment.run_id,
            agent_run_id,
            verdict: PermissionVerdict {
                intent_digest: digest,
                decision: Decision::Ask,
                mode: None,
                basis: PermissionSource::Mode,
                source: PermissionSource::Human,
                approved,
                hook_restriction: None,
                hooks: 0,
                narrowed: false,
            },
        });
        assert_eq!(segment.trace.unguarded_effects(), i64::from(approved));
        assert_eq!(
            segment.trace.unguarded_pending,
            HashMap::from([(digest, approved)])
        );
        if let Some((disposition, attested)) = outcome {
            segment.commit(Event::ToolFinished {
                checkpoint: None,
                run_id: segment.run_id,
                agent_run_id,
                observation: crate::tools::ToolObservation {
                    attestation: attested.then(|| Attestation::for_site(&intent)),
                    intent,
                    disposition,
                    enforcement_cause: None,
                    limit_cause: (disposition == crate::tools::ToolDisposition::Limit)
                        .then_some(crate::tools::LimitCause::Resource),
                    output: "tool-output-canary".repeat(1024),
                    cache_update: CacheUpdate::NotApplicable,
                },
            });
            assert!(segment.trace.unguarded_pending.is_empty());
            assert_eq!(segment.trace.unguarded_completed, expected);
        }
        assert_eq!(segment.trace.unguarded_effects(), saturating_i64(expected));
        segment.finish_agent(agent_run_id, AgentDisposition::Finished);
        segment.finish(RunDisposition::Finished, Some(&goal_view()));
        let spans = finished_spans(&telemetry, &exporter);
        for (name, key) in [
            ("invoke_workflow arany.run", "arany.run.unguarded_effects"),
            ("invoke_workflow arany.goal", "arany.goal.unguarded_effects"),
        ] {
            let span = spans.iter().find(|span| span.name == name).expect("span");
            assert!(has_attribute(span, key, saturating_i64(expected).into()));
        }
    }
}

fn has_attribute(span: &SpanData, key: &str, value: Value) -> bool {
    span.attributes
        .iter()
        .any(|attribute| attribute.key.as_str() == key && attribute.value == value)
}

fn unit_context() -> (SystemTime, SystemTime) {
    (
        committed_at(0),
        committed_at(0) + Duration::from_micros(250),
    )
}

fn supervisor_trace_verdict() -> crate::supervisor::SupervisorVerdict {
    use crate::supervisor::*;
    let signals = vec![
        Signal {
            signal: SignalKind::Loop,
            level: Level::WouldNarrow,
            would_response: Some(Response::Pause),
            features: Features::Loop(LoopFeatures {
                repeat_identical: u16::MAX,
                ..Default::default()
            }),
            evidence: vec![1],
        },
        Signal {
            signal: SignalKind::Probing,
            level: Level::WouldNarrow,
            would_response: Some(Response::Revoke),
            features: Features::Probing(Default::default()),
            evidence: vec![1],
        },
        Signal {
            signal: SignalKind::Disproportion,
            level: Level::WouldNarrow,
            would_response: Some(Response::RequireApproval),
            features: Features::Disproportion(Default::default()),
            evidence: vec![1],
        },
    ];
    SupervisorVerdict {
        subject_sequence: 1,
        site: Site::WorkerCapture,
        evaluator: 1,
        mode: Mode::Shadow,
        signals,
        capped: true,
    }
}

#[test]
fn supervisor_verdict_events_export_only_closed_signal_and_level_arrays() {
    let (telemetry, exporter) = in_memory();
    let mut segment = Segment::begin(&telemetry, unit_context());
    let actor = segment.spawn(AgentRole::Primary);
    segment.commit(Event::MessageCommitted {
        run_id: segment.run_id,
        text: "SUPERVISOR_CONTENT_CANARY".into(),
    });
    let mut verdict = supervisor_trace_verdict();
    verdict.signals[0].level = crate::supervisor::Level::Notice;
    verdict.signals[0].would_response = None;
    segment.commit(Event::SupervisorVerdictRecorded {
        run_id: RunId::new(),
        agent_run_id: actor,
        verdict: verdict.clone(),
    });
    let sequence = segment.commit(Event::SupervisorVerdictRecorded {
        run_id: segment.run_id,
        agent_run_id: actor,
        verdict,
    });
    segment.finish_agent(actor, AgentDisposition::Finished);
    segment.finish(RunDisposition::Finished, None);
    let spans = finished_spans(&telemetry, &exporter);
    let events: Vec<_> = spans
        .iter()
        .flat_map(|s| s.events.iter())
        .filter(|e| e.name == "arany.supervisor.verdict")
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].timestamp, committed_at(sequence));
    assert_eq!(
        events[0].attributes,
        vec![
            KeyValue::new(
                "arany.supervisor.signals",
                Value::Array(opentelemetry::Array::String(vec![
                    "loop".into(),
                    "probing".into(),
                    "disproportion".into()
                ]))
            ),
            KeyValue::new(
                "arany.supervisor.levels",
                Value::Array(opentelemetry::Array::String(vec![
                    "notice".into(),
                    "would_narrow".into(),
                    "would_narrow".into()
                ]))
            ),
        ]
    );
    let resource = ResourceAttributesWithSchema::from(&telemetry_resource());
    let body = ExportTraceServiceRequest {
        resource_spans: group_spans_by_resource_and_scope(spans, &resource),
    }
    .encode_to_vec();
    assert!(
        !body
            .windows(b"SUPERVISOR_CONTENT_CANARY".len())
            .any(|w| w == b"SUPERVISOR_CONTENT_CANARY"),
        "trace content canary leaked"
    );
}

#[test]
fn direct_and_team_span_trees_follow_admitted_agents() {
    for children in [0, 2, 8] {
        let (telemetry, exporter) = in_memory();
        let mut segment = Segment::routed(&telemetry, unit_context(), Some(child_routes()));
        let primary = segment.spawn(AgentRole::Primary);
        let first = if children > 0 {
            ProviderCallDisposition::Delegated
        } else {
            ProviderCallDisposition::Finished
        };
        segment.call(primary, AgentPhase::RootPlan, first, (Some(12), Some(4)));
        let spawned = (0..children)
            .map(|_| segment.spawn(AgentRole::Child))
            .collect::<Vec<_>>();
        for child in &spawned {
            segment.call(
                *child,
                AgentPhase::ChildWork,
                ProviderCallDisposition::Finished,
                (None, None),
            );
        }
        for child in spawned.into_iter().rev() {
            segment.finish_agent(child, AgentDisposition::Finished);
        }
        if children > 0 {
            segment.call(
                primary,
                AgentPhase::RootSynthesis,
                ProviderCallDisposition::Finished,
                (None, None),
            );
        }
        segment.finish_agent(primary, AgentDisposition::Finished);
        segment.finish(RunDisposition::Finished, None);
        let spans = finished_spans(&telemetry, &exporter);
        assert_eq!(
            spans.len(),
            if children == 0 { 4 } else { 6 + 2 * children }
        );
        let goal = spans
            .iter()
            .find(|span| span.name == "invoke_workflow arany.goal")
            .expect("Goal span");
        assert_eq!(goal.parent_span_id, SpanId::INVALID);
        let root = spans
            .iter()
            .find(|span| span.name == "invoke_workflow arany.run")
            .expect("workflow span");
        assert_eq!(root.parent_span_id, goal.span_context.span_id());
        let primary = spans
            .iter()
            .find(|span| span.name == "invoke_agent arany.primary")
            .expect("primary span");
        assert_eq!(primary.parent_span_id, root.span_context.span_id());
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.name == "invoke_agent arany.child")
                .count(),
            children
        );
        for child in spans
            .iter()
            .filter(|span| span.name == "invoke_agent arany.child")
        {
            assert_eq!(child.parent_span_id, primary.span_context.span_id());
        }
        let plan = spans.iter().find(|span| span.name == "plan arany.primary");
        assert_eq!(plan.is_some(), children > 0);
        if let Some(plan) = plan {
            assert_eq!(plan.parent_span_id, primary.span_context.span_id());
            assert!(spans.iter().any(|span| {
                span.name == "chat arany.provider"
                    && span.parent_span_id == plan.span_context.span_id()
            }));
        }
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.name == "chat arany.provider")
                .count(),
            if children == 0 { 1 } else { children + 2 }
        );
        let first_provider = spans
            .iter()
            .find(|span| {
                span.name == "chat arany.provider"
                    && has_attribute(span, "arany.provider.phase", "primary_plan".into())
            })
            .expect("primary Provider span");
        assert!(has_attribute(
            first_provider,
            "gen_ai.provider.name",
            "other".into()
        ));
        assert!(has_attribute(
            first_provider,
            "gen_ai.request.model",
            "model-1".into()
        ));
        assert!(has_attribute(
            first_provider,
            "gen_ai.usage.input_tokens",
            Value::I64(12)
        ));
        assert!(has_attribute(
            first_provider,
            "gen_ai.usage.output_tokens",
            Value::I64(4)
        ));
        assert!(
            !first_provider
                .attributes
                .iter()
                .any(|attribute| attribute.key.as_str() == "error.type")
        );
        for child_call in spans.iter().filter(|span| {
            span.name == "chat arany.provider"
                && has_attribute(span, "arany.provider.phase", "child_work".into())
        }) {
            assert!(has_attribute(
                child_call,
                "gen_ai.provider.name",
                "anthropic".into()
            ));
            assert!(has_attribute(
                child_call,
                "gen_ai.request.model",
                "child-model".into()
            ));
        }
        let trace_id = goal.span_context.trace_id();
        assert!(
            spans
                .iter()
                .all(|span| span.span_context.trace_id() == trace_id)
        );
    }
}

#[test]
fn spans_start_and_end_at_their_events_commit_times() {
    let (telemetry, exporter) = in_memory();
    let context = unit_context();
    let mut segment = Segment::begin(&telemetry, context);
    let primary = segment.spawn(AgentRole::Primary);
    let (reserved, recorded) = segment.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::Rejected,
        (None, None),
    );
    segment.finish_agent(primary, AgentDisposition::Failed);
    let finished = segment.sequence + 1;
    let mut goal_view = goal_view();
    goal_view.provider_calls = 1;
    segment.finish(RunDisposition::Failed, Some(&goal_view));
    let spans = finished_spans(&telemetry, &exporter);
    let span = |name: &str| {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("{name} span"))
    };
    for (name, started, ended) in [
        ("invoke_workflow arany.goal", 1, finished),
        ("invoke_workflow arany.run", 2, finished),
        ("invoke_agent arany.primary", 4, recorded + 1),
        ("chat arany.provider", reserved, recorded),
    ] {
        assert_eq!(span(name).start_time, committed_at(started), "{name} start");
        assert_eq!(span(name).end_time, committed_at(ended), "{name} end");
    }
    let provider = span("chat arany.provider");
    assert!(has_attribute(
        provider,
        "error.type",
        "provider_rejected".into()
    ));
    assert!(has_attribute(provider, "arany.outcome", "failed".into()));
    let goal = span("invoke_workflow arany.goal");
    assert!(has_attribute(goal, "arany.goal.status", "running".into()));
    assert!(has_attribute(goal, "arany.outcome", "failed".into()));
    assert!(has_attribute(
        goal,
        "arany.goal.provider_calls",
        Value::I64(1)
    ));
    let marker = &goal.events[0];
    assert_eq!(marker.timestamp, committed_at(1));
    let nanos = |time: SystemTime| {
        Value::I64(
            i64::try_from(
                time.duration_since(SystemTime::UNIX_EPOCH)
                    .expect("after epoch")
                    .as_nanos(),
            )
            .expect("bounded nanos"),
        )
    };
    let marker_value = |key: &str| {
        marker
            .attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| attribute.value.clone())
    };
    assert_eq!(
        marker_value("arany.event.kind"),
        Some("GoalRunLinked".into())
    );
    assert_eq!(
        marker_value("arany.context.started_unix_nano"),
        Some(nanos(context.0)),
        "context compilation rides on the next committed marker"
    );
    assert_eq!(
        marker_value("arany.context.ended_unix_nano"),
        Some(nanos(context.1))
    );
    assert!(
        spans
            .iter()
            .flat_map(|span| span.events.iter().skip(usize::from(span.name == goal.name)))
            .all(|event| event
                .attributes
                .iter()
                .all(|attribute| !attribute.key.as_str().starts_with("arany.context."))),
        "only the first marker carries the context timing"
    );
    let run = span("invoke_workflow arany.run");
    assert_eq!(
        run.events
            .iter()
            .map(|event| event.timestamp)
            .collect::<Vec<_>>(),
        [2, 3, 4, recorded, recorded + 1, finished].map(committed_at),
        "Run markers are timestamped at commit"
    );
}

#[test]
fn a_segment_without_its_run_receipt_stays_abandoned() {
    let (telemetry, exporter) = in_memory();
    let mut segment = Segment::begin(&telemetry, unit_context());
    let primary = segment.spawn(AgentRole::Primary);
    segment.commit(Event::GoalProviderCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        agent_run_id: primary,
        active_ceiling_ms: 1,
    });
    segment.trace.finish(None);
    let spans = finished_spans(&telemetry, &exporter);
    assert_eq!(
        spans.len(),
        3,
        "no Provider span exists without its committed record"
    );
    assert!(
        spans
            .iter()
            .all(|span| has_attribute(span, "arany.outcome", "abandoned".into()))
    );
}

#[test]
fn fallback_switches_mark_the_run_and_keep_each_call_on_its_requested_route() {
    let (telemetry, exporter) = in_memory();
    let mut routes = Providers::all(vec![identity("other", "model-1")]);
    routes.primary = vec![
        identity("openai", "primary-a"),
        identity("anthropic", "primary-b"),
        identity("openai", "primary-c"),
    ];
    routes.compaction = vec![
        identity("openai", "compact-a"),
        identity("anthropic", "compact-b"),
    ];
    let mut segment = Segment::routed(&telemetry, unit_context(), Some(routes.clone()));
    let primary = segment.spawn(AgentRole::Primary);
    let switch =
        |role, from: &ProviderIdentity, to: &ProviderIdentity| crate::provider::ProviderFallback {
            role,
            call_index: 0,
            failure: ProviderCallDisposition::Unavailable,
            from: from.clone(),
            to: to.clone(),
        };
    let reserved = segment.commit(Event::GoalProviderCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        agent_run_id: primary,
        active_ceiling_ms: 1,
    });
    let switched = segment.commit(Event::ProviderFallbackUsed {
        run_id: segment.run_id,
        agent_run_id: primary,
        fallback: switch(
            ProviderRole::Primary,
            &routes.primary[0],
            &routes.primary[1],
        ),
    });
    let switched_again = segment.commit(Event::ProviderFallbackUsed {
        run_id: segment.run_id,
        agent_run_id: primary,
        fallback: switch(
            ProviderRole::Primary,
            &routes.primary[1],
            &routes.primary[2],
        ),
    });
    let recorded = segment.commit(Event::ProviderCallRecorded {
        run_id: segment.run_id,
        agent_run_id: primary,
        record: provider_record(
            AgentPhase::RootPlan,
            ProviderCallDisposition::ToolRequested,
            (None, None),
        ),
    });
    segment.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    segment.commit(Event::GoalCompactionCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        active_ceiling_ms: 1,
    });
    segment.commit(Event::ProviderFallbackUsed {
        run_id: segment.run_id,
        agent_run_id: primary,
        fallback: switch(
            ProviderRole::Compaction,
            &routes.compaction[0],
            &routes.compaction[1],
        ),
    });
    let mut record = compaction_record(CompactionStatus::Failed {
        reason: CompactionFailure::ProviderUnavailable,
    });
    record.provider = "anthropic".into();
    record.model = "compact-b".into();
    segment.commit(Event::GoalContextCompacted {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        covered_tool_count: 1,
        record: record.clone(),
    });
    segment.commit(Event::GoalCompactionCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        active_ceiling_ms: 1,
    });
    segment.commit(Event::GoalContextCompacted {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        covered_tool_count: 2,
        record,
    });
    segment.finish_agent(primary, AgentDisposition::Finished);
    segment.finish(RunDisposition::Finished, None);
    let spans = finished_spans(&telemetry, &exporter);
    let routed = |name: &str, provider: &'static str, model: &'static str| {
        spans.iter().any(|span| {
            span.name == name
                && has_attribute(span, "gen_ai.provider.name", provider.into())
                && has_attribute(span, "gen_ai.request.model", model.into())
        })
    };
    let first = spans
        .iter()
        .find(|span| {
            span.start_time == committed_at(reserved) && span.name == "chat arany.provider"
        })
        .expect("first Provider span");
    assert_eq!(first.end_time, committed_at(recorded));
    assert!(
        has_attribute(first, "gen_ai.request.model", "primary-a".into()),
        "the logical call keeps the route it requested"
    );
    assert!(
        routed("chat arany.provider", "openai", "primary-c"),
        "the agent's next call follows the committed switch"
    );
    assert!(routed("chat arany.compaction", "openai", "compact-a"));
    assert!(routed("chat arany.compaction", "anthropic", "compact-b"));
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "chat arany.compaction")
            .count(),
        2
    );
    let run = spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.run")
        .expect("Run span");
    for sequence in [switched, switched_again] {
        let fallback = run
            .events
            .iter()
            .find(|event| event.timestamp == committed_at(sequence))
            .expect("fallback marker");
        assert!(fallback.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "arany.event.kind"
                && attribute.value == "ProviderFallbackUsed".into()
        }));
    }
}

#[test]
fn a_child_fallback_switch_keeps_other_agents_on_their_requested_routes() {
    let (telemetry, exporter) = in_memory();
    let mut routes = child_routes();
    routes.child.push(identity("openai", "child-fallback"));
    let mut segment = Segment::routed(&telemetry, unit_context(), Some(routes.clone()));
    let primary = segment.spawn(AgentRole::Primary);
    let child = segment.spawn(AgentRole::Child);
    let sibling = segment.spawn(AgentRole::Child);
    segment.commit(Event::GoalProviderCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        agent_run_id: child,
        active_ceiling_ms: 1,
    });
    segment.commit(Event::ProviderFallbackUsed {
        run_id: segment.run_id,
        agent_run_id: child,
        fallback: crate::provider::ProviderFallback {
            role: ProviderRole::Child,
            call_index: 0,
            failure: ProviderCallDisposition::Unavailable,
            from: routes.child[0].clone(),
            to: routes.child[1].clone(),
        },
    });
    segment.commit(Event::ProviderCallRecorded {
        run_id: segment.run_id,
        agent_run_id: child,
        record: provider_record(
            AgentPhase::ChildWork,
            ProviderCallDisposition::ToolRequested,
            (None, None),
        ),
    });
    let (child_call, _) = segment.call(
        child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    let (sibling_call, _) = segment.call(
        sibling,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    let (primary_call, _) = segment.call(
        primary,
        AgentPhase::RootSynthesis,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    for actor in [child, sibling, primary] {
        segment.finish_agent(actor, AgentDisposition::Finished);
    }
    segment.finish(RunDisposition::Finished, None);
    let spans = finished_spans(&telemetry, &exporter);
    let providers = spans
        .iter()
        .filter(|span| span.name == "chat arany.provider")
        .collect::<Vec<_>>();
    assert_eq!(providers.len(), 4);
    for (sequence, profile, model) in [
        (child_call, "openai", "child-fallback"),
        (sibling_call, "anthropic", "child-model"),
        (primary_call, "other", "model-1"),
    ] {
        let span = providers
            .iter()
            .find(|span| span.start_time == committed_at(sequence))
            .expect("agent call span");
        assert!(has_attribute(span, "gen_ai.provider.name", profile.into()));
        assert!(has_attribute(span, "gen_ai.request.model", model.into()));
    }
    assert!(has_attribute(
        providers[0],
        "gen_ai.request.model",
        "child-model".into()
    ));
}

fn goal_view() -> GoalView {
    let session_id = SessionId::new();
    let events = [
        Event::SessionStarted {
            title: "trace".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
        },
        Event::GoalStarted {
            goal_id: GoalId::new(),
            objective: "objective".into(),
            workspace_identity: WorkspaceIdentity {
                device: 1,
                inode: 2,
            },
            budget: crate::session::GoalBudget::default(),
            phase: crate::session::GoalPhase::Implementation,
            autonomous: false,
            resource_settings: crate::tools::ToolResourceSettings::default(),
            continues: None,
        },
    ]
    .into_iter()
    .zip(1..)
    .map(|(event, sequence)| EventEnvelope {
        sequence,
        session_id,
        run_id: None,
        agent_run_id: None,
        event,
        created_at_ms: commit_ms(sequence),
    })
    .collect::<Vec<_>>();
    crate::session::SessionView::replay(session_id, &events)
        .expect("replayed Goal")
        .expect("Session")
        .goals
        .remove(0)
}

fn compaction_record(status: CompactionStatus) -> crate::session::CompactionRecord {
    crate::session::CompactionRecord {
        covered_run_id: RunId::new(),
        covered_sequence: 1,
        source_digest: [0; 32],
        provider: "other".into(),
        model: "model-1".into(),
        prompt_version: 1,
        schema_version: 1,
        compiler_version: 3,
        source_bytes: 1,
        source_tokens_estimate: 1,
        input_tokens: Some(7),
        output_tokens: Some(3),
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        response_id: None,
        wire_provenance: None,
        chatgpt_provenance: None,
        output_token_bound: crate::provider::OutputTokenBound::ProviderEnforced,
        status,
    }
}

#[test]
fn fullest_mapped_batch_fits_trace_body_limit() {
    for supervisor in [false, true] {
        let (telemetry, exporter) = in_memory();
        let mut goal = goal_view();
        goal.provider_calls = u32::MAX;
        goal.known_tokens = u64::MAX;
        goal.active_ms = u64::MAX;
        goal.uncertain_active_ms = u64::MAX;
        for _ in 0..MAX_EXPORT_BATCH_SPANS / if supervisor { 1 } else { 2 } {
            let mut segment = Segment::begin(&telemetry, unit_context());
            let unknown = AgentRunId::new();
            // Each span holds its cap of markers with the longest kind names.
            for _ in 1..MAX_EVENTS_PER_SPAN {
                segment.commit(Event::GoalCompactionCallReserved {
                    goal_id: segment.goal_id,
                    run_id: segment.run_id,
                    active_ceiling_ms: u64::MAX,
                });
            }
            for _ in 3..MAX_EVENTS_PER_SPAN {
                if supervisor {
                    segment.commit(Event::SupervisorVerdictRecorded {
                        run_id: segment.run_id,
                        agent_run_id: unknown,
                        verdict: supervisor_trace_verdict(),
                    });
                } else {
                    segment.commit(Event::ProviderCallRecorded {
                        run_id: segment.run_id,
                        agent_run_id: unknown,
                        record: provider_record(
                            AgentPhase::RootSynthesis,
                            ProviderCallDisposition::Finished,
                            (None, None),
                        ),
                    });
                }
            }
            if supervisor {
                for _ in 0..256 {
                    segment.commit(Event::SupervisorVerdictRecorded {
                        run_id: segment.run_id,
                        agent_run_id: unknown,
                        verdict: supervisor_trace_verdict(),
                    });
                }
            }
            segment.finish(RunDisposition::Cancelled, Some(&goal));
        }
        let spans: Vec<_> = finished_spans(&telemetry, &exporter)
            .into_iter()
            .filter(|span| !supervisor || span.name == "invoke_workflow arany.run")
            .collect();
        if supervisor {
            assert!(
                spans
                    .iter()
                    .filter(|span| span.name == "invoke_workflow arany.run")
                    .all(|span| span.events.dropped_count == 256)
            );
        }
        assert_eq!(spans.len(), MAX_EXPORT_BATCH_SPANS);
        assert!(
            spans
                .iter()
                .all(|span| span.events.len() == MAX_EVENTS_PER_SPAN as usize)
        );
        let resource = ResourceAttributesWithSchema::from(&telemetry_resource());
        let request = ExportTraceServiceRequest {
            resource_spans: group_spans_by_resource_and_scope(spans, &resource),
        };
        assert!(
            request.encoded_len() <= MAX_TRACE_BODY_BYTES,
            "fullest mapped batch exceeds the OTLP body limit: supervisor={supervisor}, bytes={}",
            request.encoded_len()
        );
    }
}

#[test]
fn compaction_span_without_its_run_routing_receipt_is_not_exported() {
    let (telemetry, exporter) = in_memory();
    let mut segment = Segment::opened(&telemetry, unit_context());
    segment.commit(Event::GoalCompactionCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        active_ceiling_ms: 1,
    });
    segment.commit(Event::GoalContextCompacted {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        covered_tool_count: 1,
        record: compaction_record(CompactionStatus::Failed {
            reason: CompactionFailure::TimedOut,
        }),
    });
    segment.finish(RunDisposition::Finished, None);
    let spans = finished_spans(&telemetry, &exporter);
    assert!(
        spans
            .iter()
            .any(|span| span.name == "invoke_workflow arany.run")
    );
    assert!(
        !spans
            .iter()
            .any(|span| span.name == "compact arany.goal_tool_context"
                || span.name == "chat arany.compaction")
    );
}

#[test]
fn goal_context_compaction_spans_follow_its_reservation_and_receipt() {
    let (telemetry, exporter) = in_memory();
    let mut segment = Segment::begin(&telemetry, unit_context());
    let reserved = segment.commit(Event::GoalCompactionCallReserved {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        active_ceiling_ms: 1,
    });
    let compacted = segment.commit(Event::GoalContextCompacted {
        goal_id: segment.goal_id,
        run_id: segment.run_id,
        covered_tool_count: 1,
        record: compaction_record(CompactionStatus::Failed {
            reason: CompactionFailure::TimedOut,
        }),
    });
    segment.finish(RunDisposition::Finished, None);
    let spans = finished_spans(&telemetry, &exporter);
    let run = spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.run")
        .expect("Run span");
    let operation = spans
        .iter()
        .find(|span| span.name == "compact arany.goal_tool_context")
        .expect("compaction span");
    let provider = spans
        .iter()
        .find(|span| span.name == "chat arany.compaction")
        .expect("compaction Provider span");
    assert_eq!(operation.parent_span_id, run.span_context.span_id());
    assert_eq!(provider.parent_span_id, operation.span_context.span_id());
    for span in [operation, provider] {
        assert_eq!(span.start_time, committed_at(reserved));
        assert_eq!(span.end_time, committed_at(compacted));
    }
    assert!(has_attribute(provider, "error.type", "timeout".into()));
    assert!(has_attribute(
        provider,
        "gen_ai.usage.input_tokens",
        Value::I64(7)
    ));
    assert_eq!(operation.events.len(), 1);
}

#[test]
fn compaction_has_its_own_operation_and_committed_receipt() {
    let (telemetry, exporter) = in_memory();
    let session_id = SessionId::new();
    let trace = telemetry.begin_compaction(session_id, RunId::new(), TraceProvider::Other);
    let status = CompactionStatus::Failed {
        reason: CompactionFailure::InvalidOutcome,
    };
    let outcome = ProviderTraceOutcome::from(&status);
    trace
        .begin_provider("model-1")
        .finish_with_usage(outcome, None, None);
    trace.event_committed(&EventEnvelope {
        sequence: 42,
        session_id,
        run_id: None,
        agent_run_id: None,
        event: Event::ContextCompacted {
            record: compaction_record(status),
            active_ms: None,
        },
        created_at_ms: commit_ms(42),
    });
    trace.finish(outcome.into());
    let spans = finished_spans(&telemetry, &exporter);
    assert_eq!(spans.len(), 2);
    let operation = spans
        .iter()
        .find(|span| span.name == "compact arany.session")
        .expect("compaction span");
    let provider = spans
        .iter()
        .find(|span| span.name == "chat arany.compaction")
        .expect("compaction Provider span");
    assert_eq!(provider.parent_span_id, operation.span_context.span_id());
    assert!(has_attribute(
        provider,
        "error.type",
        "invalid_response".into()
    ));
    assert_eq!(operation.events.len(), 1);
    assert_eq!(operation.events[0].name, "arany.event.committed");
}

struct CompactionRouteProvider {
    identity: ProviderIdentity,
    calls: std::sync::Mutex<Vec<String>>,
    responses: std::sync::Mutex<
        std::collections::VecDeque<Result<crate::provider::CompactionResponse, ProviderError>>,
    >,
}

#[async_trait::async_trait]
impl Provider for CompactionRouteProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.calls.lock().expect("calls").push(request.model);
        Ok(ProviderResponse {
            turn: crate::provider::ProviderTurn::scripted(),
            outcome: ProviderOutcome::Answer(Answer {
                text: "done".into(),
            }),
            response_id: None,
            usage: crate::provider::Usage::default(),
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        request: crate::provider::CompactionRequest,
    ) -> Result<crate::provider::CompactionResponse, ProviderError> {
        self.calls.lock().expect("calls").push(request.model);
        self.responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("scripted compaction")
    }
}

#[tokio::test]
async fn session_compaction_span_uses_the_previously_switched_route() {
    let temp = tempfile::tempdir().expect("private trace root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let (telemetry, exporter) = in_memory();
    let first = Arc::new(CompactionRouteProvider {
        identity: identity("openai", "compact-a"),
        calls: Default::default(),
        // The failed compaction call and its three same-route retries.
        responses: std::sync::Mutex::new(
            (0..=crate::provider::MAX_PROVIDER_RETRIES)
                .map(|_| Err(ProviderError::RemoteHttp(503)))
                .collect(),
        ),
    });
    let second = Arc::new(CompactionRouteProvider {
        identity: identity("anthropic", "compact-b"),
        calls: Default::default(),
        responses: std::sync::Mutex::new(
            [
                Err(ProviderError::Rejected),
                Ok(crate::provider::CompactionResponse {
                    summary: "summary".into(),
                    response_id: None,
                    usage: crate::provider::Usage::default(),
                    wire_provenance: None,
                }),
            ]
            .into(),
        ),
    });
    let mut providers = crate::provider::AdmittedProviders::single(first.clone());
    providers.roles.compaction.push(second.clone());
    let state = temp.path().join("state");
    let engine = Engine::open(
        StateRoot::admit(&state).expect("private State"),
        EngineConfig {
            telemetry: telemetry.clone(),
            ..Default::default()
        },
    )
    .expect("Engine");
    let outcome = engine
        .submit(
            providers.clone(),
            RunRequest {
                skill: None,
                session_id: None,
                title: None,
                objective: "task".into(),
                images: Vec::new(),
                workspace: workspace.clone(),
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Single,
                goal: crate::engine::GoalRequest::default(),
            },
            RunControls::default(),
        )
        .expect("submission")
        .await
        .expect("Run")
        .run;
    assert_eq!(outcome.run.status, RunStatus::Finished);
    crate::fault::FaultPlan::skip_retry_backoff(outcome.session_id);
    let failed = engine
        .compact_session(&providers, outcome.session_id, workspace.clone())
        .await
        .expect("failed compaction receipt");
    assert!(matches!(failed.status, CompactionStatus::Failed { .. }));
    let succeeded = engine
        .compact_session(&providers, outcome.session_id, workspace)
        .await
        .expect("successful compaction receipt");
    assert!(matches!(
        succeeded.status,
        CompactionStatus::Succeeded { .. }
    ));
    assert_eq!(failed.model, "compact-b");
    assert_eq!(succeeded.model, "compact-b");
    engine.close().await.expect("Engine close");
    assert_eq!(*first.calls.lock().expect("first calls"), ["compact-a"; 5]);
    assert_eq!(
        *second.calls.lock().expect("second calls"),
        ["compact-b", "compact-b"]
    );
    assert!(first.responses.lock().expect("first responses").is_empty());
    assert!(
        second
            .responses
            .lock()
            .expect("second responses")
            .is_empty()
    );
    let store = crate::store::Store::open_read_only(
        StateRoot::open_existing(&state).expect("existing State"),
    )
    .expect("read-only Store");
    let events = store
        .load_session(outcome.session_id)
        .await
        .expect("committed Events");
    let view = crate::session::SessionView::replay(outcome.session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.compactions.len(), 2);
    assert_eq!(view.runs[0].provider_fallbacks.len(), 1);
    assert_eq!(view.runs[0].provider_retries.len(), 3);
    store.close().await.expect("Store close");
    let spans = finished_spans(&telemetry, &exporter);
    let compactions = spans
        .iter()
        .filter(|span| span.name == "chat arany.compaction")
        .collect::<Vec<_>>();
    assert_eq!(compactions.len(), 2);
    for (span, profile, model) in [
        (compactions[0], "openai", "compact-a"),
        (compactions[1], "anthropic", "compact-b"),
    ] {
        assert!(has_attribute(span, "gen_ai.provider.name", profile.into()));
        assert!(has_attribute(span, "gen_ai.request.model", model.into()));
    }
}

#[test]
fn provider_identity_maps_only_known_profiles_to_fixed_metadata() {
    for (profile, kind, address) in [
        ("openai", "openai", Some("api.openai.com")),
        ("anthropic", "anthropic", Some("api.anthropic.com")),
        ("custom:private-canary", "custom", None),
        ("private-canary", "other", None),
    ] {
        let mapped = TraceProvider::from_profile(profile);
        assert_eq!(mapped.name(), kind);
        assert_eq!(mapped.server_address(), address);
    }
}

#[test]
fn loopback_export_is_protobuf_with_a_minimal_resource() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("local Collector");
    listener.set_nonblocking(true).expect("bounded accept");
    let address = listener.local_addr().expect("Collector address");
    let collector = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "trace request missing");
                    thread::yield_now();
                }
                Err(error) => panic!("Collector accept failed: {error}"),
            }
        };
        stream
            .set_nonblocking(false)
            .expect("blocking accepted stream");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("request timeout");
        let mut request = Vec::new();
        let (header, body) = loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).expect("trace request bytes");
            assert!(count > 0 && request.len() + count <= 257 * 1024);
            request.extend_from_slice(&chunk[..count]);
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                let header_end = end + 4;
                let header = std::str::from_utf8(&request[..header_end])
                    .expect("trace header")
                    .to_owned();
                let length = header
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .expect("Content-Length");
                if request.len() >= header_end + length {
                    break (header, request[header_end..header_end + length].to_vec());
                }
            }
        };
        write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("Collector acknowledgment");
        (header, body)
    });
    let telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{address}"), false).expect("local endpoint"),
    )
    .expect("exporter construction");
    let mut segment = Segment::begin(&telemetry, unit_context());
    let primary = segment.spawn(AgentRole::Primary);
    segment.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::Rejected,
        (None, None),
    );
    segment.finish_agent(primary, AgentDisposition::Failed);
    segment.finish(RunDisposition::Failed, None);
    telemetry.shutdown();
    let (header, body) = collector.join().expect("Collector thread");
    assert!(header.starts_with("POST /v1/traces HTTP/1.1\r\n"));
    assert!(
        header
            .to_ascii_lowercase()
            .contains("content-type: application/x-protobuf")
    );
    assert!(!header.to_ascii_lowercase().contains("authorization:"));
    assert!(body.len() <= 256 * 1024);
    let request = ExportTraceServiceRequest::decode(body.as_slice()).expect("OTLP protobuf");
    assert_eq!(request.resource_spans.len(), 1);
    let resource_spans = &request.resource_spans[0];
    let mut resource_keys = resource_spans
        .resource
        .as_ref()
        .expect("explicit Resource")
        .attributes
        .iter()
        .map(|attribute| attribute.key.as_str())
        .collect::<Vec<_>>();
    resource_keys.sort_unstable();
    assert_eq!(
        resource_keys,
        [
            "service.instance.id",
            "service.name",
            "service.version",
            "telemetry.sdk.language",
            "telemetry.sdk.name",
            "telemetry.sdk.version",
        ]
    );
    assert_eq!(resource_spans.scope_spans.len(), 1);
    let scope = &resource_spans.scope_spans[0];
    assert_eq!(scope.scope.as_ref().expect("scope").name, "arany");
    assert_eq!(scope.spans.len(), 4);
    let provider = scope
        .spans
        .iter()
        .find(|span| span.name == "chat arany.provider")
        .expect("Provider span");
    assert!(provider.attributes.iter().any(|attribute| {
        attribute.key == "error.type"
            && matches!(
                attribute
                    .value
                    .as_ref()
                    .and_then(|value| value.value.as_ref()),
                Some(ProtoValue::StringValue(value)) if value == "provider_rejected"
            )
    }));
}

#[test]
fn collector_redirect_is_not_followed() {
    let source = TcpListener::bind("127.0.0.1:0").expect("source Collector");
    let target = TcpListener::bind("127.0.0.1:0").expect("redirect target");
    target.set_nonblocking(true).expect("target nonblocking");
    let address = source.local_addr().expect("source address");
    let target_address = target.local_addr().expect("target address");
    let collector = thread::spawn(move || {
        let (mut stream, _) = source.accept().expect("source request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("source read deadline");
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).expect("source bytes") > 0);
        write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/v1/traces\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("redirect response");
    });
    let telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{address}"), false).expect("source endpoint"),
    )
    .expect("exporter");
    Segment::begin(&telemetry, unit_context()).finish(RunDisposition::Finished, None);
    telemetry.shutdown();
    collector.join().expect("source Collector thread");
    assert!(matches!(
        target.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn stalled_collector_cannot_hold_shutdown_open() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("slow Collector");
    let address = listener.local_addr().expect("slow Collector address");
    let collector = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("slow Collector request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("slow Collector read deadline");
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).expect("slow request") > 0);
        thread::sleep(Duration::from_millis(900));
    });
    let telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{address}"), false).expect("slow endpoint"),
    )
    .expect("exporter");
    Segment::begin(&telemetry, unit_context()).finish(RunDisposition::Finished, None);
    let started = Instant::now();
    telemetry.shutdown();
    assert!(started.elapsed() < Duration::from_millis(800));
    collector.join().expect("slow Collector thread");
}

#[test]
#[ignore = "native Linux and macOS trickle-response OTLP deadline release gate"]
fn trickling_collector_cannot_hold_the_export_worker() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("local Collector");
    listener.set_nonblocking(true).expect("bounded accept");
    let address = listener.local_addr().expect("Collector address");
    let (first_seen_tx, first_seen_rx) = std::sync::mpsc::sync_channel(1);
    let collector = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut first, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "first export missing");
                    thread::yield_now();
                }
                Err(error) => panic!("Collector accept failed: {error}"),
            }
        };
        first
            .set_nonblocking(false)
            .expect("blocking first Collector connection");
        first
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("first request timeout");
        let mut bytes = [0; 4096];
        assert!(first.read(&mut bytes).expect("first request") > 0);
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nConnection: close\r\n\r\n")
            .expect("trickle headers");
        first_seen_tx.send(()).expect("first export marker");
        let trickle = thread::spawn(move || {
            for _ in 0..20 {
                if first.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(150));
            }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let second_seen = loop {
            match listener.accept() {
                Ok((mut second, _)) => {
                    second
                        .set_nonblocking(false)
                        .expect("blocking second Collector connection");
                    second
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("second request timeout");
                    assert!(second.read(&mut bytes).expect("second request") > 0);
                    second
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .expect("second acknowledgment");
                    break true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        break false;
                    }
                    thread::yield_now();
                }
                Err(error) => panic!("Collector accept failed: {error}"),
            }
        };
        trickle.join().expect("trickle thread");
        second_seen
    });
    let telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{address}"), false).expect("local endpoint"),
    )
    .expect("exporter");
    Segment::begin(&telemetry, unit_context()).finish(RunDisposition::Finished, None);
    first_seen_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("first export started");
    Segment::begin(&telemetry, unit_context()).finish(RunDisposition::Finished, None);
    let second_seen = collector.join().expect("Collector thread");
    telemetry.shutdown();
    assert!(second_seen, "trickling response held the export worker");
}

#[test]
fn endpoint_precedence_and_loopback_admission_are_closed() {
    let cases = [
        (
            "http://127.0.0.1:4318",
            false,
            "http://127.0.0.1:4318/v1/traces",
        ),
        (
            "http://[::1]:4318/otel",
            false,
            "http://[::1]:4318/otel/v1/traces",
        ),
        (
            "http://127.3.2.1:4318/v1/custom",
            true,
            "http://127.3.2.1:4318/v1/custom",
        ),
    ];
    for (input, exact, expected) in cases {
        assert_eq!(
            parse_endpoint(input, exact).expect("local endpoint").0,
            expected
        );
    }
    for input in [
        "https://127.0.0.1:4318",
        "http://localhost:4318",
        "http://192.168.1.1:4318",
        "http://[::ffff:127.0.0.1]:4318",
        "http://2130706433:4318",
        "http://0177.0.0.1:4318",
        "http://user:pass@127.0.0.1:4318",
        "http://127.0.0.1:4318/?key=secret",
        "http://127.0.0.1:4318/#fragment",
        "http://127.0.0.1:0",
    ] {
        assert!(
            parse_endpoint(input, false).is_err(),
            "invalid endpoint case"
        );
    }
    let long_path = format!("http://127.0.0.1:4318/{}", "a".repeat(250));
    assert!(parse_endpoint(&long_path, false).is_err());
    assert!(parse_endpoint(&long_path, true).is_ok());
    let lookup = |name: &str| match name {
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT" => Some(OsString::from("http://[::1]:4318/exact")),
        "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(OsString::from("http://127.0.0.1:4318/base")),
        _ => None,
    };
    assert_eq!(
        resolve_endpoint(Some("http://127.0.0.1:4318/cli"), lookup)
            .expect("CLI precedence")
            .expect("enabled")
            .0,
        "http://127.0.0.1:4318/cli/v1/traces"
    );
    assert_eq!(
        resolve_endpoint(None, lookup)
            .expect("traces precedence")
            .expect("enabled")
            .0,
        "http://[::1]:4318/exact"
    );
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        assert!(
            resolve_endpoint(Some("http://127.0.0.1:4318"), |name| {
                name.ends_with("_ENDPOINT")
                    .then(|| OsString::from_vec(vec![0xff]))
            })
            .is_ok()
        );
    }
}

#[test]
fn unsupported_ambient_configuration_is_rejected_by_name_only() {
    let lookup = |name: &str| match name {
        "OTEL_EXPORTER_OTLP_ENDPOINT" => Some(OsString::from("http://127.0.0.1:4318")),
        "OTEL_EXPORTER_OTLP_HEADERS" => Some(OsString::from("private-key-canary")),
        _ => None,
    };
    assert!(matches!(
        resolve_endpoint(None, lookup),
        Err(TelemetryConfigError::UnsupportedVariable(
            "OTEL_EXPORTER_OTLP_HEADERS"
        ))
    ));
    assert!(
        resolve_endpoint(None, |_| None)
            .expect("disabled")
            .is_none()
    );
}

#[test]
fn assigned_child_telemetry_uses_its_pinned_profile_instead_of_the_child_default() {
    let (telemetry, exporter) = in_memory();
    let routes = child_routes();
    let mut config = run_config(Some(routes.clone()));
    config.policy = CollaborationPolicy::Team {
        max_active_children: 2,
    };
    let mut roster = crate::RosterSnapshot {
        preset: "coding".into(),
        selection_digest: [0; 32],
        profiles: vec![
            crate::NamedProviderRoutes {
                name: "child".into(),
                routes: routes.child.clone(),
            },
            crate::NamedProviderRoutes {
                name: "main".into(),
                routes: routes.primary.clone(),
            },
            crate::NamedProviderRoutes {
                name: "spare".into(),
                routes: vec![identity("openai", "assigned-model")],
            },
        ],
        providers: Providers {
            primary: "main".into(),
            child: "child".into(),
            review: "main".into(),
            compaction: "main".into(),
        },
        child_profiles: vec!["child".into(), "spare".into()],
    };
    roster.selection_digest = roster.digest().unwrap();
    config.roster = Some(Box::new(roster));
    let mut segment = Segment::opened(&telemetry, unit_context());
    segment.commit(Event::RunStarted {
        run_id: segment.run_id,
        config,
        active_deadline_ms: None,
    });
    let primary = segment.spawn_as(AgentRole::Primary, Some("main"));
    segment.call(
        primary,
        AgentPhase::RootPlan,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    let child = AgentRunId::new();
    segment.commit(Event::AgentSpawned {
        run_id: segment.run_id,
        agent_run_id: child,
        role: AgentRole::Child,
        ordinal: 1,
        objective: Some("Inspect".into()),
        grant: Some(crate::ChildGrant::NoTools),
        definition: None,
        profile: Some("spare".into()),
        assignment: None,
    });
    let sibling = segment.spawn_as(AgentRole::Child, Some("child"));
    let (selected, _) = segment.call(
        child,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    let (default, _) = segment.call(
        sibling,
        AgentPhase::ChildWork,
        ProviderCallDisposition::Finished,
        (None, None),
    );
    for actor in [child, sibling, primary] {
        segment.finish_agent(actor, AgentDisposition::Finished);
    }
    segment.finish(RunDisposition::Finished, None);
    let spans = finished_spans(&telemetry, &exporter);
    for (sequence, model) in [(selected, "assigned-model"), (default, "child-model")] {
        let span = spans
            .iter()
            .find(|span| {
                span.name == "chat arany.provider" && span.start_time == committed_at(sequence)
            })
            .unwrap();
        assert!(has_attribute(span, "gen_ai.request.model", model.into()));
    }
}

#[test]
fn worker_recovery_history_and_lineage_export_only_closed_metadata() {
    let f = crate::session::worker_protocol_fixture();
    let (telemetry, exporter) = in_memory();
    let mut trace = telemetry
        .trace_run(f.session, f.goal, unit_context())
        .unwrap();
    let mut events = f.envelopes();
    let canary = "WORKER_PRIVATE_TRACE_CANARY_601";
    for event in &mut events {
        match &mut event.event {
            Event::AgentSpawned {
                objective: Some(text),
                assignment: Some(pins),
                ..
            } => {
                *text = canary.into();
                pins.request.direction = canary.into();
                pins.request.resumes = Some(AgentRunId::new());
            }
            Event::WorkerDataChunk { record, .. } => record.content = canary.into(),
            Event::AgentFinished {
                result: Some(text), ..
            } => *text = canary.into(),
            _ => {}
        }
        trace.committed(event);
    }
    trace.finish(None);
    let spans = finished_spans(&telemetry, &exporter);
    assert!(!spans.is_empty());
    assert!(
        !format!("{spans:?}").contains(canary),
        "worker content leaked through telemetry"
    );
}
