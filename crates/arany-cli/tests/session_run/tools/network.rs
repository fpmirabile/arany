use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

const LIMIT_SCRIPT: &str = r#"set -eu
ok=0; limited=0
for _ in $(seq 66); do
  exec 3<>/dev/tcp/127.0.0.1/3128
  printf 'CONNECT %s HTTP/1.1\r\n\r\n' "$1" >&3
  IFS= read -r status <&3
  case "$status" in *" 200 "*) ok=$((ok+1));; *" 429 "*) limited=$((limited+1));; esac
  exec 3<&-
done
echo "ok:$ok limited:$limited"
"#;

const SATURATE_SCRIPT: &str = r#"set -eu
exec python3 - "$1" "$2" <<'PYTHON'
import socket
import subprocess
import sys

HOLDERS = 2
HOLDER = r"""
import socket
import sys

connections = []
for _ in range(int(sys.argv[2])):
    stream = socket.create_connection(("127.0.0.1", 3128))
    stream.sendall(("CONNECT " + sys.argv[1] + " HTTP/1.1\r\n\r\n").encode())
    response = stream.makefile("rb", buffering=0)
    assert response.readline() == b"HTTP/1.1 200 Connection Established\r\n"
    assert response.readline() == b"\r\n"
    connections.append((stream, response))
print("admitted", flush=True)
assert sys.stdin.buffer.read(1) == b""
for stream, response in connections:
    response.close()
    stream.close()
"""

