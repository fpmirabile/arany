use super::*;
use crate::process::BoundedOutput;
use std::{net::TcpStream, os::unix::fs::PermissionsExt, process::Child};

const STAGE: &str = "ARANY_TEST_OFFLINE_HTTPS_SETUP_STAGE";
const TEST: &str = "setup::offline_https::release_private_file_setup_reuses_saved_account";
const KEY: &str = "synthetic-offline-api-key";
const AMBIENT_WORKSPACE: &str = "wrkspc_AmbientMustNotApply";
const SETUP_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --setup --state-dir /root/state-one --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const BARE_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --state-dir /root/state-two --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const TURN_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --state-dir /root/state-three --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const CHECK_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --state-dir /root/state-four --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const EMPTY_CATALOG_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --state-dir /root/state-five --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";
const LOADING_CATALOG_SHELL: &str = "before=$(stty -g); printf 'TTY_BEFORE:%s\n' \"$before\"; /arany --screen-reader --state-dir /root/state-six --workspace /root/workspace; code=$?; after=$(stty -g); printf 'TTY_AFTER:%s\n' \"$after\"; exit \"$code\"";

#[path = "offline_https/chatgpt.rs"]
mod chatgpt;

#[path = "offline_https/anthropic.rs"]
mod anthropic;

struct Server {
    child: Child,
    responder: Option<thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(responder) = self.responder.take() {
            let _ = responder.join();
        }
    }
}

fn openssl(args: &[&str], dir: &Path) {
    let status = Command::new("/usr/bin/openssl")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("OpenSSL fixture command");
    assert!(status.success(), "OpenSSL fixture command failed");
}

fn fixture(dir: &Path) {
    openssl(
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=arany-offline-root",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-addext",
            "keyUsage=critical,keyCertSign,cRLSign",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
        ],
        dir,
    );
    openssl(
        &[
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=api.openai.com",
            "-addext",
            "subjectAltName=DNS:api.openai.com,DNS:auth.openai.com,DNS:api.anthropic.com",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "extendedKeyUsage=serverAuth",
            "-keyout",
            "server.key",
            "-out",
            "server.csr",
        ],
        dir,
    );
    openssl(
        &[
            "x509",
            "-req",
            "-in",
            "server.csr",
            "-CA",
            "ca.pem",
            "-CAkey",
            "ca.key",
            "-CAcreateserial",
            "-days",
            "1",
            "-copy_extensions",
            "copy",
            "-out",
            "server.pem",
        ],
        dir,
    );
    for (path, body) in [
        ("v1/models", br#"{"object":"list","data":[{"id":"gpt-5.4","object":"model","created":1686935002,"owned_by":"openai"},{"id":"gpt-offline-new","object":"model","created":1686935002,"owned_by":"openai"}]}"#.as_slice()),
        ("empty/v1/models", br#"{"object":"list","data":[]}"#.as_slice()),
    ] {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().expect("catalog directory"))
            .expect("catalog response directory");
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        std::fs::write(path, response).expect("catalog response");
    }
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "resp_offline",
        "status": "completed",
        "model": "gpt-5.4",
        "output": [{
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": "{\"outcome\":{\"type\":\"finish\",\"summary\":\"offline summary\",\"result\":\"offline answer\"}}"
            }]
        }],
        "usage": {"input_tokens": 12, "output_tokens": 8}
    }))
    .expect("synthetic Responses body");
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(&body);
    std::fs::write(dir.join("v1/responses"), response).expect("Responses response");
}

