use super::write_input;
use crate::loopback::{ChildGuard, PRODUCT_EXIT_WAIT, wait_product};
use arany::StateRoot;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

pub(crate) fn tls_request(pipe: &mut impl Read) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut bytes = Vec::new();
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        let mut chunk = [0; 4096];
        match pipe.read(&mut chunk) {
            Ok(0) => return None,
            Ok(count) => {
                assert!(bytes.len() + count <= 256 * 1024);
                bytes.extend_from_slice(&chunk[..count]);
                if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let length = std::str::from_utf8(&bytes[..end])
                        .unwrap()
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|size| size.parse::<usize>().ok())
                        })
                        .expect("Content-Length");
                    if bytes.len() == end + 4 + length {
                        return Some(bytes);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::yield_now(),
            Err(_) => return None,
        }
    }
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux Tools in a PTY, local TLS and actual Guard"
)]
fn read_group_appends_once_and_denied_read_stays_separate() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let fixture_deadline = crate::isolation::native_test_deadline() - PRODUCT_EXIT_WAIT;
    assert!(
        Instant::now() < fixture_deadline,
        "native fixture startup deadline"
    );
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let tls = crate::tls_fixture::build(root);
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/note.txt"), b"one\ntwo\n").unwrap();
    std::fs::write(workspace.join("private.txt"), b"OMITTED_CANARY").unwrap();
    let skill = workspace.join(".agents/skills/synthetic");
    std::fs::create_dir_all(&skill).unwrap();
    let mut skill_body = "---\nname: synthetic\ndescription: Synthetic guidance\n---\n".to_owned();
    skill_body.push_str(&"x".repeat(1536 - skill_body.len()));
    std::fs::write(skill.join("SKILL.md"), skill_body).unwrap();
    let state = root.join("state");
    StateRoot::admit(&state).unwrap();
    crate::private_file(
        &state.join("tools.json"),
        br#"{"version":1,"workspace_paths":["src",".agents/skills"],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    );
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let mut server = ChildGuard::new(
        Command::new("/usr/bin/openssl")
            .args(["s_server", "-quiet", "-accept"])
            .arg(format!("127.0.0.1:{port}"))
            .arg("-cert")
            .arg(&tls.cert)
            .arg("-key")
            .arg(&tls.key)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "TLS fixture start deadline");
        thread::yield_now();
    }
    let mut peer_out = server.child().stdout.take().unwrap();
    let flags = rustix::fs::fcntl_getfl(&peer_out).unwrap();
    rustix::fs::fcntl_setfl(&peer_out, flags | rustix::fs::OFlags::NONBLOCK).unwrap();
    let mut peer_in = server.child().stdin.take().unwrap();
    thread::spawn(move || {
        loop {
            let Some(bytes) = tls_request(&mut peer_out) else {
                return;
            };
            let end = bytes
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap()
                + 4;
            let body: Value = serde_json::from_slice(&bytes[end..]).unwrap();
            let step = crate::semantic::tool_results(&body).len();
            let call = |name: &str, arguments: Value| json!([{"type":"function_call","id":format!("fc_{step}"),"call_id":format!("call_{step}"),"status":"completed","name":name,"arguments":arguments.to_string()}]);
            let output = match step {
                0 => call("arany_list", json!({"path":"src"})),
                1 => call(
                    "arany_read",
                    json!({"path":"src/note.txt","offset":null,"limit":1024}),
                ),
                2 => call("arany_skill", json!({"name":"synthetic","resource":null})),
                3 => call(
                    "arany_read",
                    json!({"path":"private.txt","offset":null,"limit":1024}),
                ),
                _ => arany::semantic::responses_output(&json!("TP7_FINAL_ANSWER")),
            };
            let response = json!({"id":format!("resp_{step}"),"status":"completed","model":"gpt-5.4","output":output,"usage":{"input_tokens":12,"output_tokens":8}});
            let (kind, response) = if body["stream"] == true {
                (
                    "text/event-stream",
                    crate::loopback::responses_stream(response),
                )
            } else {
                ("application/json", response.to_string())
            };
            if write!(peer_in, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).is_err() || peer_in.flush().is_err() {
                return;
            }
        }
    });

    let command = "stty rows 24 cols 80; \"$ARANY_TEST_EXE\" --screen-reader --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider openai --model gpt-5.4 --tools read; exit $?";
    let mut attached = crate::product::PtyLaunch::new(crate::product::Isolation::Caller, command)
        .session(&state, &workspace)
        .env("PATH", "/usr/bin:/bin")
        .env("LD_PRELOAD", &tls.shim)
        .env("ARANY_TEST_TLS_PORT", port.to_string())
        .env("SSL_CERT_FILE", &tls.ca)
        .env("OPENAI_API_KEY", "synthetic-native-key")
        .spawn();
    let input = Arc::new(Mutex::new(attached.child().stdin.take().unwrap()));
    let mut output = crate::product::Blocking(attached.child().stdout.take().unwrap());
    let reader_input = Arc::clone(&input);
    let (done, finished) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut declined = false;
        let mut bytes = Vec::new();
        let mut closed = false;
        loop {
            let mut chunk = [0; 4096];
            let count = output.read(&mut chunk).unwrap_or(0);
            if count == 0 {
                return bytes;
            }
            assert!(bytes.len() + count <= 64 * 1024, "bounded PTY output");
            bytes.extend_from_slice(&chunk[..count]);
            crate::process::decline_workspace_consent(
                &mut *reader_input.lock().unwrap(),
                &bytes,
                &mut declined,
            );
            let text = String::from_utf8_lossy(&bytes);
            if !closed
                && let (Some(answer), Some(settled)) = (
                    text.find("TP7_FINAL_ANSWER"),
                    text.find("Last task: finished\r\n"),
                )
                && text[answer.max(settled)..].contains("Input:\r\n")
            {
                closed = true;
                done.send(()).unwrap();
            }
        }
    });
    let waited = finished.recv_timeout(fixture_deadline.saturating_duration_since(Instant::now()));
    if waited.is_err() {
        crate::process::annotate_native_fixture(&format!(
            "fixture=tool_group; stage=settled_prompt; wait={waited:?}"
        ));
    }
    if waited.is_ok() {
        write_input(&input, b"\x04");
    }
    let result = wait_product(attached.take());
    let transcript = String::from_utf8_lossy(&reader.join().unwrap()).replace("\r\n", "\n");
    let group = transcript
        .matches(
            "Read 1 file, listed 1 directory, loaded 1 Skill\nResult: 1 entry, 2 lines, 1.5 KB",
        )
        .count();
    let denied = transcript
        .matches("Read(private.txt)\nResult: denied")
        .count();
    if waited.is_err() || group != 1 || denied != 1 {
        let tail: String = transcript
            .chars()
            .rev()
            .take(700)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|c| {
                if c.is_ascii_alphanumeric() || " _=:,.-()".contains(c) {
                    c
                } else {
                    '.'
                }
            })
            .collect();
        crate::process::annotate_native_fixture(&format!(
            "fixture=tool_group; stage=transcript; answered={}; group={group}; denied={denied}; tail={tail}",
            waited.is_ok()
        ));
    }
    assert!(
        waited.is_ok(),
        "final answer and subsequent input prompt missing"
    );
    assert_eq!(group, 1);
    assert_eq!(denied, 1);
    assert!(!transcript.contains("Tool ·") && !transcript.contains("succeeded"));
    assert!(!transcript.contains(['⏺', '⎿']));
    assert!(!transcript.contains("OMITTED_CANARY"));
    assert!(
        result.status.success(),
        "attached exit: {:?}",
        result.status
    );
}