count = int(sys.argv[2])
assert count % HOLDERS == 0
holders = []
try:
    for _ in range(HOLDERS):
        holders.append(subprocess.Popen(
            [sys.executable, "-c", HOLDER, sys.argv[1], str(count // HOLDERS)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE))
    for holder in holders:
        assert holder.stdout.readline() == b"admitted\n"
    with socket.create_connection(("127.0.0.1", 3128)) as probe:
        probe.sendall(("CONNECT " + sys.argv[1] + " HTTP/1.1\r\n\r\n").encode())
        with probe.makefile("rb", buffering=0) as response:
            assert response.readline() == b"HTTP/1.1 429 Too Many Requests\r\n"
    print("saturated:1")
finally:
    for holder in holders:
        holder.stdin.close()
    statuses = [holder.wait() for holder in holders]
    assert all(status == 0 for status in statuses), statuses
PYTHON
"#;

const SCRIPT: &str = r#"set -eu
echo "proxy:${HTTPS_PROXY}:${NO_PROXY-unset}"
if (exec 4<>/dev/tcp/127.0.0.1/"$2") 2>/dev/null; then echo direct:reachable; else echo direct:blocked; fi
python3 - <<'PY'
import socket
try:
    pair = socket.socketpair(socket.AF_UNIX, socket.SOCK_DGRAM)
    pair[0].connect("/arany-egress.sock")
    print("unix:reachable")
except PermissionError:
    print("unix:blocked")
PY
exec 3<>/dev/tcp/127.0.0.1/3128
printf 'CONNECT %s HTTP/1.1\r\n\r\n' "$1" >&3
IFS= read -r status <&3
echo "status:${status%$'\r'}"
case "$status" in *" 200 "*)
  IFS= read -r blank <&3
  printf 'GET / HTTP/1.0\r\n\r\n' >&3
  tail -n 1 <&3
esac
"#;

fn fixture_upstream() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            count.fetch_add(1, Ordering::SeqCst);
            let mut request = [0; 256];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(b"HTTP/1.0 200 OK\r\n\r\nhello-from-fixture\n");
        }
    });
    (port, accepted)
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux egress proxy: allow-listed tunnel, unlisted refusal record and no direct route"
)]
async fn egress_proxy_is_the_only_route_and_records_refusals() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let (upstream, accepted) = fixture_upstream();
    let (direct, direct_accepted) = fixture_upstream();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    private_file(&workspace.join("src/egress.sh"), SCRIPT.as_bytes());
    for (target, listed) in [
        (format!("127.0.0.1:{upstream}"), true),
        ("evil.example:443".to_owned(), false),
    ] {
        let state_path = temp.path().join(format!("state-{listed}"));
        let state = StateRoot::admit(&state_path).unwrap();
        let config = json!({"version":1,"workspace_paths":["src"],"write":true,
            "commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],
            "skills":[],"mcp":[],"network":{"allow":["127.0.0.1"]}});
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Proposal {
            call: ToolCall::Command {
                program: "bash".into(),
                args: vec!["src/egress.sh".into(), target.clone(), direct.to_string()],
                cwd: "".into(),
            },
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
        let outcome = tokio::time::timeout(
            Duration::from_secs(15),
            run_with(
                &engine,
                &provider,
                proposal_request(&workspace),
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            ),
        )
        .await
        .expect("egress case parent deadline")
        .unwrap();
        engine.close().await.unwrap();
        assert_eq!(outcome.run.status, RunStatus::Finished, "{target}");
        let observation = outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition,
            ToolDisposition::Succeeded,
            "{target}"
        );
        let output: Value = serde_json::from_str(&observation.output).unwrap();
        let stdout = output["stdout"].as_str().unwrap();
        assert!(
            stdout.starts_with("proxy:http://127.0.0.1:3128:\ndirect:blocked\nunix:blocked\n"),
            "{stdout}"
        );
        if listed {
            assert!(
                stdout
                    .ends_with("status:HTTP/1.1 200 Connection Established\nhello-from-fixture\n"),
                "{stdout}"
            );
            assert!(output.get("network_refusals").is_none(), "{output}");
        } else {
            assert!(
                stdout.ends_with("status:HTTP/1.1 403 Forbidden\n"),
                "{stdout}"
            );
            assert_eq!(
                output["network_refusals"],
                json!([{"host": "evil.example", "reason": "unlisted", "count": 1}])
            );
        }
        let connections = accepted.load(Ordering::SeqCst);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
        assert_eq!(view.runs[0], outcome.run);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            connections,
            "replay is offline"
        );
        store.close().await.unwrap();
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(direct_accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux egress proxy: connections past the total bound through the payload-facing endpoint are recorded"
)]
async fn connections_past_the_total_bound_are_recorded_through_the_endpoint() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let (upstream, _accepted) = fixture_upstream();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    private_file(&workspace.join("src/limit.sh"), LIMIT_SCRIPT.as_bytes());
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let config = json!({"version":1,"workspace_paths":["src"],"write":true,
        "commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],
        "skills":[],"mcp":[],"network":{"allow":["127.0.0.1"]}});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Proposal {
        call: ToolCall::Command {
            program: "bash".into(),
            args: vec!["src/limit.sh".into(), format!("127.0.0.1:{upstream}")],
            cwd: "".into(),
        },
        requests,
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
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_with(
            &engine,
            &provider,
            proposal_request(&workspace),
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("limit case parent deadline")
    .unwrap();
    engine.close().await.unwrap();
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    let output: Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(output["stdout"], "ok:64 limited:2\n", "{output}");
    assert_eq!(
        output["network_refusals"],
        json!([{"host": "127.0.0.1", "reason": "limit", "count": 2}])
    );
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux egress proxy: admitted concurrent tunnels stay held until one overflow is recorded"
)]
async fn held_tunnels_record_one_overflow_through_the_endpoint() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }

    let (upstream, _accepted) = fixture_upstream();
    let connections = arany::limits::LIMITS
        .rows
        .iter()
        .find(|limit| limit.name == "MAX_NETWORK_CONNECTIONS")
        .unwrap()
        .value;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    private_file(
        &workspace.join("src/saturate.sh"),
        SATURATE_SCRIPT.as_bytes(),
    );
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let config = json!({"version":1,"workspace_paths":["src"],"write":true,
        "commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],
        "skills":[],"mcp":[],"network":{"allow":["127.0.0.1"]}});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let provider = Arc::new(Proposal {
        call: ToolCall::Command {
            program: "bash".into(),
            args: vec![
                "src/saturate.sh".into(),
                format!("127.0.0.1:{upstream}"),
                connections.to_string(),
            ],
            cwd: "".into(),
        },
        requests: Arc::new(Mutex::new(Vec::new())),
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
    let outcome = tokio::time::timeout(
        Duration::from_secs(40),
        run_with(
            &engine,
            &provider,
            proposal_request(&workspace),
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("saturation case parent deadline")
    .unwrap();
    engine.close().await.unwrap();
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    let output: Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(output["stdout"], "saturated:1\n", "{output}");
    assert_eq!(
        observation.disposition,
        ToolDisposition::Succeeded,
        "{output}"
    );
    assert_eq!(
        output["network_refusals"],
        json!([{"host": "127.0.0.1", "reason": "limit", "count": 1}]),
        "{output}"
    );
}

const MAX_PAGE_CONNECTIONS: usize = 8;
const PAGE_HEADER_BYTES: usize = 8192;
const PAGE_IO_TIMEOUT: Duration = Duration::from_secs(10);

#[path = "network/web_journeys.rs"]
mod web_journeys;
#[path = "network/mcp_journeys.rs"]
mod mcp_journeys;

struct PageServer {
    port: u16,
    stop: Arc<std::sync::atomic::AtomicBool>,
    requests: Arc<AtomicUsize>,
    headers: Arc<Mutex<Vec<Vec<u8>>>>,
    received: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for PageServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            thread.join().expect("page fixture joined");
        }
    }
}

#[derive(Clone)]
struct PageReply {
    content_type: &'static str,
    body: Vec<u8>,
    status: &'static str,
    location: String,
    hold: bool,
}

fn page_upstream(content_type: &'static str, body: Vec<u8>) -> PageServer {
    page_server(PageReply {
        content_type,
        body,
        status: "200 OK",
        location: String::new(),
        hold: false,
    })
}

fn page_server(reply: PageReply) -> PageServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let headers = Arc::new(Mutex::new(Vec::new()));
    let recorded = headers.clone();
    let received = Arc::new(tokio::sync::Notify::new());
    let notify = received.clone();
    let closed = Arc::new(AtomicUsize::new(0));
    let eof = closed.clone();
    let thread = std::thread::spawn(move || {
        let mut handlers = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            if flag.load(Ordering::SeqCst) {
                break;
            }
            if handlers.len() >= MAX_PAGE_CONNECTIONS {
                continue;
            }
            count.fetch_add(1, Ordering::SeqCst);
            let (reply, recorded, notify, eof) =
                (reply.clone(), recorded.clone(), notify.clone(), eof.clone());
            handlers.push(std::thread::spawn(move || {
                serve_page(stream, reply, recorded, notify, eof);
            }));
        }
        for handler in handlers {
            handler.join().expect("page handler joined");
        }
    });
    PageServer {
        port,
        stop,
        requests,
        headers,
        received,
        closed,
        thread: Some(thread),
    }
}

