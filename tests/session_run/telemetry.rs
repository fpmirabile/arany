use super::loopback::{check_profile, read_request, send_response, wait_product, write_profile};
use arany::{RunStatus, SessionId, StateRoot, Store};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::trace::v1::Span;
use prost::Message;
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    str::FromStr,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

fn accept_with_deadline(listener: &TcpListener) -> std::net::TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "loopback request missing");
                thread::yield_now();
            }
            Err(error) => panic!("loopback accept failed: {error}"),
        }
    }
}

fn trace_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("trace request deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("trace request bytes");
        assert!(count > 0 && request.len() + count <= 257 * 1024);
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = end + 4;
            let header = std::str::from_utf8(&request[..header_end]).expect("trace header");
            assert!(header.starts_with("POST /v1/traces HTTP/1.1\r\n"));
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("content-type: application/x-protobuf")
            );
            assert!(!header.to_ascii_lowercase().contains("authorization:"));
            let body_len = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("trace Content-Length");
            if request.len() >= header_end + body_len {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-protobuf\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("Collector acknowledgment");
                return request[header_end..header_end + body_len].to_vec();
            }
        }
    }
}

fn collect_trace_requests(
    listener: TcpListener,
    done: Receiver<()>,
    mut first_export: Option<mpsc::Sender<()>>,
) -> Vec<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bodies = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                assert!(bodies.len() < 16, "too many trace batches");
                bodies.push(trace_request(&mut stream));
                if let Some(sender) = first_export.take() {
                    sender.send(()).expect("first trace batch gate");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if matches!(done.try_recv(), Ok(()) | Err(TryRecvError::Disconnected)) {
                    break;
                }
                assert!(Instant::now() < deadline, "trace export deadline");
                thread::yield_now();
            }
            Err(error) => panic!("Collector accept failed: {error}"),
        }
    }
    assert!(!bodies.is_empty(), "trace export missing");
    bodies
}

fn trace_spans(bodies: &[Vec<u8>]) -> Vec<Span> {
    let mut spans = Vec::new();
    for body in bodies {
        let request = ExportTraceServiceRequest::decode(body.as_slice()).expect("OTLP protobuf");
        assert_eq!(request.resource_spans.len(), 1);
        let resource = request.resource_spans.into_iter().next().expect("resource");
        assert_eq!(resource.scope_spans.len(), 1);
        spans.extend(
            resource
                .scope_spans
                .into_iter()
                .next()
                .expect("scope")
                .spans,
        );
    }
    spans
}

