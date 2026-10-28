use super::*;
use arany::{ApprovalMode, ToolApprovals};
use std::collections::HashMap;

const CLIENT_TOKEN: &str = "synthetic-mcp-token-8841";
const RENEWED_TOKEN: &str = "synthetic-renewed-token-3157";
const LOGIN_TOKEN: &str = "login-access-canary-7731";
const LOGIN_REFRESH: &str = "login-refresh-canary-5519";
// Above the 64 KiB bound that caps token replies (`MAX_METADATA_BYTES`).
const OVERSIZED_TOKEN_REPLY_BYTES: usize = 70 * 1024;

#[derive(Clone)]
struct Exchange {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Mode {
    Normal,
    PingBeforeResult,
    DisconnectAfterCall,
    ReflectToken,
    LoseTokenReply,
    WrongReplyId,
    WrongReplyVersion,
    ReplyWithResultAndError,
    RevokeFails,
    RejectPingReply,
    TokenEncoded,
    TokenOversized,
    HoldTokenReply,
}

struct McpFixture {
    port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
    exchanges: Arc<Mutex<Vec<Exchange>>>,
    executed: Arc<AtomicUsize>,
    requested: Arc<tokio::sync::Notify>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for McpFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            thread.join().expect("MCP fixture joined");
        }
    }
}

fn read_exchange(stream: &mut std::net::TcpStream) -> Option<Exchange> {
    let mut head = Vec::new();
    let mut byte = [0];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > PAGE_HEADER_BYTES || stream.read(&mut byte).ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let text = String::from_utf8(head).ok()?;
    let mut lines = text.split("\r\n");
    let path = lines.next()?.split(' ').nth(1)?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(Some(0), |value| value.parse().ok())?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).ok()?;
    Some(Exchange {
        path,
        headers,
        body,
    })
}

fn tool(description: &str) -> Value {
    json!({"name":"echo","description":description,"inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}})
}

fn respond(status: &str, kind: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
}

struct Gates<'a> {
    answered: &'a std::sync::atomic::AtomicBool,
    requested: &'a tokio::sync::Notify,
    stop: &'a std::sync::atomic::AtomicBool,
}

// `None` closes the connection without any reply.
fn mcp_reply(
    exchange: &Exchange,
    mode: Mode,
    executed: &AtomicUsize,
    gates: &Gates<'_>,
    stream: &mut std::net::TcpStream,
) -> Option<String> {
    let host = exchange.headers.get("host").cloned().unwrap_or_default();
    if let Some(reply) = oauth_reply(&exchange.path, &host, mode) {
        return Some(reply);
    }
    if exchange.path == "/token" {
        return token_reply(exchange, mode, gates);
    }
    rpc_reply(exchange, mode, executed, gates, stream)
}

fn oauth_reply(path: &str, host: &str, mode: Mode) -> Option<String> {
    let reply = match path {
        path if path.starts_with("/.well-known/oauth-protected-resource") => {
            respond("404 Not Found", "text/plain", "", "")
        }
        "/.well-known/oauth-authorization-server" => {
            let origin = format!("http://{host}");
            let body = json!({"issuer":origin,"authorization_endpoint":format!("{origin}/authorize"),
                "token_endpoint":format!("{origin}/token"),"registration_endpoint":format!("{origin}/register"),
                "revocation_endpoint":format!("{origin}/revoke"),"code_challenge_methods_supported":["S256"]});
            respond("200 OK", "application/json", "", &body.to_string())
        }
        "/register" => {
            let body = json!({"client_id":"fixture-client"}).to_string();
            respond("201 Created", "application/json", "", &body)
        }
        "/revoke" if mode == Mode::RevokeFails => {
            respond("500 Internal Server Error", "text/plain", "", "")
        }
        "/revoke" => respond("200 OK", "text/plain", "", ""),
        _ => return None,
    };
    Some(reply)
}

fn token_reply(exchange: &Exchange, mode: Mode, gates: &Gates<'_>) -> Option<String> {
    gates.requested.notify_one();
    if mode == Mode::HoldTokenReply {
        while !gates.stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        return None;
    }
    if mode == Mode::LoseTokenReply {
        return None;
    }
    if String::from_utf8_lossy(&exchange.body).contains("grant_type=authorization_code") {
        let body = json!({"access_token":LOGIN_TOKEN,"token_type":"Bearer","expires_in":3600,
            "refresh_token":LOGIN_REFRESH})
        .to_string();
        return Some(respond("200 OK", "application/json", "", &body));
    }
    let body = format!(
        r#"{{"access_token":"{RENEWED_TOKEN}","token_type":"Bearer","expires_in":3600,"refresh_token":"rotated-refresh"}}"#
    );
    Some(match mode {
        Mode::TokenEncoded => respond(
            "200 OK",
            "application/json",
            "Content-Encoding: gzip\r\n",
            &body,
        ),
        Mode::TokenOversized => {
            let padded = format!(
                r#"{{"access_token":"{RENEWED_TOKEN}","token_type":"Bearer","expires_in":3600,"refresh_token":"rotated-refresh","pad":"{}"}}"#,
                "a".repeat(OVERSIZED_TOKEN_REPLY_BYTES)
            );
            respond("200 OK", "application/json", "", &padded)
        }
        _ => respond("200 OK", "application/json", "", &body),
    })
}

fn stream_ping(stream: &mut std::net::TcpStream) -> Option<()> {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).ok()?;
    stream
        .write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"ping-1\",\"method\":\"ping\"}\n\n")
        .ok()?;
    stream.flush().ok()
}

