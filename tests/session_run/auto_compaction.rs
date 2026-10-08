use super::active_terminal::{ProductGuard, product_child_of};
use super::loopback::wait_product;
use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    process::{Child, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct KillOnDrop(Option<Child>);

impl KillOnDrop {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("supervised child")
    }

    fn take(&mut self) -> Child {
        self.0.take().expect("supervised child")
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("HTTP request deadline");
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 8192];
        let count = stream.read(&mut chunk).expect("HTTP request bytes");
        assert!(count > 0 && request.len() + count <= 512 * 1024);
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
            assert!(body_len <= 512 * 1024);
            if request.len() >= header_end + body_len {
                return request[header_end..header_end + body_len].to_vec();
            }
        }
    }
}

#[derive(Clone, Copy)]
enum CompactionCase {
    Success,
    ProviderFailure,
    Interrupted,
}

fn synthetic_server(
    listener: TcpListener,
    case: CompactionCase,
    compaction_started: mpsc::Sender<()>,
    compaction_release: mpsc::Receiver<()>,
) {
    listener.set_nonblocking(true).expect("bounded accept");
    let request_count = if matches!(case, CompactionCase::ProviderFailure) {
        7
    } else {
        6
    };
    for index in 0..request_count {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "synthetic request {index} missing"
                    );
                    thread::yield_now();
                }
                Err(error) => panic!("accept failed: {error}"),
            }
        };
        let body = read_http_request(&mut stream);
        assert!(
            !body
                .windows(b"OMITTED_CANARY".len())
                .any(|part| part == b"OMITTED_CANARY")
        );
        let wire: serde_json::Value = serde_json::from_slice(&body).expect("Responses request");
        assert_eq!(wire["model"], "model-1");
        assert_eq!(wire["text"]["format"]["strict"], true);
        assert_eq!(
            wire["max_output_tokens"],
            match index {
                0 | 3 | 4 => 4096,
                1 | 2 => 128,
                _ => 1024,
            }
        );
        let input: serde_json::Value =
            serde_json::from_str(wire["input"].as_str().expect("input text"))
                .expect("semantic input");
        match index {
            0..=2 => {
                assert!(
                    input
                        .get("workspace_guidance")
                        .is_none_or(serde_json::Value::is_null)
                );
            }
            3 => {
                assert_eq!(input["objective"].as_str().unwrap().len(), 8192);
                assert_eq!(
                    input["workspace_guidance"].as_str().unwrap().len(),
                    20 * 1024
                );
                assert_eq!(input["includes"].as_array().unwrap().len(), 2);
                use sha2::{Digest, Sha256};
                for (index, path, letter) in [(0, "a.txt", "A"), (1, "b.txt", "B")] {
                    let text = letter.repeat(125 * 1024);
                    let digest: String = Sha256::digest(text.as_bytes())
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    let expected = format!(
                        "File: \"{path}\"\nSHA-256: {digest}\nContent (untrusted data):\n{text}"
                    );
                    assert!(
                        input["includes"][index].as_str().unwrap() == expected,
                        "include {index} identity, digest and content"
                    );
                }
                assert!(input["history"].as_array().unwrap().is_empty());
            }
            4 => {
                assert_eq!(input["objective"], "second");
                assert_eq!(
                    input["history"][0]["assistant"].as_str().unwrap().len(),
                    32 * 1024
                );
            }
            _ => {
                assert!(input.get("workspace_guidance").is_none());
                assert!(input.get("includes").is_none());
                assert_eq!(input["items"].as_array().unwrap().len(), 2);
            }
        }
        if index == 5 {
            compaction_started
                .send(())
                .expect("compaction request gate");
            match case {
                CompactionCase::ProviderFailure => {
                    compaction_release
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release automatic failure");
                    write!(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .expect("synthetic failure response");
                    continue;
                }
                CompactionCase::Interrupted => {
                    let mut byte = [0];
                    assert_eq!(stream.read(&mut byte).expect("cancelled socket"), 0);
                    return;
                }
                CompactionCase::Success => {
                    compaction_release
                        .recv_timeout(Duration::from_secs(10))
                        .expect("release automatic summary");
                }
            }
        }
        if index == 6 {
            assert!(matches!(case, CompactionCase::ProviderFailure));
            compaction_started.send(()).expect("manual request gate");
            compaction_release
                .recv_timeout(Duration::from_secs(10))
                .expect("release manual rejection");
            write!(
                stream,
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("manual compaction rejection");
            continue;
        }
        let text = match index {
            0 => serde_json::json!({"outcome":{"type":"finish","summary":"ok","result":"ok"}}),
            1 => serde_json::json!({"outcome":{"type":"delegate","children":["synthetic child"]}}),
            2 => serde_json::json!({"summary":"synthetic summary"}),
            3 => {
                serde_json::json!({"outcome":{"type":"finish","summary":"first summary","result":"A".repeat(32 * 1024)}})
            }
            4 => {
                let result = if matches!(case, CompactionCase::Success) {
                    format!(
                        "second answer{}",
                        "Z".repeat(32 * 1024 - "second answer".len())
                    )
                } else {
                    "second answer".into()
                };
                serde_json::json!({"outcome":{"type":"finish","summary":"second summary","result":result}})
            }
            _ => serde_json::json!({"summary":"derived summary"}),
        };
        let response = serde_json::json!({
            "id": format!("resp_{}", index + 1),
            "status": "completed",
            "model": "model-1",
            "output": [{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text.to_string()}]}],
            "usage": {"input_tokens":20,"output_tokens":10}
        })
        .to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .expect("synthetic response");
    }
}

