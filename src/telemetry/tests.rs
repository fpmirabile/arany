use super::config::{parse_endpoint, resolve_endpoint};
use super::*;
use crate::engine::{Engine, RunRequest};
use crate::provider::{
    Delegate, Finish, Provider, ProviderError, ProviderOutcome, ProviderRequest, ProviderResponse,
};
use crate::session::{AgentStatus, CollaborationPolicy, RunStatus};
use crate::store::StateRoot;
use opentelemetry::Value;
use opentelemetry::trace::SpanId;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as ProtoValue;
use opentelemetry_proto::transform::common::tonic::ResourceAttributesWithSchema;
use opentelemetry_proto::transform::trace::tonic::group_spans_by_resource_and_scope;
use opentelemetry_sdk::trace::InMemorySpanExporter;
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

#[cfg(target_os = "linux")]
mod performance;

fn in_memory() -> (Telemetry, InMemorySpanExporter) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .with_sampler(Sampler::AlwaysOn)
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

impl Provider for WaitingTraceProvider {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "test-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        1
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests.lock().expect("requests").push(request);
        self.started.send(()).expect("Provider start gate");
        pending().await
    }
}

impl Provider for GatedFailureProvider {
    fn profile_name(&self) -> &str {
        "scripted"
    }

    fn model_name(&self) -> &str {
        "test-model"
    }

    fn max_concurrent_calls(&self) -> u8 {
        2
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        self.requests
            .lock()
            .expect("requests")
            .push(request.clone());
        let outcome = match request.phase {
            AgentPhase::RootPlan => ProviderOutcome::Delegate(Delegate {
                children: vec!["A".into(), "B".into()],
            }),
            AgentPhase::ChildWork => {
                let (release, gate) = oneshot::channel();
                self.started
                    .send((request.objective.clone(), release))
                    .map_err(|_| ProviderError::Unavailable)?;
                gate.await.map_err(|_| ProviderError::Unavailable)?;
                if request.objective == "B" {
                    match self.failure {
                        ChildFailureCase::Rejected => return Err(ProviderError::Rejected),
                        ChildFailureCase::NestedDelegation => {
                            return Ok(ProviderResponse {
                                outcome: ProviderOutcome::Delegate(Delegate {
                                    children: vec!["not permitted".into()],
                                }),
                                response_id: None,
                                input_tokens: Some(10),
                                output_tokens: Some(5),
                                wire_provenance: None,
                            });
                        }
                        ChildFailureCase::OutputLimit => {
                            return Ok(ProviderResponse {
                                outcome: ProviderOutcome::Finish(Finish {
                                    summary: "done".into(),
                                    result: "done".into(),
                                }),
                                response_id: None,
                                input_tokens: Some(10),
                                output_tokens: Some(request.max_output_tokens + 1),
                                wire_provenance: None,
                            });
                        }
                        ChildFailureCase::InputUsageOverflow => {
                            return Ok(ProviderResponse {
                                outcome: ProviderOutcome::Finish(Finish {
                                    summary: "done".into(),
                                    result: "done".into(),
                                }),
                                response_id: None,
                                input_tokens: Some(1_000_001),
                                output_tokens: Some(5),
                                wire_provenance: None,
                            });
                        }
                    }
                }
                ProviderOutcome::Finish(Finish {
                    summary: "done".into(),
                    result: "done".into(),
                })
            }
            AgentPhase::RootSynthesis | AgentPhase::ToolReview => {
                return Err(ProviderError::InvalidOutcome);
            }
        };
        Ok(ProviderResponse {
            outcome,
            response_id: None,
            input_tokens: Some(10),
            output_tokens: Some(5),
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
            let mut engine = Engine::open_with_telemetry(
                StateRoot::admit(&temp.path().join("state")).expect("private State"),
                provider,
                telemetry.clone(),
            )
            .expect("traced Engine");
            let request = RunRequest {
                session_id: None,
                title: None,
                objective: "synthetic task".into(),
                images: Vec::new(),
                workspace,
                include_paths: Vec::new(),
                policy: CollaborationPolicy::Team {
                    max_active_children: 2,
                },
            };
            let mut run = tokio::spawn(async move {
                let outcome = engine.run(request).await;
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
                children.sort_by(|left, right| left.objective.cmp(&right.objective));
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
        let mut engine = Engine::open_with_telemetry(
            StateRoot::admit(&temp.path().join("state")).expect("private State"),
            provider,
            telemetry.clone(),
        )
        .expect("traced Engine");
        let request = RunRequest {
            session_id: None,
            title: None,
            objective: "synthetic task".into(),
            images: Vec::new(),
            workspace,
            include_paths: Vec::new(),
            policy: CollaborationPolicy::Single,
        };
        let mut run = tokio::spawn(async move {
            let outcome = engine.run(request).await;
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
    assert!(actual.objective == objective, "trace call objective");
    assert!(actual.images.is_empty(), "trace call images");
    assert!(actual.instructions.is_none(), "trace call instructions");
    assert!(actual.includes.is_empty(), "trace call includes");
    assert!(actual.history.is_empty(), "trace call history");
    assert!(actual.context_summary.is_none(), "trace call summary");
    assert!(actual.child_results.is_empty(), "trace call children");
    assert_eq!(actual.max_output_tokens, 4096, "trace call output cap");
}

#[test]
fn direct_and_team_span_trees_follow_admitted_agents() {
    for children in [0, 2, 8] {
        let (telemetry, exporter) = in_memory();
        let session_id = SessionId::new();
        let run_id = RunId::new();
        let run = telemetry.begin_run(session_id, run_id, TraceProvider::Other, None);
        let primary = run.begin_primary(AgentRunId::new());
        let first = primary.begin_first_provider();
        first.finish(
            &primary,
            "model-1",
            children > 0,
            ProviderTraceOutcome::Finished,
            Some(12),
            Some(4),
        );
        let mut child_spans = Vec::new();
        for _ in 0..children {
            let child = primary.begin_child(AgentRunId::new());
            child
                .begin_provider(AgentPhase::ChildWork, "model-1")
                .finish_with_usage(ProviderTraceOutcome::Finished, None, None);
            child_spans.push(child);
        }
        for child in child_spans.into_iter().rev() {
            child.finish(TraceOutcome::Finished);
        }
        if children > 0 {
            primary
                .begin_provider(AgentPhase::RootSynthesis, "model-1")
                .finish_with_usage(ProviderTraceOutcome::Finished, None, None);
        }
        primary.finish(TraceOutcome::Finished);
        run.finish(TraceOutcome::Finished);
        telemetry
            .inner
            .as_ref()
            .expect("enabled")
            .provider
            .force_flush()
            .expect("in-memory flush");
        let spans = exporter.get_finished_spans().expect("finished spans");
        assert_eq!(
            spans.len(),
            if children == 0 { 3 } else { 5 + 2 * children }
        );
        let root = spans
            .iter()
            .find(|span| span.name == "invoke_workflow arany.run")
            .expect("workflow span");
        assert_eq!(root.parent_span_id, SpanId::INVALID);
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
                    && span.attributes.iter().any(|attribute| {
                        attribute.key.as_str() == "arany.provider.phase"
                            && attribute.value == Value::String("primary_plan".into())
                    })
            })
            .expect("primary Provider span");
        assert!(first_provider.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "gen_ai.provider.name"
                && attribute.value == Value::String("other".into())
        }));
        assert!(first_provider.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "gen_ai.usage.input_tokens"
                && attribute.value == Value::I64(12)
        }));
        assert!(first_provider.attributes.iter().any(|attribute| {
            attribute.key.as_str() == "gen_ai.usage.output_tokens"
                && attribute.value == Value::I64(4)
        }));
        assert!(
            !first_provider
                .attributes
                .iter()
                .any(|attribute| attribute.key.as_str() == "error.type")
        );
        let trace_id = root.span_context.trace_id();
        assert!(
            spans
                .iter()
                .all(|span| span.span_context.trace_id() == trace_id)
        );
    }
}

#[test]
fn fullest_mapped_batch_fits_trace_body_limit() {
    let (telemetry, exporter) = in_memory();
    for _ in 0..MAX_EXPORT_BATCH_SPANS {
        let trace = telemetry.begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None);
        for sequence in 1..=MAX_EVENTS_PER_SPAN {
            trace.event_committed(u64::from(sequence), TraceEventKind::ContextCompacted);
        }
        trace.finish(TraceOutcome::Finished);
    }
    telemetry
        .inner
        .as_ref()
        .expect("enabled")
        .provider
        .force_flush()
        .expect("in-memory flush");
    let spans = exporter.get_finished_spans().expect("finished spans");
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
        "fullest mapped batch exceeds the OTLP body limit"
    );
}

