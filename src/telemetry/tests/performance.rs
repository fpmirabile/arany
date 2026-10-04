use super::super::{
    MAX_EVENTS_PER_SPAN, MAX_EXPORT_BATCH_SPANS, MAX_QUEUED_SPANS, MAX_TRACE_BODY_BYTES,
    SHUTDOWN_TIMEOUT, Telemetry, TraceEventKind, TraceOutcome, TraceProvider,
    config::parse_endpoint,
};
use crate::session::{RunId, SessionId};
use crate::{
    engine::{Engine, RunRequest},
    provider::{
        AgentPhase, Finish, Provider, ProviderError, ProviderOutcome, ProviderRequest,
        ProviderResponse,
    },
    session::{CollaborationPolicy, RunStatus, SessionView},
    store::{StateRoot, Store},
};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const OFFERED_SPANS: usize = 1_024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const WARMUPS: usize = 10;
const SAMPLES: usize = 100;

fn rss_kib() -> usize {
    let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").expect("test-process RSS");
    rollup
        .lines()
        .find_map(|line| {
            line.strip_prefix("Rss:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<usize>().ok())
        })
        .expect("test-process RSS field")
}

fn read_one_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("Collector read deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("Collector request bytes");
        assert!(count > 0, "Collector request closed early");
        assert!(
            request.len() + count <= MAX_HEADER_BYTES + MAX_TRACE_BODY_BYTES,
            "bounded Collector request"
        );
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = end + 4;
            assert!(header_end <= MAX_HEADER_BYTES, "bounded Collector header");
            let header = std::str::from_utf8(&request[..header_end]).expect("Collector header");
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("Collector Content-Length");
            assert!(length <= MAX_TRACE_BODY_BYTES, "bounded OTLP body");
            if request.len() >= header_end + length {
                return request[header_end..header_end + length].to_vec();
            }
        } else {
            assert!(
                request.len() <= MAX_HEADER_BYTES,
                "bounded Collector header"
            );
        }
    }
}

fn healthy_collector(expected_spans: usize) -> (SocketAddr, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Collector");
    listener
        .set_nonblocking(true)
        .expect("nonblocking Collector accept");
    let address = listener.local_addr().expect("Collector address");
    let collector = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut received = 0;
        while received < expected_spans {
            let mut stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "Collector span deadline");
                    thread::yield_now();
                    continue;
                }
                Err(error) => panic!("Collector accept: {error}"),
            };
            let body = read_one_request(&mut stream);
            let export = ExportTraceServiceRequest::decode(body.as_slice())
                .expect("bounded OTLP protobuf request");
            received += export
                .resource_spans
                .iter()
                .flat_map(|resource| &resource.scope_spans)
                .map(|scope| scope.spans.len())
                .sum::<usize>();
            assert!(received <= expected_spans, "unexpected extra trace spans");
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .expect("Collector acknowledgment");
        }
        received
    });
    (address, collector)
}

fn percentiles(samples: &mut [Duration]) -> (Duration, Duration, Duration) {
    samples.sort_unstable();
    (
        samples[samples.len() / 2],
        samples[samples.len() * 95 / 100],
        samples[samples.len() - 1],
    )
}

struct FastProvider;

impl Provider for FastProvider {
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
        assert!(matches!(request.phase, AgentPhase::RootPlan));
        Ok(ProviderResponse {
            outcome: ProviderOutcome::Finish(Finish {
                summary: "done".into(),
                result: "synthetic answer".into(),
            }),
            response_id: None,
            input_tokens: None,
            output_tokens: Some(8),
            wire_provenance: None,
        })
    }
}

async fn timed_direct_run(
    engine: &mut Engine<FastProvider>,
    observer: &Store,
    workspace: &Path,
) -> Duration {
    let request = RunRequest {
        session_id: None,
        title: None,
        objective: "synthetic objective".into(),
        images: Vec::new(),
        workspace: workspace.to_path_buf(),
        include_paths: Vec::new(),
        policy: CollaborationPolicy::Single,
    };
    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(5), engine.run(request))
        .await
        .expect("bounded direct Run")
        .expect("successful direct Run");
    let elapsed = started.elapsed();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(
        outcome.run.assistant_message.as_deref(),
        Some("synthetic answer")
    );
    let events = observer
        .load_session(outcome.session_id)
        .await
        .expect("committed Events");
    let view = SessionView::replay(outcome.session_id, &events)
        .expect("strict replay")
        .expect("committed Session");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0], outcome.run);
    elapsed
}

