use super::report;
use crate::loopback::{ChildGuard, check_profile, send_response, wait_product, write_profile};
use arany::{
    CollaborationPolicy, Event, RunId, RunStatus, SessionId, SessionView, StateRoot, Store,
};
use std::{
    collections::HashSet,
    io::Read,
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WARMUPS: usize = 10;
const SAMPLES: usize = 100;
const FILE_BYTES: usize = 32 * 1024;
const REQUEST_LIMIT: usize = 128 * 1024;

fn read_large_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).expect("request bytes");
        assert!(count > 0 && request.len() + count <= REQUEST_LIMIT);
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header_end = end + 4;
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            assert!(header.starts_with("POST /v1/responses HTTP/1.1\r\n"));
            assert!(
                header
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-key")
            );
            let body_len = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .expect("Content-Length");
            let total = header_end.checked_add(body_len).expect("request length");
            assert!(total <= REQUEST_LIMIT, "bounded request");
            if request.len() >= total {
                return request[header_end..total].to_vec();
            }
        }
    }
}

#[test]
#[ignore = "run only with cargo test --release --test session_run startup_upper_bound_two_files -- --ignored --nocapture"]
fn startup_upper_bound_two_files_on_named_host() {
    let test_binary = std::env::current_exe().expect("test binary path");
    assert!(
        test_binary
            .components()
            .any(|part| part.as_os_str() == "release"),
        "release profile required for latency measurement"
    );
    let temp = tempfile::tempdir().expect("private benchmark root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("benchmark Workspace");
    std::fs::write(workspace.join("first.txt"), vec![b'A'; FILE_BYTES])
        .expect("first included file");
    std::fs::write(workspace.join("second.txt"), vec![b'B'; FILE_BYTES])
        .expect("second included file");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback Provider");
    listener
        .set_nonblocking(true)
        .expect("bounded Provider accept");
    write_profile(
        &state,
        listener.local_addr().expect("Provider address").port(),
    );
    let (accepted_sender, accepted_receiver) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        for index in 0..3 + WARMUPS + SAMPLES {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "synthetic request missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            if index >= 3 {
                accepted_sender
                    .send((Instant::now(), SystemTime::now()))
                    .expect("startup observation receiver");
            }
            let body = read_large_request(&mut stream);
            let wire: serde_json::Value =
                serde_json::from_slice(&body).expect("synthetic Responses request");
            assert_eq!(wire["model"], "model-1");
            let input: serde_json::Value =
                serde_json::from_str(wire["input"].as_str().expect("semantic input"))
                    .expect("semantic request");
            let text = if index < 3 {
                match index {
                    0 => {
                        serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}})
                    }
                    1 => {
                        serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                    }
                    _ => serde_json::json!({"summary":"synthetic summary"}),
                }
            } else {
                assert_eq!(input["phase"], "root_plan");
                assert_eq!(input["objective"], "Synthetic startup question");
                let includes = input["includes"].as_array().expect("included input");
                assert_eq!(includes.len(), 2);
                for (include, byte) in includes.iter().zip(*b"AB") {
                    let content = include.as_str().expect("included text");
                    assert_eq!(content.len(), FILE_BYTES);
                    assert!(content.bytes().all(|actual| actual == byte));
                }
                serde_json::json!({"outcome":{"type":"finish","summary":"done","result":"startup answer"}})
            };
            send_response(&mut stream, index, text);
        }
    });
    check_profile(&workspace, &state);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("benchmark runtime");
    let observer = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
        .expect("read-only observer");
    let mut session_ids = HashSet::with_capacity(WARMUPS + SAMPLES);
    let mut samples = Vec::with_capacity(SAMPLES);
    let mut started_event_samples = Vec::with_capacity(SAMPLES);
    let mut spawned_event_samples = Vec::with_capacity(SAMPLES);
    for index in 0..WARMUPS + SAMPLES {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
            .current_dir(&workspace)
            .args(["exec", "--state-dir"])
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
                "--include",
                "first.txt",
                "--include",
                "second.txt",
                "Synthetic startup question",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let wall_started = SystemTime::now();
        let started = Instant::now();
        let mut child = ChildGuard::new(command.spawn().expect("startup process"));
        let (accepted, wall_accepted) = accepted_receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("Provider connection after RunStarted");
        let upper_bound = accepted.duration_since(started);
        let result = wait_product(child.take());
        if index >= WARMUPS {
            samples.push(upper_bound);
        }
        assert!(result.status.success(), "startup process succeeded");
        assert_eq!(result.stdout, b"Answer:\n  startup answer\n");
        let receipt = std::str::from_utf8(&result.stderr).expect("startup receipt");
        let lines = receipt.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        let session_id = lines[0]
            .strip_prefix("Session: ")
            .expect("Session receipt")
            .parse::<SessionId>()
            .expect("Session ID");
        assert!(session_ids.insert(session_id), "fresh Session per Run");
        let run_id = lines[1]
            .strip_prefix("Run: ")
            .expect("Run receipt")
            .parse::<RunId>()
            .expect("Run ID");
        assert_eq!(lines[2], "Status: finished");
        assert_eq!(lines[3], "Provider: custom verified");
        let events = runtime
            .block_on(observer.load_session(session_id))
            .expect("committed Events");
        let started_at_ms = events
            .iter()
            .find(|event| matches!(&event.event, Event::RunStarted { .. }))
            .expect("committed RunStarted")
            .created_at_ms;
        let spawned_at_ms = events
            .iter()
            .find(|event| matches!(&event.event, Event::AgentSpawned { .. }))
            .expect("committed primary AgentSpawned")
            .created_at_ms;
        let started_at = UNIX_EPOCH
            + Duration::from_millis(u64::try_from(started_at_ms).expect("valid Event timestamp"));
        let spawned_at = UNIX_EPOCH
            + Duration::from_millis(u64::try_from(spawned_at_ms).expect("valid Event timestamp"));
        if index >= WARMUPS
            && let (Ok(started_interval), Ok(spawned_interval), Ok(wall_span)) = (
                started_at.duration_since(wall_started),
                spawned_at.duration_since(wall_started),
                wall_accepted.duration_since(wall_started),
            )
            && started_interval <= spawned_interval
            && spawned_interval <= wall_span
            && wall_span <= upper_bound + Duration::from_millis(5)
            && upper_bound <= wall_span + Duration::from_millis(5)
        {
            started_event_samples.push(started_interval);
            spawned_event_samples.push(spawned_interval);
        }
        let view = SessionView::replay(session_id, &events)
            .expect("strict replay")
            .expect("startup Session");
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].id, run_id);
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        assert_eq!(view.runs[0].agents.len(), 1);
        assert_eq!(
            view.runs[0].assistant_message.as_deref(),
            Some("startup answer")
        );
        let config = view.runs[0].config.as_ref().expect("pinned Run");
        assert_eq!(config.policy, CollaborationPolicy::Single);
        assert_eq!(config.include_digests.len(), 2);
    }
    server.join().expect("synthetic Provider completed");
    runtime.block_on(observer.close()).expect("close observer");
    if started_event_samples.len() == SAMPLES {
        report(
            "startup_to_run_started_event_creation_diagnostic",
            &mut started_event_samples,
        );
        report(
            "startup_to_agent_spawned_event_creation_diagnostic",
            &mut spawned_event_samples,
        );
    } else {
        println!(
            "startup event-clock diagnostic unavailable: {}/{} comparable samples",
            started_event_samples.len(),
            SAMPLES
        );
    }
    let p95 = report("startup_to_provider_accept_upper_bound", &mut samples);
    assert!(
        p95 <= Duration::from_millis(50),
        "startup upper-bound p95 exceeds named-host 50 ms release gate"
    );
}