#[test]
fn compaction_has_its_own_operation_and_committed_receipt() {
    let (telemetry, exporter) = in_memory();
    let trace = telemetry.begin_compaction(SessionId::new(), RunId::new(), TraceProvider::Other);
    trace.begin_provider("model-1").finish_with_usage(
        ProviderTraceOutcome::Failed(TraceFailure::InvalidResponse),
        None,
        None,
    );
    trace.event_committed(42);
    trace.finish(TraceOutcome::Failed);
    telemetry
        .inner
        .as_ref()
        .expect("enabled")
        .provider
        .force_flush()
        .expect("in-memory flush");
    let spans = exporter.get_finished_spans().expect("finished spans");
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
    assert!(provider.attributes.iter().any(|attribute| {
        attribute.key.as_str() == "error.type"
            && attribute.value == Value::String("invalid_response".into())
    }));
    assert_eq!(operation.events.len(), 1);
    assert_eq!(operation.events[0].name, "arany.event.committed");
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
    let run = telemetry.begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None);
    let primary = run.begin_primary(AgentRunId::new());
    primary.begin_first_provider().finish(
        &primary,
        "model-1",
        false,
        ProviderTraceOutcome::Failed(TraceFailure::Rejected),
        None,
        None,
    );
    primary.finish(TraceOutcome::Failed);
    run.finish(TraceOutcome::Failed);
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
    assert_eq!(scope.spans.len(), 3);
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
    telemetry
        .begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None)
        .finish(TraceOutcome::Finished);
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
    telemetry
        .begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None)
        .finish(TraceOutcome::Finished);
    let started = Instant::now();
    telemetry.shutdown();
    assert!(started.elapsed() < Duration::from_millis(800));
    collector.join().expect("slow Collector thread");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "native Linux trickle-response OTLP deadline release gate"]
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
    telemetry
        .begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None)
        .finish(TraceOutcome::Finished);
    first_seen_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("first export started");
    telemetry
        .begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None)
        .finish(TraceOutcome::Finished);
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