#[test]
#[ignore = "run with cargo test --release telemetry::tests::performance::healthy_collector_span_end_and_run_delta -- --ignored --nocapture"]
fn healthy_collector_span_end_and_run_delta() {
    let test_binary = std::env::current_exe().expect("test binary");
    assert!(
        test_binary
            .components()
            .any(|part| part.as_os_str() == "release"),
        "release profile required"
    );
    const HOT_SPANS: usize = 1_024;
    let (hot_address, hot_collector) = healthy_collector(HOT_SPANS);
    let hot_telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{hot_address}"), false).expect("local endpoint"),
    )
    .expect("healthy exporter");
    let mut span_end_samples = Vec::with_capacity(HOT_SPANS);
    for index in 0..HOT_SPANS {
        let run =
            hot_telemetry.begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None);
        let started = Instant::now();
        run.finish(TraceOutcome::Finished);
        span_end_samples.push(started.elapsed());
        if index % MAX_EXPORT_BATCH_SPANS == MAX_EXPORT_BATCH_SPANS - 1 {
            hot_telemetry
                .inner
                .as_ref()
                .expect("enabled telemetry")
                .provider
                .force_flush()
                .expect("healthy batch export");
        }
    }
    assert_eq!(hot_collector.join().expect("healthy Collector"), HOT_SPANS);
    hot_telemetry.shutdown();
    let (span_p50, span_p95, span_max) = percentiles(&mut span_end_samples);

    let temp = tempfile::tempdir().expect("private performance root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("empty Workspace");
    let disabled_state = temp.path().join("disabled-state");
    let enabled_state = temp.path().join("enabled-state");
    let (run_address, run_collector) = healthy_collector(4 * (WARMUPS + SAMPLES));
    let run_telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{run_address}"), false).expect("local endpoint"),
    )
    .expect("healthy exporter");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("performance runtime");
    let mut disabled = Engine::open(
        StateRoot::admit(&disabled_state).expect("private disabled State"),
        FastProvider,
    )
    .expect("disabled Engine");
    let mut enabled = Engine::open_with_telemetry(
        StateRoot::admit(&enabled_state).expect("private enabled State"),
        FastProvider,
        run_telemetry.clone(),
    )
    .expect("enabled Engine");
    let disabled_observer =
        Store::open_read_only(StateRoot::open_existing(&disabled_state).expect("disabled State"))
            .expect("disabled observer");
    let enabled_observer =
        Store::open_read_only(StateRoot::open_existing(&enabled_state).expect("enabled State"))
            .expect("enabled observer");
    let mut disabled_samples = Vec::with_capacity(SAMPLES);
    let mut enabled_samples = Vec::with_capacity(SAMPLES);
    for index in 0..WARMUPS + SAMPLES {
        let (disabled_elapsed, enabled_elapsed) = if index % 2 == 0 {
            let first = runtime.block_on(timed_direct_run(
                &mut disabled,
                &disabled_observer,
                &workspace,
            ));
            let second = runtime.block_on(timed_direct_run(
                &mut enabled,
                &enabled_observer,
                &workspace,
            ));
            (first, second)
        } else {
            let first = runtime.block_on(timed_direct_run(
                &mut enabled,
                &enabled_observer,
                &workspace,
            ));
            let second = runtime.block_on(timed_direct_run(
                &mut disabled,
                &disabled_observer,
                &workspace,
            ));
            (second, first)
        };
        if index >= WARMUPS {
            disabled_samples.push(disabled_elapsed);
            enabled_samples.push(enabled_elapsed);
        }
        if index % 16 == 15 {
            run_telemetry
                .inner
                .as_ref()
                .expect("enabled telemetry")
                .provider
                .force_flush()
                .expect("healthy Run trace export");
        }
    }
    run_telemetry
        .inner
        .as_ref()
        .expect("enabled telemetry")
        .provider
        .force_flush()
        .expect("final Run trace export");
    runtime
        .block_on(disabled_observer.close())
        .expect("disabled observer close");
    runtime
        .block_on(enabled_observer.close())
        .expect("enabled observer close");
    runtime
        .block_on(disabled.close())
        .expect("disabled Engine close");
    runtime
        .block_on(enabled.close())
        .expect("enabled Engine close");
    drop(runtime);
    assert_eq!(
        run_collector.join().expect("healthy Run Collector"),
        4 * (WARMUPS + SAMPLES)
    );
    run_telemetry.shutdown();
    let (disabled_p50, disabled_p95, disabled_max) = percentiles(&mut disabled_samples);
    let (enabled_p50, enabled_p95, enabled_max) = percentiles(&mut enabled_samples);
    let p95_delta = enabled_p95.saturating_sub(disabled_p95);
    println!(
        "OTLP healthy: span-end p50/p95/max {span_p50:?}/{span_p95:?}/{span_max:?}; disabled Run p50/p95/max {disabled_p50:?}/{disabled_p95:?}/{disabled_max:?}; enabled Run p50/p95/max {enabled_p50:?}/{enabled_p95:?}/{enabled_max:?}; p95 delta {p95_delta:?}"
    );
    assert!(
        span_p95 <= Duration::from_micros(50),
        "healthy span-end budget"
    );
    assert!(
        p95_delta <= Duration::from_millis(5),
        "healthy Run delta budget"
    );
}

