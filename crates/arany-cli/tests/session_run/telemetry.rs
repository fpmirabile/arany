use super::loopback::{check_profile, read_request, send_response, wait_product, write_profile};
use arany::{GoalStatus, RunStatus, SessionId, StateRoot, Store};
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

fn committed_markers(span: &Span) -> Vec<(u64, String)> {
    span.events
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
                    (*sequence as u64, kind.clone())
                }
                _ => panic!("typed committed marker missing"),
            }
        })
        .collect()
}

fn string_attribute<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            Value::StringValue(value) => Some(value.as_str()),
            _ => None,
        })
}

fn int_attribute(span: &Span, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            Value::IntValue(value) => Some(*value),
            _ => None,
        })
}

fn bool_attribute(span: &Span, key: &str) -> Option<bool> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| value.value.as_ref())
        .and_then(|value| match value {
            Value::BoolValue(value) => Some(*value),
            _ => None,
        })
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
        for index in 0..14 {
            let mut stream = crate::process::accept_with_deadline(
                &provider_listener,
                Duration::from_secs(10),
                "loopback request",
            );
            let body = read_request(&mut stream);
            assert!(
                !body
                    .windows(b"OMITTED_SECRET_CANARY".len())
                    .any(|part| part == b"OMITTED_SECRET_CANARY")
            );
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"synthetic child","profile":null}]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                4 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"child one","profile":null},{"objective":"child two","profile":null}]}})
                }
                _ => {
                    serde_json::json!({"outcome":{"type":"finish","summary":"SUMMARY_SECRET_CANARY","result":"RESULT_SECRET_CANARY"}})
                }
            };
            if index >= 9 {
                assert!(
                    !body
                        .windows(b"SupervisorVerdictRecorded".len())
                        .any(|part| part == b"SupervisorVerdictRecorded"),
                    "shadow verdict entered model input"
                );
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["tool_choice"], "required");
                let (name, arguments) = if index < 13 {
                    (
                        "arany_read",
                        serde_json::json!({"path":"supervisor-path-canary.txt","offset":0,"limit":64,"plan_contract":null}),
                    )
                } else {
                    (
                        "arany_finish",
                        serde_json::json!({"summary":"SUMMARY_SECRET_CANARY","result":"RESULT_SECRET_CANARY"}),
                    )
                };
                let response = serde_json::json!({"id":format!("resp_shadow_{index}"),"status":"completed","model":"model-1","output":[
                    {"type":"function_call","id":format!("fc_shadow_{index}"),"call_id":format!("call_shadow_{index}"),"name":name,"arguments":arguments.to_string(),"status":"completed"}
                ],"usage":{"input_tokens":20,"output_tokens":10}});
                let (content_type, response) = if request["stream"] == true {
                    (
                        "text/event-stream",
                        format!(
                            "data: {}\n\n",
                            serde_json::json!({"type":"response.completed","response":response})
                        ),
                    )
                } else {
                    ("application/json", response.to_string())
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
            } else {
                send_response(&mut stream, index, text);
            }
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
    let goal = spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.goal")
        .expect("Goal span");
    let root = spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.run")
        .expect("Run span");
    assert_eq!(root.parent_span_id, goal.span_id);
    assert_eq!(
        string_attribute(goal, "arany.goal.status"),
        Some("completed")
    );
    for key in [
        "arany.goal.provider_calls",
        "arany.goal.known_tokens",
        "arany.goal.token_usage_unknown",
        "arany.goal.active_ms",
        "arany.goal.uncertain_active_ms",
    ] {
        assert!(
            goal.attributes.iter().any(|attribute| attribute.key == key),
            "Goal usage attribute {key}"
        );
    }
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
    assert!(
        goal.start_time_unix_nano <= root.start_time_unix_nano,
        "Goal starts no later than its Run child"
    );
    let first_marker = goal.events.first().expect("first Goal marker");
    let context_time = |key: &str| {
        first_marker
            .attributes
            .iter()
            .find(|attribute| attribute.key == key)
            .and_then(|attribute| attribute.value.as_ref())
            .and_then(|value| match value.value {
                Some(Value::IntValue(nanos)) => Some(nanos),
                _ => None,
            })
    };
    assert!(
        matches!(
            (
                context_time("arany.context.started_unix_nano"),
                context_time("arany.context.ended_unix_nano"),
            ),
            (Some(started), Some(ended)) if 0 < started && started <= ended
        ),
        "context compilation rides on the first committed marker"
    );
    assert!(
        root.events.iter().all(|event| event
            .attributes
            .iter()
            .all(|attribute| !attribute.key.starts_with("arany.context."))),
        "only the first marker carries context compilation"
    );

    let lines = String::from_utf8(result.stdout)
        .expect("JSONL output")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("Event JSONL"))
        .collect::<Vec<_>>();
    let markers = committed_markers(root);
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
    let goal_event = |kind: &str| kind.starts_with("Goal");
    let expected_goal = expected
        .iter()
        .filter(|(_, kind)| goal_event(kind))
        .map(|(sequence, kind)| (*sequence, (*kind).to_owned()))
        .collect::<Vec<_>>();
    let expected_run = expected
        .iter()
        .filter(|(_, kind)| !goal_event(kind))
        .map(|(sequence, kind)| (*sequence, (*kind).to_owned()))
        .collect::<Vec<_>>();
    assert_eq!(markers, expected_run, "Run trace markers follow Run Events");
    assert_eq!(
        committed_markers(goal),
        expected_goal,
        "Goal trace markers follow Goal Events"
    );
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
    assert_eq!(view.goals.len(), 1);
    let replayed_goal = &view.goals[0];
    assert_eq!(replayed_goal.status, GoalStatus::Completed);
    let replayed_goal_id = replayed_goal.id.to_string();
    assert_eq!(
        string_attribute(goal, "arany.goal.id"),
        Some(replayed_goal_id.as_str())
    );
    assert_eq!(
        int_attribute(goal, "arany.goal.provider_calls"),
        Some(i64::from(replayed_goal.provider_calls))
    );
    assert_eq!(
        int_attribute(goal, "arany.goal.known_tokens"),
        Some(i64::try_from(replayed_goal.known_tokens).expect("bounded known token total"))
    );
    assert_eq!(
        bool_attribute(goal, "arany.goal.token_usage_unknown"),
        Some(replayed_goal.token_usage_unknown)
    );
    assert_eq!(
        int_attribute(goal, "arany.goal.active_ms"),
        Some(i64::try_from(replayed_goal.active_ms).expect("bounded active time"))
    );
    assert_eq!(
        int_attribute(goal, "arany.goal.uncertain_active_ms"),
        Some(i64::try_from(replayed_goal.uncertain_active_ms).expect("bounded uncertain time"))
    );
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
    let team_goal = team_spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.goal")
        .expect("team Goal span");
    let team_run = team_spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.run")
        .expect("team Run span");
    assert_eq!(team_run.parent_span_id, team_goal.span_id);
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

    std::fs::write(
        workspace.join("supervisor-path-canary.txt"),
        b"SUPERVISOR_FILE_CONTENT_CANARY",
    )
    .unwrap();
    crate::private_file(
        &state.join("tools.json"),
        &serde_json::to_vec(&serde_json::json!({
            "version": 1, "workspace_paths": ["supervisor-path-canary.txt"], "write": false,
            "commands": [], "skills": [], "mcp": []
        }))
        .unwrap(),
    );
    let shadow_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    shadow_listener.set_nonblocking(true).unwrap();
    let shadow_address = shadow_listener.local_addr().unwrap();
    let (shadow_done, shadow_wait) = mpsc::channel();
    let shadow_collector =
        thread::spawn(move || collect_trace_requests(shadow_listener, shadow_wait, None));
    let mut shadow = Command::new(env!("CARGO_BIN_EXE_arany"));
    shadow
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args([
            "--otlp-endpoint",
            &format!("http://{shadow_address}"),
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
            "--tools",
            "--output",
            "jsonl",
            "SUPERVISOR_OBJECTIVE_CANARY",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let shadow_result = wait_product(shadow.spawn().unwrap());
    shadow_done.send(()).unwrap();
    let shadow_bodies = shadow_collector.join().unwrap();
    provider.join().expect("Provider server");
    assert!(shadow_result.status.success(), "shadow exec failed");
    assert!(shadow_result.stderr.is_empty());
    for canary in [
        "supervisor-path-canary.txt",
        "SUPERVISOR_FILE_CONTENT_CANARY",
        "SUPERVISOR_OBJECTIVE_CANARY",
        "SUMMARY_SECRET_CANARY",
        "RESULT_SECRET_CANARY",
    ] {
        assert!(
            shadow_bodies.iter().all(|body| !body
                .windows(canary.len())
                .any(|part| part == canary.as_bytes())),
            "Supervisor trace canary leaked"
        );
    }
    let shadow_lines = String::from_utf8(shadow_result.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let shadow_session =
        SessionId::from_str(shadow_lines[0]["session_id"].as_str().unwrap()).unwrap();
    let shadow_view = runtime
        .block_on(store.load_view(shadow_session))
        .unwrap()
        .unwrap();
    let persisted = runtime
        .block_on(store.load_session(shadow_session))
        .unwrap();
    assert!(
        persisted
            .iter()
            .map(|event| serde_json::to_value(arany_cli::protocol::EventLine::from(event)).unwrap())
            .collect::<Vec<_>>()
            == shadow_lines,
        "JSONL differs from committed history"
    );
    assert_eq!(shadow_view.runs[0].status, RunStatus::Finished);
    let verdicts: Vec<_> = shadow_lines
        .iter()
        .filter(|line| line["kind"] == "SupervisorVerdictRecorded")
        .collect();
    assert_eq!(verdicts.len(), 2);
    assert_eq!(
        shadow_view.runs[0].supervisor.verdicts().len(),
        verdicts.len()
    );
    let shadow_spans = trace_spans(&shadow_bodies);
    let shadow_run = shadow_spans
        .iter()
        .find(|span| span.name == "invoke_workflow arany.run")
        .unwrap();
    let exported: Vec<_> = shadow_run
        .events
        .iter()
        .filter(|event| event.name == "arany.supervisor.verdict")
        .collect();
    assert_eq!(exported.len(), verdicts.len());
    for (event, verdict) in exported.iter().zip(verdicts) {
        assert_eq!(
            event.time_unix_nano,
            verdict["created_at_ms"].as_u64().unwrap() * 1_000_000
        );
        assert_eq!(event.attributes.len(), 2);
        for (key, expected) in [
            ("arany.supervisor.signals", vec!["loop"]),
            (
                "arany.supervisor.levels",
                vec![verdict["payload"]["signals"][0]["level"].as_str().unwrap()],
            ),
        ] {
            let value = event
                .attributes
                .iter()
                .find(|attr| attr.key == key)
                .unwrap()
                .value
                .as_ref()
                .unwrap()
                .value
                .as_ref()
                .unwrap();
            let Value::ArrayValue(array) = value else {
                panic!("closed enum array");
            };
            assert_eq!(
                array
                    .values
                    .iter()
                    .map(|value| match value.value.as_ref().unwrap() {
                        Value::StringValue(value) => value.as_str(),
                        _ => panic!("closed string enum"),
                    })
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

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