#[test]
fn product_exec_exports_only_safe_committed_trace_facts() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("AGENTS.md"), b"GUIDANCE_SECRET_CANARY")
        .expect("guidance canary");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_SECRET_CANARY")
        .expect("omitted canary");
    let state = temp.path().join("state");
    let provider_listener = TcpListener::bind("127.0.0.1:0").expect("Provider listener");
    provider_listener
        .set_nonblocking(true)
        .expect("bounded Provider accept");
    write_profile(
        &state,
        provider_listener
            .local_addr()
            .expect("Provider address")
            .port(),
    );
    let (first_team_export, team_export_gate) = mpsc::channel();
    let provider = thread::spawn(move || {
        for index in 0..9 {
            let mut stream = accept_with_deadline(&provider_listener);
            let body = read_request(&mut stream);
            assert!(
                !body
                    .windows(b"OMITTED_SECRET_CANARY".len())
                    .any(|part| part == b"OMITTED_SECRET_CANARY")
            );
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                4 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["child one","child two"]}})
                }
                _ => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"SUMMARY_SECRET_CANARY","result":"RESULT_SECRET_CANARY"}})
                }
            };
            send_response(&mut stream, index, text);
            if index == 4 {
                team_export_gate
                    .recv_timeout(Duration::from_secs(10))
                    .expect("first team trace batch before remaining Provider replies");
            }
        }
    });
    check_profile(&workspace, &state);

    let collector_listener = TcpListener::bind("127.0.0.1:0").expect("Collector listener");
    collector_listener
        .set_nonblocking(true)
        .expect("bounded Collector accept");
    let collector_address = collector_listener.local_addr().expect("Collector address");
    let (collector_done, collector_wait) = mpsc::channel();
    let collector =
        thread::spawn(move || collect_trace_requests(collector_listener, collector_wait, None));

    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("OTEL_RESOURCE_ATTRIBUTES", "leak=RESOURCE_SECRET_CANARY")
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .current_dir(&workspace)
        .args([
            "--otlp-endpoint",
            &format!("http://{collector_address}"),
            "exec",
        ])
        .arg("--state-dir")
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--collaboration",
            "single",
            "--output",
            "jsonl",
            "OBJECTIVE_SECRET_CANARY",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let result = wait_product(command.spawn().expect("traced product process"));
    collector_done.send(()).expect("Collector completion");
    let bodies = collector.join().expect("Collector server");
    assert!(result.status.success(), "traced exec exit");
    assert_eq!(result.stderr, b"");
    for canary in [
        "OBJECTIVE_SECRET_CANARY",
        "GUIDANCE_SECRET_CANARY",
        "OMITTED_SECRET_CANARY",
        "SUMMARY_SECRET_CANARY",
        "RESULT_SECRET_CANARY",
        "RESOURCE_SECRET_CANARY",
        "test-key",
    ] {
        assert!(
            bodies.iter().all(|body| !body
                .windows(canary.len())
                .any(|part| part == canary.as_bytes())),
            "trace canary leaked"
        );
    }
    let spans = trace_spans(&bodies);
    assert_eq!(spans.len(), 4);
    let root = spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.run")
        .expect("Run span");
    let primary = spans
        .iter()
        .find(|span| span.name == "invoke_agent arany.primary")
        .expect("primary span");
    assert_eq!(primary.parent_span_id, root.span_id);
    let provider_span = spans
        .iter()
        .find(|span| span.name == "chat arany.provider")
        .expect("Provider span");
    assert_eq!(provider_span.parent_span_id, primary.span_id);
    let compilation = spans
        .iter()
        .find(|span| span.name == "compile_context arany.run")
        .expect("context compilation span");
    assert_eq!(compilation.parent_span_id, root.span_id);

    let lines = String::from_utf8(result.stdout)
        .expect("JSONL output")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("Event JSONL"))
        .collect::<Vec<_>>();
    let markers = root
        .events
        .iter()
        .map(|event| {
            assert_eq!(event.name, "arany.event.committed");
            let sequence = event
                .attributes
                .iter()
                .find(|attribute| attribute.key == "arany.event.sequence")
                .and_then(|attribute| attribute.value.as_ref())
                .and_then(|value| value.value.as_ref());
            let kind = event
                .attributes
                .iter()
                .find(|attribute| attribute.key == "arany.event.kind")
                .and_then(|attribute| attribute.value.as_ref())
                .and_then(|value| value.value.as_ref());
            match (sequence, kind) {
                (Some(Value::IntValue(sequence)), Some(Value::StringValue(kind))) => {
                    (*sequence as u64, kind.as_str())
                }
                _ => panic!("typed committed marker missing"),
            }
        })
        .collect::<Vec<_>>();
    let expected = lines
        .iter()
        .skip(1)
        .map(|line| {
            (
                line["sequence"].as_u64().expect("Event sequence"),
                line["kind"].as_str().expect("Event kind"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(markers, expected, "trace markers follow committed Events");
    let session_id = SessionId::from_str(lines[0]["session_id"].as_str().expect("Session ID"))
        .expect("typed Session ID");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state root"))
        .expect("read-only Store");
    let view = runtime
        .block_on(store.load_view(session_id))
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    let run_id = view.runs[0].id.to_string();
    assert!(bodies.iter().any(|body| {
        body.windows(run_id.len())
            .any(|part| part == run_id.as_bytes())
    }));

    let team_listener = TcpListener::bind("127.0.0.1:0").expect("team Collector listener");
    team_listener
        .set_nonblocking(true)
        .expect("team Collector nonblocking");
    let team_address = team_listener.local_addr().expect("team Collector address");
    let (team_done, team_wait) = mpsc::channel();
    let team_collector = thread::spawn(move || {
        collect_trace_requests(team_listener, team_wait, Some(first_team_export))
    });
    let mut team = Command::new(env!("CARGO_BIN_EXE_arany"));
    team.env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["--otlp-endpoint", &format!("http://{team_address}"), "exec"])
        .arg("--state-dir")
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--collaboration",
            "team",
            "--max-active-children",
            "2",
            "--output",
            "jsonl",
            "TEAM_SECRET_CANARY",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let team_result = wait_product(team.spawn().expect("traced team process"));
    team_done.send(()).expect("team Collector completion");
    let team_bodies = team_collector.join().expect("team Collector server");
    assert!(team_bodies.len() >= 2, "team trace crossed two batches");
    assert!(team_result.status.success(), "traced team exit");
    assert_eq!(team_result.stderr, b"");
    assert!(team_bodies.iter().all(|body| {
        !body
            .windows(b"TEAM_SECRET_CANARY".len())
            .any(|part| part == b"TEAM_SECRET_CANARY")
    }));
    let team_spans = trace_spans(&team_bodies);
    assert_eq!(
        team_spans.len(),
        10,
        "two admitted children produce dynamic topology"
    );
    let team_primary = team_spans
        .iter()
        .find(|span| span.name == "invoke_agent arany.primary")
        .expect("team primary span");
    assert_eq!(
        team_spans
            .iter()
            .filter(|span| span.name == "invoke_agent arany.child")
            .count(),
        2
    );
    assert!(
        team_spans
            .iter()
            .filter(|span| span.name == "invoke_agent arany.child")
            .all(|span| span.parent_span_id == team_primary.span_id)
    );
    assert_eq!(
        team_spans
            .iter()
            .filter(|span| span.name == "chat arany.provider")
            .count(),
        4
    );
    let team_lines = String::from_utf8(team_result.stdout)
        .expect("team JSONL")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("team Event"))
        .collect::<Vec<_>>();
    let team_session = SessionId::from_str(
        team_lines[0]["session_id"]
            .as_str()
            .expect("team Session ID"),
    )
    .expect("typed team Session ID");
    let team_view = runtime
        .block_on(store.load_view(team_session))
        .expect("team strict replay")
        .expect("team Session");
    assert_eq!(team_view.runs.len(), 1);
    assert_eq!(team_view.runs[0].status, RunStatus::Finished);
    assert_eq!(team_view.runs[0].agents.len(), 3);

    let refused_listener = TcpListener::bind("127.0.0.1:0").expect("unavailable Collector port");
    let refused_address = refused_listener.local_addr().expect("unavailable address");
    drop(refused_listener);
    let mut unavailable = Command::new(env!("CARGO_BIN_EXE_arany"));
    unavailable
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args([
            "--otlp-endpoint",
            &format!("http://{refused_address}"),
            "exec",
        ])
        .arg("--state-dir")
        .arg(&state)
        .arg("--workspace")
        .arg(&workspace)
        .args([
            "--provider",
            "custom:local",
            "--model",
            "model-1",
            "--collaboration",
            "single",
            "--output",
            "jsonl",
            "OBJECTIVE_SECRET_CANARY",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let unavailable_result = wait_product(unavailable.spawn().expect("unavailable Collector run"));
    provider.join().expect("Provider server");
    assert!(
        unavailable_result.status.success(),
        "Collector outage is nonfatal"
    );
    assert_eq!(unavailable_result.stderr, b"");
    let unavailable_lines = String::from_utf8(unavailable_result.stdout)
        .expect("unavailable JSONL")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("unavailable Event"))
        .collect::<Vec<_>>();
    let unavailable_session = SessionId::from_str(
        unavailable_lines[0]["session_id"]
            .as_str()
            .expect("unavailable Session ID"),
    )
    .expect("typed unavailable Session ID");
    let unavailable_view = runtime
        .block_on(store.load_view(unavailable_session))
        .expect("unavailable Collector replay")
        .expect("unavailable Session");
    assert_eq!(unavailable_view.runs.len(), 1);
    assert_eq!(unavailable_view.runs[0].status, RunStatus::Finished);

    let show_listener = TcpListener::bind("127.0.0.1:0").expect("show Collector listener");
    show_listener
        .set_nonblocking(true)
        .expect("show Collector nonblocking");
    let show_address = show_listener.local_addr().expect("show Collector address");
    let mut show = Command::new(env!("CARGO_BIN_EXE_arany"));
    show.env_clear()
        .current_dir(&workspace)
        .args(["--otlp-endpoint", &format!("http://{show_address}"), "show"])
        .arg("--state-dir")
        .arg(&state)
        .args(["--output", "jsonl", &session_id.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let shown = wait_product(show.spawn().expect("traced show process"));
    assert!(
        shown.status.success(),
        "show succeeds with telemetry opt-in"
    );
    assert_eq!(shown.stderr, b"");
    assert!(matches!(
        show_listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    runtime.block_on(store.close()).expect("close Store");
}

#[test]
fn invalid_telemetry_configuration_exits_before_workspace_or_state_use() {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    for (endpoint, header, expected) in [
        (
            "https://collector.example/v1/traces",
            None,
            "error: invalid local OTLP trace endpoint\n",
        ),
        (
            "http://127.0.0.1:4318",
            Some("HEADER_SECRET_CANARY"),
            "error: unsupported OTLP setting OTEL_EXPORTER_OTLP_HEADERS; configure the local Collector instead\n",
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(&workspace)
            .args(["--otlp-endpoint", endpoint, "exec"])
            .arg("--state-dir")
            .arg(&state)
            .arg("--workspace")
            .arg(&workspace)
            .args(["--provider", "openai", "--model", "gpt-5.4", "objective"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(header) = header {
            command.env("OTEL_EXPORTER_OTLP_HEADERS", header);
        }
        let result = wait_product(command.spawn().expect("invalid telemetry process"));
        assert_eq!(result.status.code(), Some(2));
        assert_eq!(result.stdout, b"");
        assert_eq!(result.stderr, expected.as_bytes());
        assert!(
            !state.exists(),
            "configuration rejected before state creation"
        );
    }
}