fn rpc_reply(
    exchange: &Exchange,
    mode: Mode,
    executed: &AtomicUsize,
    gates: &Gates<'_>,
    stream: &mut std::net::TcpStream,
) -> Option<String> {
    let bearer = exchange.headers.get("authorization");
    let admitted = [CLIENT_TOKEN, RENEWED_TOKEN]
        .iter()
        .any(|token| bearer.is_some_and(|value| value == &format!("Bearer {token}")));
    if !admitted {
        return Some(respond("401 Unauthorized", "text/plain", "", ""));
    }
    let message: Value = serde_json::from_slice(&exchange.body).unwrap_or(Value::Null);
    let id = &message["id"];
    if message.get("method").is_none() && message["id"] == "ping-1" {
        gates.answered.store(true, Ordering::SeqCst);
        return Some(if mode == Mode::RejectPingReply {
            respond("401 Unauthorized", "text/plain", "", "")
        } else {
            respond("202 Accepted", "text/plain", "", "")
        });
    }
    match message["method"].as_str() {
        Some("initialize") => Some(respond(
            "200 OK",
            "application/json",
            "Mcp-Session-Id: fixture-session\r\n",
            &json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}}).to_string(),
        )),
        Some("notifications/initialized") => Some(respond("202 Accepted", "text/plain", "", "")),
        Some("tools/list") => tools_list_reply(id, mode, gates, stream),
        Some("tools/call") => tools_call_reply(id, mode, executed, gates, stream),
        _ => Some(respond("400 Bad Request", "text/plain", "", "")),
    }
}