fn serve_page(
    mut stream: std::net::TcpStream,
    reply: PageReply,
    headers: Arc<Mutex<Vec<Vec<u8>>>>,
    received: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicUsize>,
) {
    stream.set_read_timeout(Some(PAGE_IO_TIMEOUT)).unwrap();
    stream.set_write_timeout(Some(PAGE_IO_TIMEOUT)).unwrap();
    let mut request = Vec::new();
    let mut byte = [0];
    while request.len() < PAGE_HEADER_BYTES {
        if stream.read(&mut byte).unwrap_or(0) == 0 {
            return;
        }
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            headers.lock().unwrap().push(request);
            received.notify_one();
            if reply.hold {
                if stream.read(&mut byte).is_ok_and(|size| size == 0) {
                    closed.fetch_add(1, Ordering::SeqCst);
                }
                return;
            }
            let head = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
                reply.status, reply.content_type, reply.body.len(), reply.location,
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&reply.body);
            return;
        }
    }
}

async fn fetch_page(content_type: &'static str, body: Vec<u8>) -> (Value, bool) {
    let large = body.len() > 64 * 1024;
    let server = page_upstream(content_type, body);
    let port = server.port;
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let config = json!({"version":1,"workspace_paths":["src"],"write":false,
        "commands":[],"skills":[],"mcp":[],"network":{"allow":["127.0.0.1"]}});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let provider = Arc::new(Proposal {
        call: ToolCall::WebFetch {
            url: format!("http://127.0.0.1:{port}/page"),
        },
        requests: Arc::new(Mutex::new(Vec::new())),
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
    let outcome = run_fetch_page(&engine, &workspace, provider, large).await;
    engine.close().await.unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    let (events, replay) = arany::test_support::goals::replayed(&state_path, outcome.session_id).await;
    assert_eq!(replay.runs[0], outcome.run);
    assert!(!events.is_empty());
    if large {
        read_reopened_web_spill(&state_path, &workspace, &outcome).await;
    }
    assert_eq!(
        server.requests.load(Ordering::SeqCst),
        1,
        "spill reads and replay never fetch"
    );
    (
        serde_json::from_str(&observation.output).unwrap(),
        observation.spill.is_some(),
    )
}

async fn run_fetch_page(
    engine: &arany::Engine,
    workspace: &Path,
    provider: Arc<Proposal>,
    large: bool,
) -> arany::RunOutcome {
    tokio::time::timeout(Duration::from_secs(30), async {
        let controls = RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        };
        if large {
            let provider = Arc::new(super::native_exec::SpillJourney {
                script: arany::test_support::scenario::ScriptedProvider::new(Vec::new()),
                command: provider.call.clone(),
            });
            run_with(engine, &provider, proposal_request(workspace), controls).await
        } else {
            run_with(engine, &provider, proposal_request(workspace), controls).await
        }
    })
    .await
    .expect("web fetch parent deadline")
    .unwrap()
}

async fn read_reopened_web_spill(state: &Path, workspace: &Path, outcome: &arany::RunOutcome) {
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    let expected = "2\tRETAINED_WEB_TEXT\n";
    let read = outcome.run.tools[1].observation.as_ref().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&read.output).unwrap()["text"],
        expected
    );
    assert!(!observation.output.contains("RETAINED_WEB_TEXT"));
    let reference = &observation.spill.as_ref().unwrap().r#ref;
    let engine = arany::test_support::engine::open(
        StateRoot::open_existing(state).unwrap(),
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(vec![
        crate::tool_outcome(ToolCall::Read {
            path: format!("spill:{reference}"),
            offset: Some(2),
            limit: Some(1),
        }),
        ProviderOutcome::Answer(Answer {
            text: "Retained page read".into(),
        }),
    ]));
    let mut request = proposal_request(workspace);
    request.session_id = Some(outcome.session_id);
    let resumed = tokio::time::timeout(
        Duration::from_secs(20),
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
    .expect("reopened spill read deadline")
    .unwrap();
    let read = resumed.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(read.disposition, ToolDisposition::Succeeded);
    assert_eq!(
        serde_json::from_str::<Value>(&read.output).unwrap()["text"],
        expected
    );
    engine.close().await.unwrap();
}

#[tokio::test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Linux web_fetch through the egress proxy: HTML reduction and the download bound"
)]
async fn web_fetch_reads_a_listed_page_and_bounds_a_large_one() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let (page, spilled) = fetch_page(
        "text/html; charset=utf-8",
        b"<html><body><script>evil()</script><h1>Title</h1><p>Hello &amp; welcome</p></body></html>"
            .to_vec(),
    )
    .await;
    assert_eq!(page["status"], 200);
    assert_eq!(page["content_type"], "text/html");
    assert_eq!(page["truncated"], false);
    assert_eq!(page["text"], "Title\n\nHello & welcome");
    assert!(!spilled);

    let mut body = vec![b'a'; 80 * 1024];
    body.extend_from_slice(b"\nRETAINED_WEB_TEXT\n");
    body.resize(3 * 1024 * 1024, b'a');
    let (large, spilled) = fetch_page("text/plain", body).await;
    assert_eq!(large["truncated"], true);
    assert!(spilled);
    let text = large["text"].as_str().unwrap();
    assert!(text.ends_with("[web_fetch truncated]"));
    assert!(text.len() < 64 * 1024);
}
