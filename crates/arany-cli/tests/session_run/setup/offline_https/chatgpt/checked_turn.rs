use super::*;

const MAX_CATALOG_READS: usize = 3;
const MAX_BACKEND_REQUESTS: usize = MAX_CATALOG_READS + 1;
const MAX_FAILURE_CHARACTERS: usize = 2048;
const ANSWER: &str = "offline ChatGPT answer";

pub(super) fn backend_server(started: Arc<AtomicBool>, access: &'static str) -> Server {
    let mut child = tls_server("127.0.0.1:443");
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let responder = thread::spawn(move || {
        for position in 0..MAX_BACKEND_REQUESTS {
            let request = read_request(&mut output);
            started.store(true, Ordering::SeqCst);
            let text = std::str::from_utf8(&request).unwrap();
            for header in [
                format!("authorization: Bearer {access}"),
                format!("chatgpt-account-id: {ACCOUNT}"),
                "originator: arany".to_owned(),
                "host: chatgpt.com".to_owned(),
            ] {
                assert!(
                    text.lines()
                        .any(|line| line.trim_end_matches('\r').eq_ignore_ascii_case(&header)),
                    "backend account header missing"
                );
            }
            if text.starts_with("GET ") {
                assert!(position < MAX_CATALOG_READS, "catalog read limit exceeded");
                assert!(text.starts_with(&format!(
                    "GET /backend-api/codex/models?client_version={} HTTP/1.1\r\n",
                    arany::ChatGptProvider::CATALOG_CLIENT_VERSION
                )));
                send_json(&mut input, br#"{"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list","context_window":272000}]}"#);
            } else {
                assert!(text.starts_with("POST /backend-api/codex/responses HTTP/1.1\r\n"));
                assert_backend_request(text);
                let body = serde_json::json!({"type":"response.completed","response":{
                    "id":"resp_chatgpt","status":"completed","model":"gpt-6.1-sol",
                    "output":[{"id":"msg_chatgpt","type":"message","role":"assistant","status":"completed","phase":"final_answer","content":[{"type":"output_text","text":ANSWER}]}],
                    "usage":{"input_tokens":12,"output_tokens":8}
                }});
                let stream = format!("data: {body}\n\n");
                write!(input, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{stream}", stream.len()).unwrap();
                return;
            }
        }
        panic!("backend request limit reached without the requested inference");
    });
    Server {
        child,
        responder: Some(responder),
    }
}

fn assert_backend_request(request: &str) {
    let body: serde_json::Value =
        serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["stream"], true);
    assert!(body["input"].is_array());
    assert!(
        body["instructions"]
            .as_str()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(
        body["include"],
        serde_json::json!(["reasoning.encrypted_content"])
    );
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("previous_response_id").is_none());
    let session_id = body["prompt_cache_key"].as_str().unwrap();
    for header in [
        format!("session-id: {session_id}"),
        format!("x-client-request-id: {session_id}"),
        "openai-beta: responses=experimental".into(),
    ] {
        assert!(
            request
                .lines()
                .any(|line| line.trim_end_matches('\r').eq_ignore_ascii_case(&header))
        );
    }
}

pub(super) fn renew_once(input: &mut impl Write, output: &mut impl Read, nonce: &str) {
    let request = read_request(output);
    assert!(request.starts_with(b"POST /oauth/token HTTP/1.1\r\n"));
    let body = request
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .unwrap()
        + 4;
    let fields = url::form_urlencoded::parse(&request[body..])
        .into_owned()
        .collect::<Vec<_>>();
    assert_eq!(fields.len(), 3);
    assert!(
        fields
            .iter()
            .any(|field| field == &("grant_type".into(), "refresh_token".into()))
    );
    assert!(
        fields
            .iter()
            .any(|field| field == &("client_id".into(), CLIENT.into()))
    );
    assert!(
        fields
            .iter()
            .any(|field| field == &("refresh_token".into(), REFRESH.into()))
    );
    let (keys, identity) = identity_fixture(nonce);
    let token = serde_json::to_vec(&serde_json::json!({
        "access_token": RENEWED_ACCESS, "refresh_token": REFRESH,
        "id_token": identity, "expires_in": 3600,
    }))
    .unwrap();
    send_json(input, &token);
    let request = read_request(output);
    assert!(request.starts_with(b"GET /.well-known/jwks.json HTTP/1.1\r\n"));
    send_json(input, &keys);
}

fn product(args: &[&str]) -> std::process::Output {
    let output = Command::new("/arany")
        .args(args)
        .env_clear()
        .env("HOME", "/root")
        .env("SSL_CERT_FILE", "/fixture/ca.pem")
        .current_dir("/root/workspace")
        .bounded_output()
        .unwrap();
    let mut diagnostic = String::from_utf8_lossy(&output.stderr).into_owned();
    for secret in [ACCESS, RENEWED_ACCESS, REFRESH, ACCOUNT] {
        diagnostic = diagnostic.replace(secret, "[redacted]");
    }
    assert!(
        output.status.success(),
        "product {} exited {:?}: {}",
        args[0],
        output.status.code(),
        diagnostic
            .chars()
            .take(MAX_FAILURE_CHARACTERS)
            .flat_map(char::escape_default)
            .collect::<String>()
    );
    output
}

pub(super) fn assert_backend_turn(root: &StateRoot, expected_index: &[u8]) {
    let models = product(&["provider", "models", "chatgpt"]);
    assert!(models.status.success());
    assert!(models.stderr.is_empty());
    assert!(
        std::str::from_utf8(&models.stdout)
            .unwrap()
            .contains("gpt-6.1-sol")
    );
    let turn = product(&[
        "exec",
        "--provider",
        "chatgpt",
        "--model",
        "gpt-6.1-sol",
        "--effort",
        "low",
        "--output",
        "jsonl",
        "--state-dir",
        "/root/state-exec",
        "--workspace",
        "/root/workspace",
        "offline ChatGPT objective",
    ]);
    assert!(turn.status.success());
    assert!(turn.stderr.is_empty());
    let view = session("/root/state-exec");
    assert_eq!(view.runs.len(), 1);
    assert_eq!(view.runs[0].status, arany::RunStatus::Finished);
    assert_eq!(view.runs[0].assistant_message.as_deref(), Some(ANSWER));
    assert_eq!(view.runs[0].agents[0].provider_calls.len(), 1);
    let shown = product(&[
        "show",
        "--state-dir",
        "/root/state-exec",
        "--output",
        "jsonl",
        &view.id.to_string(),
    ]);
    assert!(shown.status.success());
    assert!(shown.stderr.is_empty());
    assert!(
        turn.stdout == shown.stdout,
        "closed replay must equal committed JSONL"
    );
    for secret in [ACCESS, RENEWED_ACCESS, REFRESH, ACCOUNT] {
        for bytes in [&turn.stdout, &shown.stdout, &turn.stderr] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes())
            );
        }
    }
    assert!(
        root.record(RecordKind::ChatgptAccounts)
            .read()
            .unwrap()
            .unwrap()
            == expected_index
    );
}