fn start_server(directory: &str) -> Server {
    let child = Command::new("/usr/bin/openssl")
        .args([
            "s_server",
            "-HTTP",
            "-accept",
            "127.0.0.1:443",
            "-cert",
            "/fixture/server.pem",
            "-key",
            "/fixture/server.key",
        ])
        .current_dir(directory)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("isolated HTTPS server");
    let server = Server {
        child,
        responder: None,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect("127.0.0.1:443").is_err() {
        assert!(
            Instant::now() < deadline,
            "isolated HTTPS server did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    server
}

enum NativeReplies {
    Direct,
    Unreviewed,
    ScopedAnthropic,
    PendingCatalog(std::sync::mpsc::SyncSender<()>),
}

fn start_response_server(replies: NativeReplies) -> Server {
    let mut child = Command::new("/usr/bin/openssl")
        .args([
            "s_server",
            "-quiet",
            "-accept",
            "127.0.0.1:443",
            "-cert",
            "/fixture/server.pem",
            "-key",
            "/fixture/server.key",
        ])
        .current_dir("/fixture")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("isolated Responses HTTPS server");
    let mut input = child.stdin.take().expect("HTTPS response pipe");
    let mut output = child.stdout.take().expect("HTTPS request pipe");
    let responder = thread::spawn(move || {
        match replies {
            NativeReplies::ScopedAnthropic => {
                anthropic::respond(&mut input, &mut output);
                return;
            }
            NativeReplies::Unreviewed => {
                respond_unreviewed_native(&mut input, &mut output);
                return;
            }
            NativeReplies::PendingCatalog(started) => {
                let request = chatgpt::read_request(&mut output);
                assert!(request.starts_with(b"GET /v1/models HTTP/1.1\r\n"));
                let authorization = format!("authorization: Bearer {KEY}\r\n");
                assert!(
                    request
                        .windows(authorization.len())
                        .any(|part| part == authorization.as_bytes())
                );
                started.send(()).expect("pending catalog request gate");
                let mut extra = [0];
                assert_eq!(output.read(&mut extra).expect("pending server shutdown"), 0);
                return;
            }
            NativeReplies::Direct => {}
        }
        let mut request = Vec::new();
        let mut chunk = [0; 4096];
        let header_end = loop {
            let count = output.read(&mut chunk).expect("HTTPS request read");
            assert!(count > 0, "HTTPS peer closed before request");
            request.extend_from_slice(&chunk[..count]);
            assert!(
                request.len() <= 64 * 1024,
                "HTTPS request exceeded fixture cap"
            );
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = std::str::from_utf8(&request[..header_end]).expect("HTTP headers");
        assert!(headers.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        assert!(
            headers.lines().any(|line| line
                .trim_end_matches('\r')
                .eq_ignore_ascii_case(&format!("authorization: Bearer {KEY}"))),
            "selected synthetic key was not sent to the compiled endpoint"
        );
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(str::to_owned)
            })
            .expect("request content length")
            .parse()
            .expect("numeric content length");
        assert!(length <= 64 * 1024 - header_end, "request body cap");
        while request.len() < header_end + length {
            let count = output.read(&mut chunk).expect("HTTPS request body");
            assert!(count > 0, "HTTPS peer closed during request body");
            request.extend_from_slice(&chunk[..count]);
            assert!(
                request.len() <= 64 * 1024,
                "HTTPS request exceeded fixture cap"
            );
        }
        let body: serde_json::Value =
            serde_json::from_slice(&request[header_end..header_end + length])
                .expect("bounded Responses request");
        assert!(
            !request[header_end..header_end + length]
                .windows(KEY.len())
                .any(|part| part == KEY.as_bytes()),
            "selected key entered the Provider body"
        );
        assert_eq!(body["model"], "gpt-5.4");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["max_output_tokens"], 4096);
        let provider_input: serde_json::Value =
            serde_json::from_str(body["input"].as_str().expect("structured Provider input"))
                .expect("structured Provider input JSON");
        assert_eq!(provider_input["objective"], "offline objective");
        let response = std::fs::read("/fixture/v1/responses").expect("synthetic response");
        input.write_all(&response).expect("HTTPS response write");
    });
    let server = Server {
        child,
        responder: Some(responder),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect("127.0.0.1:443").is_err() {
        assert!(
            Instant::now() < deadline,
            "isolated Responses server did not start"
        );
        thread::sleep(Duration::from_millis(20));
    }
    server
}

fn respond_unreviewed_native(input: &mut impl Write, output: &mut impl Read) {
    for index in 0..2 {
        let outcome = serde_json::json!({"outcome": {"type": "finish", "summary": "offline summary", "result": "offline answer"}});
        let request = chatgpt::read_request(output);
        let end = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .expect("native headers")
            + 4;
        let headers = std::str::from_utf8(&request[..end]).expect("native headers UTF-8");
        assert!(headers.starts_with("POST /v1/responses HTTP/1.1\r\n"));
        assert!(headers.lines().any(|line| {
            line.trim_end_matches('\r')
                .eq_ignore_ascii_case(&format!("authorization: Bearer {KEY}"))
        }));
        assert_eq!(
            request
                .windows(KEY.len())
                .filter(|part| *part == KEY.as_bytes())
                .count(),
            1
        );
        let body: serde_json::Value =
            serde_json::from_slice(&request[end..]).expect("native request body");
        assert_eq!(body["model"], "gpt-offline-new");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], false);
        assert_eq!(body["max_output_tokens"], 4096);
        assert_eq!(body["text"]["format"]["name"], "arany_outcome");
        assert_eq!(body["text"]["format"]["strict"], true);
        let content = body["input"].as_str().expect("native input");
        let context: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(context["objective"], "offline objective");
        let response = serde_json::to_vec(&serde_json::json!({
            "id": format!("resp_native_{index}"), "status": "completed", "model": "gpt-offline-new",
            "output": [{"type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": outcome.to_string()}]}],
            "usage": {"input_tokens": 12, "output_tokens": 8}
        })).expect("native response JSON");
        input.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len()).as_bytes()).expect("native response headers");
        input.write_all(&response).expect("native response body");
    }
}