fn tools_list_reply(
    id: &Value,
    mode: Mode,
    gates: &Gates<'_>,
    stream: &mut std::net::TcpStream,
) -> Option<String> {
    let description = if mode == Mode::ReflectToken {
        format!("echo {CLIENT_TOKEN}")
    } else {
        "Echo".to_owned()
    };
    let reply = json!({"jsonrpc":"2.0","id":id,"result":{"tools":[tool(&description)]}});
    if mode == Mode::PingBeforeResult {
        stream_ping(stream)?;
        let deadline = std::time::Instant::now() + PAGE_IO_TIMEOUT;
        while !gates.answered.load(Ordering::SeqCst) {
            if std::time::Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return Some(format!("data: {reply}\n\n"));
    }
    Some(respond(
        "200 OK",
        "text/event-stream",
        "",
        &format!(": comment\nid: 1\ndata: {reply}\n\n"),
    ))
}

fn tools_call_reply(
    id: &Value,
    mode: Mode,
    executed: &AtomicUsize,
    gates: &Gates<'_>,
    stream: &mut std::net::TcpStream,
) -> Option<String> {
    executed.fetch_add(1, Ordering::SeqCst);
    if mode == Mode::DisconnectAfterCall {
        return None;
    }
    if mode == Mode::RejectPingReply {
        stream_ping(stream)?;
        let deadline = std::time::Instant::now() + PAGE_IO_TIMEOUT;
        while !gates.answered.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        return None;
    }
    let mut reply =
        json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"done"}]}});
    match mode {
        Mode::WrongReplyId => reply["id"] = json!(9999),
        Mode::WrongReplyVersion => reply["jsonrpc"] = json!("1.0"),
        Mode::ReplyWithResultAndError => reply["error"] = json!({"code":-1,"message":"x"}),
        _ => {}
    }
    Some(respond(
        "200 OK",
        "application/json",
        "",
        &reply.to_string(),
    ))
}

fn mcp_fixture(mode: Mode) -> McpFixture {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let gate = stop.clone();
    let exchanges = Arc::new(Mutex::new(Vec::new()));
    let recorded = exchanges.clone();
    let executed = Arc::new(AtomicUsize::new(0));
    let count = executed.clone();
    let answered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let requested = Arc::new(tokio::sync::Notify::new());
    let notify = requested.clone();
    let thread = std::thread::spawn(move || {
        let mut handlers = Vec::new();
        while let Ok((mut stream, _)) = listener.accept() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            let (recorded, count, answered, notify, gate) = (
                recorded.clone(),
                count.clone(),
                answered.clone(),
                notify.clone(),
                gate.clone(),
            );
            handlers.push(std::thread::spawn(move || {
                stream.set_read_timeout(Some(PAGE_IO_TIMEOUT)).unwrap();
                let Some(exchange) = read_exchange(&mut stream) else {
                    return;
                };
                recorded.lock().unwrap().push(exchange.clone());
                let gates = Gates {
                    answered: &answered,
                    requested: &notify,
                    stop: &gate,
                };
                let reply = mcp_reply(&exchange, mode, &count, &gates, &mut stream);
                if let Some(reply) = reply {
                    let _ = stream.write_all(reply.as_bytes());
                }
            }));
        }
        for handler in handlers {
            handler.join().expect("MCP handler joined");
        }
    });
    McpFixture {
        port,
        stop,
        exchanges,
        executed,
        requested,
        thread: Some(thread),
    }
}