#[test]
#[ignore = "run with cargo test --release telemetry::tests::performance::saturated_collector_keeps_span_end_bounded -- --ignored --nocapture"]
fn saturated_collector_keeps_span_end_bounded() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Collector");
    listener
        .set_nonblocking(true)
        .expect("nonblocking Collector accept");
    let address = listener.local_addr().expect("Collector address");
    let (accepted_tx, accepted_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let collector = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "Collector accept deadline");
                    thread::yield_now();
                }
                Err(error) => panic!("Collector accept: {error}"),
            }
        };
        drop(read_one_request(&mut stream));
        accepted_tx.send(()).expect("first export request ready");
        release_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Collector release deadline");
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    });
    let telemetry = Telemetry::from_endpoint(
        parse_endpoint(&format!("http://{address}"), false).expect("numeric-loopback endpoint"),
    )
    .expect("bounded exporter");
    for _ in 0..MAX_EXPORT_BATCH_SPANS {
        let run = telemetry.begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None);
        for sequence in 1..=MAX_EVENTS_PER_SPAN {
            run.event_committed(u64::from(sequence), TraceEventKind::RunStarted);
        }
        run.finish(TraceOutcome::Finished);
    }
    accepted_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first batch in flight");
    let idle_rss_kib = rss_kib();
    let mut peak_rss_kib = idle_rss_kib;
    let mut span_end_us = Vec::with_capacity(OFFERED_SPANS);
    let started = Instant::now();
    for index in 0..OFFERED_SPANS {
        let run = telemetry.begin_run(SessionId::new(), RunId::new(), TraceProvider::Other, None);
        for sequence in 1..=MAX_EVENTS_PER_SPAN {
            run.event_committed(u64::from(sequence), TraceEventKind::RunStarted);
        }
        let end_started = Instant::now();
        run.finish(TraceOutcome::Finished);
        span_end_us.push(end_started.elapsed().as_micros());
        if index % 32 == 31 {
            peak_rss_kib = peak_rss_kib.max(rss_kib());
        }
    }
    let offer_elapsed = started.elapsed();
    release_tx.send(()).expect("release Collector response");
    collector.join().expect("Collector thread");
    let shutdown_started = Instant::now();
    telemetry.shutdown();
    let shutdown_elapsed = shutdown_started.elapsed();
    span_end_us.sort_unstable();
    let p50 = span_end_us[OFFERED_SPANS / 2];
    let p95 = span_end_us[OFFERED_SPANS * 95 / 100];
    let max = span_end_us[OFFERED_SPANS - 1];
    let rss_delta_kib = peak_rss_kib.saturating_sub(idle_rss_kib);
    println!(
        "OTLP saturation: offered {OFFERED_SPANS}, queue {MAX_QUEUED_SPANS}, batch {MAX_EXPORT_BATCH_SPANS}; span-end p50/p95/max {p50}/{p95}/{max} us; idle/peak/delta RSS {idle_rss_kib}/{peak_rss_kib}/{rss_delta_kib} KiB; offer {offer_elapsed:?}, shutdown {shutdown_elapsed:?}"
    );
    assert!(
        offer_elapsed < Duration::from_millis(450),
        "export worker stayed gated"
    );
    assert!(p95 <= 50, "span end exceeds 50 us under saturation");
    assert!(
        rss_delta_kib <= 4 * 1024,
        "saturated queue exceeds 4 MiB RSS delta"
    );
    assert!(shutdown_elapsed <= SHUTDOWN_TIMEOUT, "bounded shutdown");
}