fn wait_for_sanitized(
    output: &mut impl Read,
    input: &mut impl Write,
    transcript: &mut Vec<u8>,
    answered: &mut crate::session_picker::PtyResponses,
    start: usize,
    needle: &[u8],
) -> usize {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(offset) = transcript[start..]
            .windows(needle.len())
            .position(|part| part == needle)
        {
            return start + offset;
        }
        pump(output, input, transcript, answered);
        assert!(
            Instant::now() < deadline,
            "setup stage missing {:?}: {}",
            String::from_utf8_lossy(needle),
            tail(transcript).replace(KEY, "[redacted]")
        );
        thread::yield_now();
    }
}

fn run_pty(shell: &str, answers: &[(&[u8], &[u8])], completion: Option<&[u8]>) -> Vec<u8> {
    run_pty_with_gate(shell, answers, completion, |_| {})
}

fn run_pty_with_gate(
    shell: &str,
    answers: &[(&[u8], &[u8])],
    completion: Option<&[u8]>,
    mut before_answer: impl FnMut(&[u8]),
) -> Vec<u8> {
    let mut command = Command::new("/usr/bin/script");
    command
        .args(["-q", "-e", "-c", shell, "/dev/null"])
        .env_clear()
        .env("SHELL", "/usr/bin/sh")
        .env("TERM", "dumb")
        .env("HOME", "/root")
        .env("XDG_STATE_HOME", "/root/legacy-state")
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .env("ANTHROPIC_WORKSPACE_ID", AMBIENT_WORKSPACE)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGuard::new(command.spawn().expect("offline setup PTY"));
    let mut input = child.child().stdin.take().expect("PTY input");
    let mut output = child.child().stdout.take().expect("PTY output");
    let flags = fcntl_getfl(&output).expect("stdout flags");
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).expect("nonblocking PTY output");
    let mut transcript = Vec::new();
    let mut answered = crate::session_picker::PtyResponses::default();
    let mut after_answer = 0;
    for &(needle, answer) in answers {
        let stage_at = wait_for_sanitized(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_answer,
            needle,
        );
        let catalog_busy = needle == b"Notice: Loading model catalog; Ctrl+C cancels"
            || needle.ends_with(b"catalog loading; Ctrl+C cancels")
            || needle.ends_with(b"draft unchanged; catalog loading");
        if needle != b"Choose number, n next, p previous, or q close:"
            && needle != b"Choose number, exact ID, n next, p previous, or q close:"
            && !catalog_busy
        {
            wait_for_sanitized(
                &mut output,
                &mut input,
                &mut transcript,
                &mut answered,
                stage_at,
                b"Input:\r\n",
            );
        }
        after_answer = transcript.len();
        before_answer(needle);
        input.write_all(answer).expect("setup answer");
    }
    if let Some(completion) = completion {
        after_answer = wait_for_sanitized(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            after_answer,
            completion,
        );
    }
    wait_for_sanitized(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        after_answer,
        b"Input:\r\n",
    );
    let quit_at = transcript.len();
    input.write_all(b"/quit\r").expect("quit Session");
    wait_for_sanitized(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        quit_at,
        b"TTY_AFTER:",
    );
    let result = wait_product(child.take());
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    assert!(result.status.success(), "offline setup product exit");
    assert_eq!(
        answered.cursor_queries, 0,
        "screen-reader mode queried the cursor"
    );
    assert!(
        !transcript
            .windows(KEY.len())
            .any(|part| part == KEY.as_bytes()),
        "API key entered terminal output"
    );
    assert!(
        !transcript.contains(&b'\x1b'),
        "screen-reader output has escapes"
    );
    let text = std::str::from_utf8(&transcript).expect("screen-reader UTF-8");
    assert!(
        !text.contains('─'),
        "screen-reader output has decorative rules"
    );
    let marker = |name: &str| {
        text.lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix(name))
            .expect("terminal marker")
            .to_owned()
    };
    assert_eq!(marker("TTY_BEFORE:"), marker("TTY_AFTER:"));
    transcript
}