fn write_token(port: u16, access: &str, expires_at: u64) {
    // The test Engine pins this isolated account root on open, so the grant must be saved there first.
    let root = StateRoot::admit(arany::test_support::isolate_account_root()).unwrap();
    let record = json!({"schema":1,"servers":{"remote":{
        "url": format!("http://127.0.0.1:{port}/mcp"),
        "client_id": "fixture-client",
        "token_endpoint": format!("http://127.0.0.1:{port}/token"),
        "revocation_endpoint": null,
        "access_token": access,
        "refresh_token": "initial-refresh",
        "expires_at": expires_at,
        "renewal": "ready",
        "claim": null,
    }}});
    root.record(arany::RecordKind::McpOauth)
        .replace(&serde_json::to_vec(&record).unwrap())
        .unwrap();
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

async fn run_remote(port: u16, call: ToolCall, approve: bool) -> Value {
    run_remote_until(port, call, approve, None).await
}

// With `cancel`, the Run is cancelled once that signal fires, e.g. when the token request arrives.
async fn run_remote_until(
    port: u16,
    call: ToolCall,
    approve: bool,
    cancel: Option<Arc<tokio::sync::Notify>>,
) -> Value {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let config = json!({"version":1,"workspace_paths":["src"],"write":false,
        "commands":[],"skills":[],"mcp":[{"name":"remote","tools":["echo"],
        "url": format!("http://127.0.0.1:{port}/mcp"), "auth": "oauth"}]});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Proposal {
        call,
        requests: requests.clone(),
        inspect_first: false,
        plan_on_inspection: false,
        omit_plan: false,
    });
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
    let cancellation = arany::RunCancellation::new();
    let cancel_run = cancellation.clone();
    let control = async {
        if approve {
            inbox.next().await.expect("one prompt").decide(true);
        }
        if let Some(requested) = &cancel {
            requested.notified().await;
            cancel_run.cancel();
        }
    };
    let (outcome, ()) = tokio::time::timeout(REMOTE_RUN_DEADLINE, async {
        tokio::join!(
            run_with(
                &engine,
                &provider,
                proposal_request(&workspace),
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: approve.then_some(approvals),
                    cancellation,
                    ..Default::default()
                },
            ),
            control
        )
    })
    .await
    .expect("remote MCP parent deadline");
    let outcome = outcome.unwrap();
    engine.close().await.unwrap();
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_seed_retired(observation.intent.id);
    let (events, replay) =
        arany::test_support::goals::replayed(&state_path, outcome.session_id).await;
    assert_eq!(replay.runs[0], outcome.run);
    let evidence = events
        .iter()
        .map(|event| event.event.payload().unwrap())
        .chain([format!("{:?}", requests.lock().unwrap())])
        .collect::<String>();
    json!({
        "disposition": format!("{:?}", observation.disposition),
        "output": serde_json::from_str::<Value>(&observation.output).unwrap_or(Value::Null),
        "text": observation.output,
        "status": format!("{:?}", outcome.run.status),
        "evidence": evidence,
    })
}

async fn list_remote(port: u16) -> Value {
    run_remote(
        port,
        ToolCall::McpList {
            server: "remote".into(),
        },
        false,
    )
    .await
}

