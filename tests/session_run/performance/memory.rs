use crate::loopback::{ChildGuard, check_profile, send_response, wait_product, write_profile};
use arany::{RunStatus, SessionId, SessionView, StateRoot, Store};
use rustix::process::{Pid, Signal, kill_process};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::Stdio,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const FILE_BYTES: usize = 128 * 1024;
const RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_LIMIT: usize = 512 * 1024;
const OUTPUT_LIMIT: usize = 32 * 1024;

fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("request deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 8192];
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

fn rss_kib(pid: Pid) -> usize {
    let rollup = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", pid.as_raw_pid()))
        .expect("product memory snapshot");
    rollup
        .lines()
        .find_map(|line| {
            line.strip_prefix("Rss:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<usize>().ok())
        })
        .expect("product resident memory")
}

struct ProductGuard(Option<Pid>);

impl Drop for ProductGuard {
    fn drop(&mut self) {
        let Some(pid) = self.0 else {
            return;
        };
        if std::fs::read_link(format!("/proc/{}/exe", pid.as_raw_pid()))
            .ok()
            .as_deref()
            == Some(Path::new(env!("CARGO_BIN_EXE_arany")))
        {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}

#[derive(Debug)]
enum Stage {
    Pid(Pid),
    Idle,
    Finished,
}

fn receive_stage(receiver: &mpsc::Receiver<Stage>) -> Stage {
    receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("bounded attached output stage")
}

#[test]
#[ignore = "run only with cargo test --release --test session_run resident_memory_two_includes -- --ignored --nocapture"]
fn resident_memory_two_includes_on_named_host() {
    let test_binary = std::env::current_exe().expect("test binary path");
    assert!(
        test_binary
            .components()
            .any(|part| part.as_os_str() == "release"),
        "release profile required for memory measurement"
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
    let (request_ready, request_received) = mpsc::sync_channel(1);
    let (release_request, released) = mpsc::sync_channel(1);
    let (half_response_ready, half_response_received) = mpsc::sync_channel(1);
    let (finish_response, finish_response_received) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        for index in 0..4 {
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
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write deadline");
            let body = read_request(&mut stream);
            let wire: serde_json::Value =
                serde_json::from_slice(&body).expect("synthetic Responses request");
            assert_eq!(wire["model"], "model-1");
            let input: serde_json::Value =
                serde_json::from_str(wire["input"].as_str().expect("semantic input"))
                    .expect("semantic request");
            let text = match index {
                0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}})
                }
                2 => serde_json::json!({"summary":"synthetic summary"}),
                _ => {
                    assert_eq!(input["phase"], "root_plan");
                    assert_eq!(input["objective"], "Synthetic memory question");
                    let includes = input["includes"].as_array().expect("included input");
                    assert_eq!(includes.len(), 2);
                    for (include, byte) in includes.iter().zip(*b"AB") {
                        let content = include.as_str().expect("included text");
                        assert_eq!(content.len(), FILE_BYTES);
                        assert!(content.bytes().all(|actual| actual == byte));
                    }
                    request_ready.send(()).expect("memory sample gate");
                    released
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release synthetic response");
                    let text = serde_json::json!({"outcome":{"type":"finish","summary":"done","result":"memory answer"}});
                    let mut response = serde_json::json!({
                        "id": format!("resp_{}", index + 1),
                        "status": "completed",
                        "model": "model-1",
                        "output": [{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text.to_string()}]}],
                        "usage": {"input_tokens":20,"output_tokens":10},
                        "padding": ""
                    });
                    let padding = RESPONSE_BYTES
                        .checked_sub(response.to_string().len())
                        .expect("response under cap before padding");
                    response["padding"] = serde_json::json!("x".repeat(padding));
                    let response = response.to_string();
                    assert_eq!(response.len(), RESPONSE_BYTES);
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.len()
                    )
                    .expect("bounded response headers");
                    let midpoint = RESPONSE_BYTES / 2;
                    stream
                        .write_all(&response.as_bytes()[..midpoint])
                        .expect("first response half");
                    half_response_ready
                        .send(())
                        .expect("mid-response sample gate");
                    finish_response_received
                        .recv_timeout(Duration::from_secs(10))
                        .expect("complete synthetic response");
                    stream
                        .write_all(&response.as_bytes()[midpoint..])
                        .expect("second response half");
                    continue;
                }
            };
            send_response(&mut stream, index, text);
        }
    });
    check_profile(&workspace, &state);

    let mut command =
        crate::process::account_isolated_script(state.parent().expect("fixture root"));
    command
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/usr/bin/zsh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args([
            "-q",
            "-e",
            "-c",
            r#"TIMEFMT='ARANY_MAX_RSS:%M'; time /usr/bin/zsh -c 'printf "ARANY_PID:%s\n" "$$"; exec "$ARANY_TEST_EXE" --screen-reader --state-dir "$ARANY_TEST_STATE" --workspace "$ARANY_TEST_WORKSPACE" --provider custom:local --model model-1 --include first.txt --include second.txt'"#,
            "/dev/null",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("attached PTY process"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let (stage_sender, stage_receiver) = mpsc::channel();
    let mut consent_input =
        std::fs::File::from(rustix::io::dup(&input).expect("owned fixture input"));
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut pid_reported = false;
        let mut idle_reported = false;
        let mut finished_reported = false;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= OUTPUT_LIMIT, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            if !pid_reported
                && let Some(start) = bytes
                    .windows(b"ARANY_PID:".len())
                    .position(|part| part == b"ARANY_PID:")
            {
                let suffix = &bytes[start + b"ARANY_PID:".len()..];
                if let Some(end) = suffix.iter().position(|byte| *byte == b'\n') {
                    let raw = std::str::from_utf8(&suffix[..end])
                        .expect("product PID marker")
                        .trim()
                        .parse::<i32>()
                        .expect("product PID");
                    let pid = Pid::from_raw(raw).expect("positive product PID");
                    stage_sender.send(Stage::Pid(pid)).expect("PID receiver");
                    pid_reported = true;
                }
            }
            if !idle_reported && bytes.windows(b"Input:".len()).any(|part| part == b"Input:") {
                stage_sender.send(Stage::Idle).expect("idle receiver");
                idle_reported = true;
            }
            if idle_reported && !finished_reported {
                let answer = b"Answer:\r\n  memory answer";
                if let Some(start) = bytes.windows(answer.len()).position(|part| part == answer) {
                    let after_answer = &bytes[start + answer.len()..];
                    if after_answer
                        .windows(b"Input:".len())
                        .any(|part| part == b"Input:")
                    {
                        stage_sender
                            .send(Stage::Finished)
                            .expect("finished receiver");
                        finished_reported = true;
                    }
                }
            }
        }
        bytes
    });
    let pid = match receive_stage(&stage_receiver) {
        Stage::Pid(pid) => pid,
        other => panic!("expected product PID, got {other:?}"),
    };
    let mut product = ProductGuard(Some(pid));
    assert!(matches!(receive_stage(&stage_receiver), Stage::Idle));
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", pid.as_raw_pid())).expect("idle product"),
        Path::new(env!("CARGO_BIN_EXE_arany"))
    );
    let idle_kib = rss_kib(pid);
    input
        .write_all(b"Synthetic memory question\n")
        .expect("submit objective");
    request_received
        .recv_timeout(Duration::from_secs(10))
        .expect("large Provider request");
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", pid.as_raw_pid())).expect("active product"),
        Path::new(env!("CARGO_BIN_EXE_arany"))
    );
    let active_rss_kib = rss_kib(pid);
    release_request.send(()).expect("release Provider result");
    half_response_received
        .recv_timeout(Duration::from_secs(10))
        .expect("first half of capped response");
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", pid.as_raw_pid()))
            .expect("mid-response product"),
        Path::new(env!("CARGO_BIN_EXE_arany"))
    );
    let mid_response_rss_kib = rss_kib(pid);
    finish_response.send(()).expect("finish Provider response");
    assert!(matches!(receive_stage(&stage_receiver), Stage::Finished));
    assert_eq!(
        std::fs::read_link(format!("/proc/{}/exe", pid.as_raw_pid())).expect("finished product"),
        Path::new(env!("CARGO_BIN_EXE_arany"))
    );
    let after_rss_kib = rss_kib(pid);
    input.write_all(b"/quit\n").expect("exit attached mode");
    drop(input);
    let result = wait_product(child.take());
    product.0 = None;
    let output = reader.join().expect("PTY output reader");
    server.join().expect("synthetic Provider completed");
    assert!(result.status.success(), "attached process succeeded");
    let transcript = String::from_utf8(output).expect("linear transcript");
    assert!(transcript.contains("Answer:\r\n  memory answer"));
    assert!(!transcript.contains("test-key"));
    let timed_peak_kib = transcript
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("ARANY_MAX_RSS:"))
        .find_map(|value| value.parse::<usize>().ok())
        .expect("timed Arany peak RSS");
    let peak_kib = timed_peak_kib
        .max(active_rss_kib)
        .max(mid_response_rss_kib)
        .max(after_rss_kib)
        .max(idle_kib);
    let session_id = transcript
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("Session: "))
        .find_map(|value| value.parse::<SessionId>().ok())
        .expect("Session receipt");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime");
    let observer = Store::open_read_only(StateRoot::open_existing(&state).expect("state"))
        .expect("read-only Store");
    let events = runtime
        .block_on(observer.load_session(session_id))
        .expect("committed Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("memory Session");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(
        view.runs[0].assistant_message.as_deref(),
        Some("memory answer")
    );
    assert_eq!(
        view.runs[0]
            .config
            .as_ref()
            .expect("pinned Run")
            .include_digests
            .len(),
        2
    );
    runtime.block_on(observer.close()).expect("close observer");
    let delta_kib = peak_kib - idle_kib;
    println!(
        "resident_memory: idle={}KiB active={}KiB mid_response={}KiB after={}KiB timed_peak={}KiB observed_peak={}KiB delta={}KiB response_bytes={} profile=release",
        idle_kib,
        active_rss_kib,
        mid_response_rss_kib,
        after_rss_kib,
        timed_peak_kib,
        peak_kib,
        delta_kib,
        RESPONSE_BYTES
    );
    assert!(
        delta_kib <= 32 * 1024,
        "256 KiB included-input and 1 MiB response RSS exceeds 32 MiB above idle"
    );
}