fn session(state: &str) -> SessionView {
    let state = Path::new(state);
    let journal = std::fs::read(state.join("events.sqlite3")).expect("Session journal");
    assert!(
        !journal
            .windows(KEY.len())
            .any(|part| part == KEY.as_bytes()),
        "API key entered Session journal"
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("read-only runtime");
    runtime.block_on(async {
        let sessions = arany::list_sessions(
            StateRoot::open_existing(state).expect("existing State"),
            PathBuf::from("/root/workspace"),
        )
        .await
        .expect("Workspace Sessions");
        assert_eq!(sessions.len(), 1);
        let store = Store::open_read_only(StateRoot::open_existing(state).expect("State reopen"))
            .expect("read-only Store");
        let events = store.load_session(sessions[0].id).await.expect("Events");
        let view = SessionView::replay(sessions[0].id, &events)
            .expect("strict replay")
            .expect("Session");
        store.close().await.expect("close Store");
        view
    })
}

fn unsaved(state: &str) {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("replay runtime")
        .block_on(async {
            let sessions = arany::list_sessions(
                StateRoot::open_existing(Path::new(state)).expect("State"),
                PathBuf::from("/root/workspace"),
            )
            .await
            .expect("Workspace Sessions");
            assert!(
                sessions.is_empty(),
                "setup and startup must not save an empty conversation"
            );
        });
}

fn isolated_stage() {
    assert_eq!(nix::unistd::geteuid().as_raw(), 0);
    assert_eq!(
        StateRoot::account_path().unwrap(),
        Path::new("/root/.local/state/arany")
    );
    let status = Command::new("/usr/bin/ip")
        .args(["link", "set", "lo", "up"])
        .status()
        .expect("bring up private loopback");
    assert!(status.success(), "private loopback unavailable");
    let hosts = Command::new("/usr/bin/getent")
        .args(["hosts", "api.openai.com"])
        .bounded_output()
        .expect("private hostname lookup");
    assert!(hosts.status.success());
    assert!(hosts.stdout.starts_with(b"127.0.0.1 "));

    let server = start_server("/fixture");
    let first_output = run_pty(
        SETUP_SHELL,
        &[
            (b"type a choice name: API key or ChatGPT plan", b"API key\r"),
            (
                b"type a choice name: Cancel or Use private file",
                b"Use private file\r",
            ),
            (b"type a choice name: OpenAI or Anthropic", b"OpenAI\r"),
            (b"Setup: Enter API key", b"synthetic-offline-api-key\r"),
        ],
        None,
    );
    let setup_text = String::from_utf8_lossy(&first_output);
    assert!(setup_text.contains("openai ready: gpt-5.4 · low"));
    assert!(!setup_text.contains("Choose model effort"));
    assert!(!setup_text.contains("Billable model check"));
    assert!(!setup_text.contains("Choose number"));
    assert!(
        String::from_utf8_lossy(&first_output).contains("NOT encrypted; same-user apps read key")
    );
    drop(server);

    let account_root = StateRoot::open_existing(Path::new("/root/.local/state/arany"))
        .expect("saved account root");
    let record = account_root
        .read_saved_account_record()
        .expect("saved account record")
        .expect("saved account");
    let json: serde_json::Value = serde_json::from_slice(&record).expect("account JSON");
    assert_eq!(json["storage"], "private_file");
    assert_eq!(json["account"]["provider"], "openai");
    assert_eq!(json["account"]["model"], "gpt-5.4");
    assert!(
        json["account"]["api_key"] == KEY,
        "saved synthetic key differs"
    );
    let account_id: Uuid = json["account"]["id"]
        .as_str()
        .expect("account ID")
        .parse()
        .expect("account UUID");
    let metadata = std::fs::metadata(account_root.path().join("account-credentials.json"))
        .expect("account file metadata");
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    unsaved("/root/state-one");
    assert!(setup_text.contains("Provider: openai\r\nModel: gpt-5.4\r\n"));

    let second_output = run_pty(BARE_SHELL, &[], None);
    assert!(!String::from_utf8_lossy(&second_output).contains("Choose access method"));
    unsaved("/root/state-two");
    assert!(
        String::from_utf8_lossy(&second_output).contains("Provider: openai\r\nModel: gpt-5.4\r\n")
    );

    let server = start_response_server(NativeReplies::Direct);
    let turn_output = run_pty(
        TURN_SHELL,
        &[(b"Input:\r\n", b"offline objective\r")],
        Some("Arany · Answer:\r\n  offline answer".as_bytes()),
    );
    assert!(
        String::from_utf8_lossy(&turn_output).contains("Arany · Answer:\r\n  offline answer"),
        "direct turn output: {}",
        String::from_utf8_lossy(&turn_output)
            .replace(KEY, "[redacted]")
            .chars()
            .take(4096)
            .flat_map(char::escape_default)
            .collect::<String>()
    );
    drop(server);
    let third = session("/root/state-three");
    assert_eq!(third.defaults.account_id, Some(account_id));
    assert_eq!(third.runs.len(), 1, "direct turn count");
    let run = &third.runs[0];
    assert_eq!(run.objective, "offline objective");
    assert_eq!(run.status, arany::RunStatus::Finished);
    assert_eq!(run.assistant_message.as_deref(), Some("offline answer"));
    let config = run.config.as_ref().expect("pinned Run config");
    assert_eq!(config.provider, "openai");
    assert_eq!(config.model, "gpt-5.4");
    assert_eq!(config.saved_api_account_id, Some(account_id));
    assert_eq!(run.agents.len(), 1);
    assert_eq!(run.agents[0].provider_calls.len(), 1);
    let call = &run.agents[0].provider_calls[0];
    assert_eq!(call.response_id.as_deref(), Some("resp_offline"));
    assert_eq!(call.input_tokens, Some(12));
    assert_eq!(call.output_tokens, Some(8));
    assert_eq!(
        call.wire_provenance,
        Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
    );

    let mut server = start_response_server(NativeReplies::Unreviewed);
    let output = run_pty(
        CHECK_SHELL,
        &[
            (b"Input:\r\n", b"/model gpt-offline-new high\r"),
            (b"Notice: Model: gpt-offline-new", b"offline objective\r"),
        ],
        Some(b"Answer:\r\n  offline answer"),
    );
    assert!(String::from_utf8_lossy(&output).contains("Answer:\r\n  offline answer"));
    let headless = Command::new("/usr/bin/timeout")
        .args([
            "-k",
            "1s",
            "15s",
            "/arany",
            "exec",
            "--provider",
            "openai",
            "--model",
            "gpt-offline-new",
            "--effort",
            "high",
            "--state-dir",
            "/root/state-native-exec",
            "--workspace",
            "/root/workspace",
            "--collaboration",
            "single",
            "--output",
            "jsonl",
            "offline objective",
        ])
        .env_clear()
        .env("OPENAI_API_KEY", KEY)
        .env("ANTHROPIC_API_KEY", "unselected-poison-key")
        .env("ANTHROPIC_WORKSPACE_ID", AMBIENT_WORKSPACE)
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .current_dir("/root/workspace")
        .bounded_output()
        .expect("native headless Run without evidence");
    assert!(headless.status.success());
    assert!(headless.stderr.is_empty());
    assert!(headless.stdout.ends_with(b"\n"));
    let direct = session("/root/state-native-exec");
    assert_eq!(direct.runs.len(), 1);
    let pinned = direct.runs[0].config.as_ref().unwrap();
    assert_eq!(pinned.model, "gpt-offline-new");
    assert_eq!(pinned.effort, Some(arany::Effort::High));
    assert_eq!(pinned.saved_api_account_id, None);
    assert_eq!(pinned.provider_concurrency, 1);
    let shown = Command::new("/arany")
        .args([
            "show",
            "--state-dir",
            "/root/state-native-exec",
            "--output",
            "jsonl",
        ])
        .arg(direct.id.to_string())
        .env_clear()
        .current_dir("/root/workspace")
        .bounded_output()
        .unwrap();
    assert!(shown.status.success());
    assert!(shown.stderr.is_empty());
    assert_eq!(
        headless.stdout, shown.stdout,
        "JSONL must equal closed public replay"
    );
    server
        .responder
        .take()
        .expect("native checked peer")
        .join()
        .expect("native checked HTTPS requests");
    drop(server);
    let checked = session("/root/state-four");
    assert_eq!(checked.defaults.model.as_deref(), Some("gpt-offline-new"));
    assert_eq!(checked.defaults.effort, Some(arany::Effort::High));
    assert_eq!(checked.defaults.account_id, Some(account_id));
    assert_eq!(checked.runs.len(), 1, "native checker is not a Run");
    let run = &checked.runs[0];
    assert_eq!(run.status, arany::RunStatus::Finished);
    assert_eq!(run.objective, "offline objective");
    assert_eq!(run.assistant_message.as_deref(), Some("offline answer"));
    let config = run.config.as_ref().expect("checked native config");
    assert_eq!(config.model, "gpt-offline-new");
    assert_eq!(config.effort, Some(arany::Effort::High));
    assert_eq!(config.saved_api_account_id, Some(account_id));
    assert_eq!(config.provider_concurrency, 1);
    assert_eq!(
        run.agents[0].provider_calls[0].response_id.as_deref(),
        Some("resp_native_0")
    );

    let server = start_server("/fixture/empty");
    run_pty(
        EMPTY_CATALOG_SHELL,
        &[
            (b"Input:\r\n", b"/setup\r"),
            (b"type a choice name: API key or ChatGPT plan", b"API key\r"),
            (
                b"type a choice name: Cancel or Use private file",
                b"Use private file\r",
            ),
            (b"type a choice name: OpenAI or Anthropic", b"OpenAI\r"),
            (b"Setup: Enter API key", b"synthetic-offline-api-key\r"),
        ],
        Some(b"Notice: Error: selected account has no visible models; use /setup to retry"),
    );
    drop(server);
    let account_after = account_root
        .read_saved_account_record()
        .expect("account after empty catalog")
        .expect("saved account remains");
    assert!(
        record == account_after,
        "empty catalog changed saved account"
    );
    unsaved("/root/state-five");

    let mut preferences: serde_json::Value = serde_json::from_slice(
        &account_root
            .read_model_preferences_record()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    for source in preferences["sources"].as_array_mut().unwrap() {
        if source["account_id"] == account_id.to_string() {
            source["catalog"] = serde_json::json!([]);
        }
    }
    account_root
        .with_account_replacement_lock(std::path::Path::new("/root/workspace"), || {
            account_root
                .replace_model_preferences_record(&serde_json::to_vec(&preferences).unwrap())
        })
        .unwrap()
        .unwrap();
    let (started, request_started) = std::sync::mpsc::sync_channel(1);
    let mut server = start_response_server(NativeReplies::PendingCatalog(started));
    let output = run_pty_with_gate(
        LOADING_CATALOG_SHELL,
        &[
            (b"Input:\r\n", b"/model\r"),
            (
                b"Notice: Loading model catalog; Ctrl+C cancels",
                b"catalog draft\x04",
            ),
            (
                b"Notice: Draft: 13 characters; catalog loading; Ctrl+C cancels",
                b"\x01\x04",
            ),
            (
                b"Notice: Error: invalid or overlong terminal line; draft unchanged; catalog loading",
                b"!\x04",
            ),
            (
                b"Notice: Draft: 14 characters; catalog loading; Ctrl+C cancels",
                b"\x03",
            ),
            (b"Notice: Model catalog cancelled", b"\x04"),
            (
                b"Notice: Draft retained; press Enter to submit or Ctrl+C to clear",
                b"\x03",
            ),
        ],
        None,
        |needle| {
            if needle == b"Notice: Loading model catalog; Ctrl+C cancels" {
                request_started
                    .recv_timeout(Duration::from_secs(5))
                    .expect("catalog GET accepted before drafting");
            }
        },
    );
    server.child.kill().expect("stop held catalog server");
    server.child.wait().expect("reap held catalog server");
    server
        .responder
        .take()
        .expect("pending catalog reader")
        .join()
        .expect("one catalog request and bounded shutdown");
    drop(server);
    let text = std::str::from_utf8(&output).expect("catalog-loading transcript");
    assert_eq!(
        text.matches("Notice: Draft: 13 characters; catalog loading; Ctrl+C cancels\r\n")
            .count(),
        1
    );
    assert_eq!(
        text.matches(
            "Notice: Error: invalid or overlong terminal line; draft unchanged; catalog loading\r\n"
        )
        .count(),
        1
    );
    assert_eq!(
        text.matches("Notice: Draft: 14 characters; catalog loading; Ctrl+C cancels\r\n")
            .count(),
        1
    );
    assert!(!text.contains("Notice: Catalog loading; draft retained; Ctrl+C cancels"));
    unsaved("/root/state-six");
    assert!(text.contains("Provider: openai\r\nModel: gpt-offline-new\r\n"));
    assert!(
        record
            == account_root
                .read_saved_account_record()
                .expect("account after loading")
                .expect("saved account"),
        "catalog loading changed saved account"
    );
    anthropic::stage();
}

#[test]
#[ignore = "native Linux no-network HTTPS setup gate; requires OpenSSL, ip, unshare, bwrap, and script"]
fn release_private_file_setup_reuses_saved_account() {
    if std::env::var_os(STAGE).is_some() {
        isolated_stage();
        return;
    }

    let temp = tempfile::tempdir().expect("private offline setup fixture");
    let fixture_dir = temp.path().join("fixture");
    let home = temp.path().join("home");
    std::fs::create_dir(&fixture_dir).expect("fixture directory");
    std::fs::create_dir(&home).expect("private home");
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
        .expect("owner-only home");
    std::fs::create_dir(home.join("workspace")).expect("Workspace");
    fixture(&fixture_dir);
    let hosts = temp.path().join("hosts");
    std::fs::write(&hosts, b"127.0.0.1 api.openai.com api.anthropic.com\n")
        .expect("private hosts file");
    let nsswitch = temp.path().join("nsswitch.conf");
    std::fs::write(&nsswitch, b"passwd: files\nhosts: files\n")
        .expect("private name-service policy");
    let passwd = temp.path().join("passwd");
    std::fs::write(&passwd, b"owner:x:0:0::/root:/usr/bin/sh\n").expect("private passwd file");
    let output = Command::new("/usr/bin/timeout")
        .args([
            "-k",
            "2s",
            "60s",
            "/usr/bin/unshare",
            "--map-auto",
            "--map-user",
            "0",
            "--map-group",
            "0",
            "--user",
            "--mount",
            "--net",
            "--fork",
            "--kill-child",
            "/usr/bin/bwrap",
            "--unshare-pid",
            "--die-with-parent",
            "--tmpfs",
            "/",
            "--ro-bind",
            "/usr",
            "/usr",
            "--symlink",
            "usr/bin",
            "/bin",
            "--symlink",
            "usr/lib",
            "/lib",
            "--symlink",
            "usr/lib",
            "/lib64",
            "--ro-bind",
        ])
        .arg(&passwd)
        .args(["/etc/passwd", "--ro-bind"])
        .arg(&hosts)
        .args(["/etc/hosts", "--ro-bind"])
        .arg(&nsswitch)
        .args(["/etc/nsswitch.conf", "--chmod", "0755", "/etc", "--bind"])
        .arg(&home)
        .args(["/root", "--ro-bind"])
        .arg(&fixture_dir)
        .args(["/fixture", "--ro-bind"])
        .arg(std::env::current_exe().expect("test executable"))
        .args([
            "/test",
            "--ro-bind",
            env!("CARGO_BIN_EXE_arany"),
            "/arany",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--clearenv",
            "--setenv",
            STAGE,
            "1",
            "--",
            "/test",
            "--exact",
            TEST,
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .bounded_output_for(Duration::from_secs(65), 64 * 1024)
        .expect("isolated offline setup gate");
    assert!(
        output.status.success(),
        "isolated offline setup failed: {}",
        format!(
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .replace(KEY, "[redacted]")
        .chars()
        .take(4096)
        .flat_map(char::escape_default)
        .collect::<String>()
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
}