fn receive_stage(receiver: &mpsc::Receiver<&'static str>, expected: &str) {
    assert_eq!(
        receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("attached output stage"),
        expected
    );
}

fn run_attached_auto_compaction(case: CompactionCase) {
    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("AGENTS.md"), "G".repeat(20 * 1024)).expect("guidance");
    std::fs::write(workspace.join("a.txt"), "A".repeat(125 * 1024)).expect("first include");
    std::fs::write(workspace.join("b.txt"), "B".repeat(125 * 1024)).expect("second include");
    std::fs::write(workspace.join("private.txt"), "OMITTED_CANARY").expect("omitted input");
    let state = temp.path().join("state");
    StateRoot::admit(&state).expect("state root");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    let port = listener.local_addr().expect("listener address").port();
    let profile = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "local",
            "protocol": "openai-responses",
            "endpoint": format!("http://127.0.0.1:{port}/v1/responses"),
            "model": "model-1",
            "credential_env": "ARANY_PROVIDER_LOCAL_KEY",
            "outcome_encoding": "json_schema",
            "privacy": "user_authorized",
            "max_output_tokens": 4096,
            "capability_evidence_version": 1
        }]
    });
    let profile_path = state.join("provider-profiles.json");
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).expect("profile");
    std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o600))
        .expect("private profile");
    let (request_sender, request_ready) = mpsc::channel();
    let (release_compaction, compaction_release) = mpsc::sync_channel(1);
    let server =
        thread::spawn(move || synthetic_server(listener, case, request_sender, compaction_release));

    let mut check = Command::new(env!("CARGO_BIN_EXE_arany"));
    check
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["provider", "check", "local", "--state-dir"])
        .arg(&state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let check = check.spawn().expect("check process");
    let checked = wait_product(check);
    assert!(checked.status.success());
    assert_eq!(
        checked.stdout,
        b"Provider: local\nStatus: custom verified\n"
    );
    assert_eq!(checked.stderr, b"");

    let shell = if matches!(case, CompactionCase::Interrupted) {
        "trap ':' INT; printf 'ARANY_TEST_SHELL_PID:%s\n' \"$$\"; before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 --include a.txt --include b.txt \"$ARANY_TEST_FIRST_OBJECTIVE\"; exit_code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$exit_code\""
    } else {
        "printf 'ARANY_TEST_SHELL_PID:%s\n' \"$$\"; exec \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model model-1 --include a.txt --include b.txt \"$ARANY_TEST_FIRST_OBJECTIVE\""
    };
    let mut attached =
        crate::process::account_isolated_script(state.parent().expect("fixture root"));
    attached
        .env_clear()
        .env("ARANY_TEST_EXE", env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE", &state)
        .env("ARANY_TEST_WORKSPACE", &workspace)
        .env("ARANY_TEST_FIRST_OBJECTIVE", "Q".repeat(8192))
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .env("SHELL", "/bin/sh")
        .env("TERM", "dumb")
        .current_dir(&workspace)
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut attached = KillOnDrop(Some(attached.spawn().expect("attached PTY process")));
    let mut input = attached.child().stdin.take().expect("PTY input");
    let mut output = attached.child().stdout.take().expect("PTY output");
    assert_eq!(
        rustix::pipe::fcntl_setpipe_size(&output, 4096).expect("bounded output pipe"),
        4096
    );
    let (sender, receiver) = mpsc::channel();
    let (resume_output, output_resume) = mpsc::channel();
    let (pid_sender, pid_ready) = mpsc::sync_channel(1);
    let mut consent_input =
        std::fs::File::from(rustix::io::dup(&input).expect("owned fixture input"));
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut first_reported = false;
        let mut compaction_reported = false;
        let mut manual_reported = false;
        let mut admission_reported = false;
        let mut second_output_paused = false;
        let mut source_mismatch_reported = false;
        let mut continuation_reported = false;
        let mut retention_count = 0;
        let mut clear_count = 0;
        let mut pid_reported = false;
        let mut rejection_reported = false;
        loop {
            let mut chunk = [0; 1024];
            let count = output.read(&mut chunk).expect("PTY output bytes");
            if count == 0 {
                break;
            }
            assert!(bytes.len() + count <= 128 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(&mut consent_input, &bytes, &mut declined);
            if !pid_reported {
                for line in bytes.split(|byte| *byte == b'\n').rev().skip(1) {
                    if let Some(pid) = std::str::from_utf8(line)
                        .ok()
                        .and_then(|line| {
                            line.trim_end_matches('\r')
                                .strip_prefix("ARANY_TEST_SHELL_PID:")
                        })
                        .and_then(|pid| pid.parse::<u32>().ok())
                    {
                        pid_sender.send(pid).expect("private process PID");
                        pid_reported = true;
                        break;
                    }
                }
            }
            if matches!(case, CompactionCase::Success)
                && !second_output_paused
                && bytes
                    .windows(b"second answer".len())
                    .any(|part| part == b"second answer")
            {
                sender
                    .send("second restored output")
                    .expect("stage receiver");
                second_output_paused = true;
                output_resume
                    .recv_timeout(Duration::from_secs(10))
                    .expect("resume second output");
            }
            if !first_reported
                && bytes
                    .windows(b"Status: finished".len())
                    .any(|part| part == b"Status: finished")
            {
                sender.send("first receipt").expect("stage receiver");
                first_reported = true;
            }
            let compaction_marker = match case {
                CompactionCase::Success => b"Notice: Compaction saved; 20 input / 10 output tokens; Draft retained: 14 characters; Enter submits\r\nInput:\r\n".as_slice(),
                CompactionCase::ProviderFailure => b"Notice: Error: Compaction failed: Provider unavailable; usage unavailable; Draft retained: 14 characters; Enter submits\r\nInput:\r\n".as_slice(),
                CompactionCase::Interrupted => b"Notice: Automatic compaction unavailable: Compaction interrupted; check Session history; history retained; Draft retained: 14 characters; Enter submits\r\nInput:\r\n".as_slice(),
            };
            if !compaction_reported
                && bytes
                    .windows(compaction_marker.len())
                    .any(|part| part == compaction_marker)
            {
                sender.send("compaction receipt").expect("stage receiver");
                compaction_reported = true;
            }
            let mismatch = b"Notice: Error: Automatic compaction unavailable: saved account requires a native Provider; history retained\r\nInput:\r\n";
            if matches!(case, CompactionCase::Success)
                && !source_mismatch_reported
                && bytes.windows(mismatch.len()).any(|part| part == mismatch)
            {
                sender.send("wrong account source").expect("stage receiver");
                source_mismatch_reported = true;
            }
            let continuation =
                b"Notice: Draft: 9 characters; Enter retains until compaction ends\r\n";
            if !continuation_reported
                && bytes
                    .windows(continuation.len())
                    .any(|part| part == continuation)
            {
                sender.send("draft continuation").expect("stage receiver");
                continuation_reported = true;
            }
            let rejected = b"Notice: Error: invalid or overlong terminal line; draft unchanged\r\n";
            if !rejection_reported && bytes.windows(rejected.len()).any(|part| part == rejected) {
                sender.send("draft rejected").expect("stage receiver");
                rejection_reported = true;
            }
            let retention = b"Notice: Draft retained; press Enter after compaction to submit\r\n";
            let observed = bytes
                .windows(retention.len())
                .filter(|part| *part == retention)
                .count();
            if observed > retention_count {
                assert_eq!(observed, retention_count + 1);
                sender.send("draft retained").expect("stage receiver");
                retention_count = observed;
            }
            let cleared = b"^C\r\nInput:\r\n";
            let observed = bytes
                .windows(cleared.len())
                .filter(|part| *part == cleared)
                .count();
            if observed > clear_count {
                assert_eq!(observed, clear_count + 1);
                sender.send("draft cleared").expect("stage receiver");
                clear_count = observed;
            }
            let manual_marker = b"Notice: Error: Compaction failed: Provider rejected; usage unavailable; Draft retained: 6 characters; Enter submits\r\nInput:\r\n";
            if matches!(case, CompactionCase::ProviderFailure)
                && !manual_reported
                && bytes
                    .windows(manual_marker.len())
                    .any(|part| part == manual_marker)
            {
                sender
                    .send("manual compaction receipt")
                    .expect("stage receiver");
                manual_reported = true;
            }
            let admission_marker =
                b"Notice: Error: custom Provider profile file is unsafe\r\nInput:\r\n";
            if matches!(case, CompactionCase::ProviderFailure)
                && !admission_reported
                && bytes
                    .windows(admission_marker.len())
                    .any(|part| part == admission_marker)
            {
                sender
                    .send("compaction admission error")
                    .expect("stage receiver");
                admission_reported = true;
            }
        }
        bytes
    });
    receive_stage(&receiver, "first receipt");
    let shell_pid = pid_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("product parent PID");
    let product_pid = if matches!(case, CompactionCase::Interrupted) {
        product_child_of(shell_pid)
    } else {
        rustix::process::Pid::from_raw(shell_pid.try_into().expect("positive PID"))
            .expect("product PID")
    };
    let product_guard = ProductGuard::new(product_pid, state.clone());
    assert!(
        product_guard.is_owned_and_running(),
        "private product for cleanup"
    );
    input.write_all(b"second\n").expect("second objective");
    let future_account = uuid::Uuid::now_v7();
    if matches!(case, CompactionCase::Success) {
        receive_stage(&receiver, "second restored output");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("defaults runtime");
        runtime.block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(&state).expect("State reopen"),
                workspace.clone(),
            )
            .await
            .expect("fixture Session");
            assert_eq!(sessions.len(), 1);
            let store =
                Store::open_read_only(StateRoot::open_existing(&state).expect("State reopen"))
                    .expect("read-only Store");
            let view = store
                .load_view(sessions[0].id)
                .await
                .expect("committed view")
                .expect("Session");
            assert_eq!(view.runs.len(), 2);
            assert!(
                view.runs
                    .iter()
                    .all(|run| run.status == arany::RunStatus::Finished)
            );
            assert!(view.compactions.is_empty());
            store.close().await.expect("close Store");
            arany::set_session_defaults(
                StateRoot::open_existing(&state).expect("State reopen"),
                workspace.clone(),
                sessions[0].id,
                arany::SessionDefaults {
                    provider: Some("openai".into()),
                    model: Some("gpt-5.4".into()),
                    effort: None,
                    account_id: Some(future_account),
                    policy: arany::CollaborationPolicy::Single,
                },
            )
            .await
            .expect("change only next-Run defaults during restored output");
        });
        resume_output.send(()).expect("release second output");
    }
    request_ready
        .recv_timeout(Duration::from_secs(10))
        .expect("in-flight compaction request");
    input
        .write_all(b"retained \x04")
        .expect("continue maintenance draft");
    receive_stage(&receiver, "draft continuation");
    input
        .write_all(b"draft\n")
        .expect("retain maintenance draft");
    receive_stage(&receiver, "draft retained");
    if matches!(case, CompactionCase::Interrupted) {
        input
            .write_all(b"\x07\n")
            .expect("reject only malformed maintenance segment");
        receive_stage(&receiver, "draft rejected");
        input.write_all(b"\x03").expect("interrupt compaction");
    } else {
        release_compaction
            .send(())
            .expect("finish automatic maintenance");
    }
    receive_stage(&receiver, "compaction receipt");
    input
        .write_all(b"\x03")
        .expect("explicitly clear retained draft");
    receive_stage(&receiver, "draft cleared");
    if matches!(case, CompactionCase::ProviderFailure) {
        input
            .write_all(b"/compact\n")
            .expect("explicit manual compaction");
        request_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("manual compaction request");
        input
            .write_all(b"/setup\n")
            .expect("retain slash text during maintenance");
        receive_stage(&receiver, "draft retained");
        release_compaction
            .send(())
            .expect("finish manual maintenance");
        receive_stage(&receiver, "manual compaction receipt");
        input.write_all(b"\x03").expect("clear retained slash text");
        receive_stage(&receiver, "draft cleared");
        std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o644))
            .expect("make fixture profile unsafe");
        input
            .write_all(b"/compact\n")
            .expect("reject unsafe compaction profile");
        receive_stage(&receiver, "compaction admission error");
        std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o600))
            .expect("restore private fixture profile");
    }
    input.write_all(b"/quit\n").expect("exit command");
    drop(input);
    let result = wait_product(attached.take());
    let output = reader.join().expect("PTY output reader");
    server.join().expect("synthetic server completed");
    assert!(
        result.status.success(),
        "attached process exited successfully"
    );
    assert_eq!(result.stderr, b"");
    let transcript = String::from_utf8(output).expect("linear transcript UTF-8");
    let second_answer = transcript
        .find("second answer")
        .expect("second committed answer");
    let warning = transcript
        .find("Context threshold reached; attempting automatic compaction")
        .expect("durable threshold warning");
    let compacted = transcript
        .find(match case {
            CompactionCase::Success => "Compaction saved; 20 input / 10 output tokens",
            CompactionCase::ProviderFailure => {
                "Notice: Error: Compaction failed: Provider unavailable; usage unavailable"
            }
            CompactionCase::Interrupted => {
                "Automatic compaction unavailable: Compaction interrupted; check Session history; history retained"
            }
        })
        .expect("visible committed compaction outcome");
    assert!(second_answer < warning && warning < compacted);
    if matches!(case, CompactionCase::ProviderFailure) {
        let manual = "Notice: Error: Compaction failed: Provider rejected; usage unavailable; Draft retained: 6 characters; Enter submits\r\nInput:\r\n";
        assert!(
            compacted
                < transcript
                    .find(manual)
                    .expect("manual error after automatic error")
        );
        assert_eq!(transcript.matches(manual).count(), 1);
        let admission = "Notice: Error: custom Provider profile file is unsafe\r\nInput:\r\n";
        assert!(
            transcript.find(manual).expect("manual error")
                < transcript.find(admission).expect("admission error")
        );
        assert_eq!(transcript.matches(admission).count(), 1);
    }
    assert!(!transcript.contains("derived summary"));
    assert!(!transcript.contains("test-key"));
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(!transcript.contains('\x1b'), "linear output stays unstyled");
    if matches!(case, CompactionCase::Interrupted) {
        let tty = |label| {
            transcript
                .lines()
                .find_map(|line| line.trim_end_matches('\r').strip_prefix(label))
                .expect("TTY settings marker")
        };
        assert_eq!(tty("TTY_BEFORE:"), tty("TTY_AFTER:"));
    }
    let session_id = transcript
        .lines()
        .filter_map(|line| line.trim_end_matches('\r').strip_prefix("Session: "))
        .find_map(|value| value.parse::<SessionId>().ok())
        .expect("Session receipt");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only replay runtime");
    let store = Store::open_read_only(StateRoot::open_existing(&state).expect("state reopen"))
        .expect("read-only Store");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 2);
    if matches!(case, CompactionCase::Success) {
        assert_eq!(view.defaults.account_id, Some(future_account));
        assert_eq!(view.defaults.provider.as_deref(), Some("openai"));
        for run in &view.runs {
            let config = run.config.as_ref().expect("pinned config");
            assert_eq!(config.provider, "custom:local");
            assert_eq!(config.saved_api_account_id, None);
        }
    }
    assert!(
        view.runs
            .iter()
            .all(|run| run.status == arany::RunStatus::Finished)
    );
    assert_eq!(
        view.compactions.len(),
        match case {
            CompactionCase::Success => 1,
            CompactionCase::ProviderFailure => 2,
            CompactionCase::Interrupted => 0,
        }
    );
    assert!(!view.runs[0].config.as_ref().unwrap().auto_compaction_due());
    assert!(view.runs[1].config.as_ref().unwrap().auto_compaction_due());
    if let Some(compaction) = view.compactions.first() {
        assert_eq!(compaction.record.covered_run_id, view.runs[1].id);
        assert!(match case {
            CompactionCase::Success => matches!(
                compaction.record.status,
                arany::CompactionStatus::Succeeded { .. }
            ),
            CompactionCase::ProviderFailure => matches!(
                compaction.record.status,
                arany::CompactionStatus::Failed {
                    reason: arany::CompactionFailure::ProviderUnavailable
                }
            ),
            CompactionCase::Interrupted => false,
        });
    }
    if matches!(case, CompactionCase::ProviderFailure) {
        let manual = &view.compactions[1].record;
        assert_eq!(manual.covered_run_id, view.runs[1].id);
        assert!(matches!(
            manual.status,
            arany::CompactionStatus::Failed {
                reason: arany::CompactionFailure::ProviderRejected
            }
        ));
        assert_eq!(manual.input_tokens, None);
        assert_eq!(manual.output_tokens, None);
    }
    runtime.block_on(store.close()).expect("close Store");
    let database = std::fs::read(state.join("events.sqlite3")).expect("database bytes");
    assert!(
        !database
            .windows(b"test-key".len())
            .any(|part| part == b"test-key")
    );
    assert!(
        !database
            .windows(b"OMITTED_CANARY".len())
            .any(|part| part == b"OMITTED_CANARY")
    );
}

#[test]
fn attached_context_threshold_preserves_history_after_success_failure_or_interruption() {
    run_attached_auto_compaction(CompactionCase::Success);
    run_attached_auto_compaction(CompactionCase::ProviderFailure);
    run_attached_auto_compaction(CompactionCase::Interrupted);
}