fn echo_call(description: &str) -> ToolCall {
    ToolCall::McpCall {
        server: "remote".into(),
        tool: "echo".into(),
        schema_digest: hash(&serde_json::to_vec(&tool(description)).unwrap()),
        arguments: json!({"text": "hi"}).to_string(),
    }
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP through the egress proxy with a saved OAuth token"
)]
async fn remote_mcp_lists_tools_with_the_saved_token() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::Normal);
    write_token(server.port, CLIENT_TOKEN, unix_now() + 3600);
    let listed = list_remote(server.port).await;
    assert_eq!(listed["disposition"], "Succeeded");
    assert_eq!(listed["output"]["tools"][0]["name"], "echo");
    let exchanges = server.exchanges.lock().unwrap();
    assert_eq!(exchanges.len(), 3, "initialize, initialized and tools/list");
    for exchange in exchanges.iter() {
        assert_eq!(exchange.path, "/mcp");
        assert_eq!(
            exchange.headers["authorization"],
            format!("Bearer {CLIENT_TOKEN}")
        );
        assert_eq!(exchange.headers["mcp-protocol-version"], "2025-11-25");
    }
    assert!(!exchanges[0].headers.contains_key("mcp-session-id"));
    assert_eq!(exchanges[2].headers["mcp-session-id"], "fixture-session");
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: a missing token refuses before any request"
)]
async fn remote_mcp_without_a_saved_token_refuses_before_any_request() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::Normal);
    let listed = list_remote(server.port).await;
    assert_eq!(listed["disposition"], "Failed");
    assert_eq!(listed["output"]["error"], "unauthorized");
    assert!(server.exchanges.lock().unwrap().is_empty());
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: an expired token renews through the saved refresh token"
)]
async fn remote_mcp_renews_an_expired_token_before_the_call() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::Normal);
    write_token(server.port, "expired-token", unix_now() - 10);
    let listed = list_remote(server.port).await;
    assert_eq!(listed["disposition"], "Succeeded");
    let exchanges = server.exchanges.lock().unwrap();
    assert_eq!(exchanges[0].path, "/token");
    assert!(String::from_utf8_lossy(&exchanges[0].body).contains("grant_type=refresh_token"));
    assert!(
        exchanges[1..]
            .iter()
            .all(|exchange| exchange.headers["authorization"] == format!("Bearer {RENEWED_TOKEN}"))
    );
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: an unconfirmed refresh blocks every later use of the refresh token"
)]
async fn remote_mcp_never_resubmits_a_refresh_token_after_an_unconfirmed_reply() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    for mode in [
        Mode::LoseTokenReply,
        Mode::TokenEncoded,
        Mode::TokenOversized,
    ] {
        let server = mcp_fixture(mode);
        write_token(server.port, "expired-token", unix_now() - 10);
        let first = list_remote(server.port).await;
        assert_eq!(
            first["disposition"], "Uncertain",
            "the renewal may have rotated the token"
        );
        assert_ne!(first["status"], "Finished");
        let second = list_remote(server.port).await;
        assert_eq!(second["disposition"], "Failed");
        assert_eq!(second["output"]["error"], "unauthorized");
        let requests = server.exchanges.lock().unwrap();
        assert_eq!(requests.len(), 1, "one refresh attempt, never a second");
        assert_eq!(requests[0].path, "/token");
        let root = StateRoot::open_existing(&StateRoot::account_path().unwrap()).unwrap();
        let record = root
            .record(arany::RecordKind::McpOauth)
            .read()
            .unwrap()
            .unwrap();
        let record: Value = serde_json::from_slice(&record).unwrap();
        assert_eq!(record["servers"]["remote"]["renewal"], "renewing");
    }
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: cancelling during a renewal is uncertain and never refreshes twice"
)]
async fn remote_mcp_cancelled_during_a_renewal_is_uncertain_and_blocks_reuse() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::HoldTokenReply);
    write_token(server.port, "expired-token", unix_now() - 10);
    let call = ToolCall::McpList {
        server: "remote".into(),
    };
    let cancelled =
        run_remote_until(server.port, call, false, Some(server.requested.clone())).await;
    assert_eq!(cancelled["status"], "Cancelled");
    assert_eq!(cancelled["disposition"], "Uncertain");
    let reopened = list_remote(server.port).await;
    assert_eq!(reopened["disposition"], "Failed");
    assert_eq!(reopened["output"]["error"], "unauthorized");
    assert_eq!(
        server.exchanges.lock().unwrap().len(),
        1,
        "the reopened Session never refreshes again"
    );
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: a real deadline expiry or a dead launcher during a renewal is uncertain and blocks reuse"
)]
async fn remote_mcp_stopped_during_a_renewal_is_uncertain_and_blocks_reuse() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    for launcher_dies in [false, true] {
        let server = mcp_fixture(Mode::HoldTokenReply);
        write_token(server.port, "expired-token", unix_now() - 10);
        let call = ToolCall::McpList {
            server: "remote".into(),
        };
        let trigger = if launcher_dies {
            arany::FaultPlan::kill_launcher(call.clone())
        } else {
            arany::FaultPlan::cap_action_deadline(call.clone(), Duration::from_secs(2))
        };
        let run = run_remote(server.port, call, false);
        tokio::pin!(run);
        let stopped = tokio::select! {
            _ = &mut run => panic!("remote MCP stopped before the renewal request"),
            () = server.requested.notified() => {
                trigger.send(()).expect("renewal is still in flight");
                run.await
            }
        };
        assert_eq!(stopped["disposition"], "Uncertain");
        assert_ne!(stopped["status"], "Finished");
        let reopened = list_remote(server.port).await;
        assert_eq!(reopened["disposition"], "Failed");
        assert_eq!(reopened["output"]["error"], "unauthorized");
        assert_eq!(
            server.exchanges.lock().unwrap().len(),
            1,
            "the reopened Session never refreshes again"
        );
    }
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: a researcher proposing the remote server is refused before any request"
)]
async fn remote_mcp_is_refused_to_researchers_without_touching_credentials() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::Normal);
    write_token(server.port, "expired-token", unix_now() - 10);
    let account = StateRoot::account_path().unwrap().join("mcp-oauth.json");
    let before = std::fs::read(&account).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let config = json!({"version":1,"workspace_paths":["src"],"write":false,
        "commands":[],"skills":[],"mcp":[{"name":"remote","tools":["echo"],
        "url": format!("http://127.0.0.1:{}/mcp", server.port), "auth": "oauth"}]});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        ProviderOutcome::Delegate(arany::Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "List the remote server".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(ToolCall::McpList {
            server: "remote".into(),
        }),
    ]));
    let mut request = proposal_request(&workspace);
    request.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = tokio::time::timeout(
        REMOTE_RUN_DEADLINE,
        run_with(
            &engine,
            &provider,
            request,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("researcher parent deadline")
    .unwrap();
    engine.close().await.unwrap();
    assert_eq!(outcome.run.status, RunStatus::Failed);
    assert!(
        outcome.run.agents[1].tools.is_empty(),
        "refused before dispatch"
    );
    assert!(
        server.exchanges.lock().unwrap().is_empty(),
        "no token or MCP request"
    );
    assert_eq!(
        std::fs::read(&account).unwrap(),
        before,
        "saved grant untouched"
    );
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: a refused ping reply during a pending call stays uncertain"
)]
async fn remote_mcp_call_with_a_refused_ping_reply_is_uncertain() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::RejectPingReply);
    write_token(server.port, CLIENT_TOKEN, unix_now() + 3600);
    let called = run_remote(server.port, echo_call("Echo"), true).await;
    assert_eq!(server.executed.load(Ordering::SeqCst), 1);
    assert_eq!(called["disposition"], "Uncertain");
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: the client answers a server ping while the result is pending"
)]
async fn remote_mcp_answers_a_ping_before_the_server_finishes_its_stream() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::PingBeforeResult);
    write_token(server.port, CLIENT_TOKEN, unix_now() + 3600);
    let listed = list_remote(server.port).await;
    assert_eq!(listed["disposition"], "Succeeded");
    assert_eq!(listed["output"]["tools"][0]["name"], "echo");
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: a call whose reply is lost is uncertain and stops the Run"
)]
async fn remote_mcp_call_with_a_lost_reply_is_uncertain() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::DisconnectAfterCall);
    write_token(server.port, CLIENT_TOKEN, unix_now() + 3600);
    let called = run_remote(server.port, echo_call("Echo"), true).await;
    assert_eq!(
        server.executed.load(Ordering::SeqCst),
        1,
        "the server ran it"
    );
    assert_eq!(called["disposition"], "Uncertain");
    assert_ne!(
        called["status"], "Finished",
        "an unknown outcome stops the Run"
    );
    assert!(
        called["text"]
            .as_str()
            .unwrap()
            .contains("outcome of this remote MCP effect is unknown")
    );
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: a reflected bearer token never reaches Events, spills or the model"
)]
async fn remote_mcp_rejects_a_reflected_bearer_token() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    let server = mcp_fixture(Mode::ReflectToken);
    write_token(server.port, CLIENT_TOKEN, unix_now() + 3600);
    let listed = list_remote(server.port).await;
    assert_eq!(listed["disposition"], "Failed");
    for text in [
        listed["text"].as_str().unwrap(),
        listed["evidence"].as_str().unwrap(),
    ] {
        assert!(!text.contains(CLIENT_TOKEN), "credential escaped");
    }
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux remote MCP: an invalid reply to a dispatched call stays uncertain"
)]
async fn remote_mcp_call_with_an_invalid_reply_is_uncertain() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    for mode in [
        Mode::WrongReplyId,
        Mode::WrongReplyVersion,
        Mode::ReplyWithResultAndError,
    ] {
        let server = mcp_fixture(mode);
        write_token(server.port, CLIENT_TOKEN, unix_now() + 3600);
        let called = run_remote(server.port, echo_call("Echo"), true).await;
        assert_eq!(server.executed.load(Ordering::SeqCst), 1);
        assert_eq!(called["disposition"], "Uncertain");
    }
}

fn query_value(url: &str, key: &str) -> String {
    let parsed = url::Url::parse(url).unwrap();
    let value = parsed
        .query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned());
    value.unwrap_or_else(|| panic!("authorization URL lacks {key}"))
}

const MAX_CLI_CHANNEL_BYTES: usize = 4096;
const CLI_DEADLINE: Duration = Duration::from_secs(30);
const REMOTE_RUN_DEADLINE: Duration = Duration::from_secs(40);

fn arany_mcp(action: &str, state: &Path, workspace: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .args(["mcp", action, "remote", "--state-dir"])
        .arg(state)
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

struct CliRun {
    success: bool,
    stdout: String,
    stderr: String,
}

// Kills and reaps the child on every exit, including a failed assertion.
struct Supervised {
    child: std::process::Child,
    stdout: Option<std::thread::JoinHandle<String>>,
    stderr: Option<std::thread::JoinHandle<String>>,
}

impl Drop for Supervised {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// Keeps at most the cap in memory and discards the rest, so a flood fills neither the pipe nor memory.
// With `announced`, sends the first complete `http://` line as soon as it arrives.
fn drain(
    mut pipe: impl Read + Send + 'static,
    mut announced: Option<std::sync::mpsc::Sender<String>>,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = pipe.read(&mut buffer).unwrap_or(0);
            if read == 0 {
                break;
            }
            let room = (MAX_CLI_CHANNEL_BYTES + 1).saturating_sub(kept.len());
            kept.extend_from_slice(&buffer[..read.min(room)]);
            if let Some(sender) = &announced {
                let text = String::from_utf8_lossy(&kept).into_owned();
                if let Some(line) = text
                    .split_inclusive('\n')
                    .find(|line| line.starts_with("http://") && line.ends_with('\n'))
                {
                    let _ = sender.send(line.trim().to_owned());
                    announced = None;
                }
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    })
}

fn supervise(
    mut command: std::process::Command,
) -> (Supervised, std::sync::mpsc::Receiver<String>) {
    let mut child = command.spawn().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let stdout = drain(child.stdout.take().unwrap(), None);
    let stderr = drain(child.stderr.take().unwrap(), Some(sender));
    let run = Supervised {
        child,
        stdout: Some(stdout),
        stderr: Some(stderr),
    };
    (run, receiver)
}

fn complete(mut run: Supervised) -> CliRun {
    let deadline = std::time::Instant::now() + CLI_DEADLINE;
    let status = loop {
        if let Some(status) = run.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CLI exceeded its deadline"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    CliRun {
        success: status.success(),
        stdout: run.stdout.take().unwrap().join().unwrap(),
        stderr: run.stderr.take().unwrap().join().unwrap(),
    }
}

// Runs `arany mcp login`, answering its authorization URL like a browser redirect would.
fn cli_login(state: &Path, workspace: &Path) -> (CliRun, String) {
    let (run, announced) = supervise(arany_mcp("login", state, workspace));
    let url = announced
        .recv_timeout(CLI_DEADLINE)
        .expect("no URL announced");
    let redirect = url::Url::parse(&query_value(&url, "redirect_uri")).unwrap();
    let port = redirect.port().unwrap();
    let request = format!(
        "GET /callback?code=fixture-code&state={} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n",
        query_value(&url, "state")
    );
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(PAGE_IO_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(PAGE_IO_TIMEOUT)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut reply = Vec::new();
    let _ = stream.read_to_end(&mut reply);
    (complete(run), url)
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "isolated loopback authorization server: the shipped login and logout commands"
)]
async fn remote_mcp_login_and_logout_commands_run_the_full_oauth_flow() {
    if !crate::isolation::enter_native_account_isolated_test() {
        return;
    }
    // The shipped CLI resolves this same account root from the private fixture passwd home.
    let root = std::env::var_os("ARANY_TEST_CACHE_CANARY_ROOT").expect("private fixture root");
    assert_eq!(
        StateRoot::account_path().unwrap(),
        Path::new(&root).join("account-home/.local/state/arany")
    );
    for revoke_fails in [false, true] {
        let server = mcp_fixture(if revoke_fails {
            Mode::RevokeFails
        } else {
            Mode::Normal
        });
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let state_path = temp.path().join("state");
        StateRoot::admit(&state_path).unwrap();
        let config = json!({"version":1,"workspace_paths":["."],"write":false,
            "commands":[],"skills":[],"mcp":[{"name":"remote","tools":["echo"],
            "url": format!("http://127.0.0.1:{}/mcp", server.port), "auth": "oauth"}]});
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let (login, url) = cli_login(&state_path, &workspace);
        assert!(login.success);
        assert_eq!(login.stdout, "Authorized remote\n");
        assert!(login.stderr.contains("saved unencrypted"));
        assert_eq!(query_value(&url, "code_challenge_method"), "S256");
        assert_eq!(
            query_value(&url, "resource"),
            format!("http://127.0.0.1:{}/mcp", server.port)
        );
        let exchanges = server.exchanges.lock().unwrap();
        let redeem = exchanges
            .iter()
            .find(|exchange| exchange.path == "/token")
            .expect("code redeemed");
        let form = String::from_utf8_lossy(&redeem.body).into_owned();
        let verifier = form
            .split('&')
            .find_map(|pair| pair.strip_prefix("code_verifier="))
            .unwrap();
        let challenge = {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            use sha2::{Digest, Sha256};
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        };
        assert_eq!(query_value(&url, "code_challenge"), challenge);
        drop(exchanges);
        let account = StateRoot::open_existing(&StateRoot::account_path().unwrap()).unwrap();
        let saved = account
            .record(arany::RecordKind::McpOauth)
            .read()
            .unwrap()
            .unwrap();
        let saved: Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(saved["servers"]["remote"]["access_token"], LOGIN_TOKEN);

        let logout = complete(supervise(arany_mcp("logout", &state_path, &workspace)).0);
        assert!(logout.success, "logout succeeds even when revocation fails");
        assert_eq!(logout.stdout, "Removed the authorization for remote\n");
        assert_eq!(
            logout.stderr.contains("did not confirm revocation"),
            revoke_fails
        );
        let exchanges = server.exchanges.lock().unwrap();
        let revoke = exchanges
            .iter()
            .find(|exchange| exchange.path == "/revoke")
            .expect("revocation attempted");
        assert!(String::from_utf8_lossy(&revoke.body).contains(LOGIN_REFRESH));
        assert!(
            account
                .record(arany::RecordKind::McpOauth)
                .read()
                .unwrap()
                .is_none(),
            "the local grant is removed either way"
        );
        for run in [&login, &logout] {
            for channel in [&run.stdout, &run.stderr] {
                assert!(channel.len() <= MAX_CLI_CHANNEL_BYTES, "bounded output");
                for canary in [LOGIN_TOKEN, LOGIN_REFRESH, "fixture-code"] {
                    assert!(!channel.contains(canary), "credential reached output");
                }
            }
        }
    }
}
