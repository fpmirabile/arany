use super::*;
use crate::process::BoundedOutput;
use arany::{ApprovalMode, RunCancellation, ToolApprovals};
use std::process::{Command, Stdio};

const SEARCH_KEY: &str = "SYNTHETIC_WEB_KEY_4927";
const JOURNEY_TIMEOUT: Duration = Duration::from_secs(40);

struct WebCase {
    calls: Vec<ToolCall>,
    config: Value,
    permissions: Value,
    decisions: Vec<bool>,
    cancel_on_request: Option<Arc<tokio::sync::Notify>>,
}

impl WebCase {
    fn fetch(url: String) -> Self {
        Self {
            calls: vec![ToolCall::WebFetch { url }],
            config: json!({"version":1,"workspace_paths":["src"],"write":false,
                "commands":[],"skills":[],"mcp":[],"network":{"allow":["127.0.0.1"]}}),
            permissions: json!({"permissions":{}}),
            decisions: Vec::new(),
            cancel_on_request: None,
        }
    }

    fn search(port: u16) -> Self {
        let mut case = Self::fetch(String::new());
        case.calls = vec![ToolCall::WebSearch {
            query: "rust & café/?".into(),
        }];
        case.config["web_search"] = json!({
            "endpoint": format!("http://127.0.0.1:{port}/search"),
            "key": "ARANY_WEB_SEARCH_JOURNEY_KEY",
        });
        case
    }
}

fn web_engine(root: &Path, case: &WebCase) -> (arany::Engine, std::path::PathBuf) {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let state_path = root.join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&case.config).unwrap(),
    );
    private_file(
        &state_path.join("permissions.json"),
        &serde_json::to_vec(&case.permissions).unwrap(),
    );
    let engine = arany::test_support::engine::open(
        state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    (engine, workspace)
}

async fn web_case(root: &Path, case: WebCase) -> arany::RunOutcome {
    let _deadline = arany::test_support::engine::deadline("web journey", JOURNEY_TIMEOUT);
    let (engine, workspace) = web_engine(root, &case);
    let mut script = vec![arany::test_support::scenario::plan_outcome(
        proposal_plan_contract(&case.calls[0]).unwrap(),
    )];
    script.extend(case.calls.iter().cloned().map(crate::tool_outcome));
    script.extend([
        crate::tool_outcome(ToolCall::Read {
            path: "src/current.txt".into(),
            offset: None,
            limit: None,
        }),
        ProviderOutcome::Answer(Answer {
            text: "Web result observed".into(),
        }),
    ]);
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(script));
    let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
    let cancellation = RunCancellation::new();
    let cancel = cancellation.clone();
    let control = async {
        for (index, allowed) in case.decisions.iter().enumerate() {
            let pending = inbox.next().await.expect("one prompt per fetch");
            assert!(pending.intent.call == case.calls[index]);
            pending.decide(*allowed);
        }
        if let Some(received) = &case.cancel_on_request {
            received.notified().await;
            cancel.cancel();
        }
    };
    let (outcome, ()) = tokio::time::timeout(JOURNEY_TIMEOUT, async {
        tokio::join!(
            run_with(
                &engine,
                &provider,
                proposal_request(&workspace),
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: (!case.decisions.is_empty()).then_some(approvals),
                    cancellation,
                    ..Default::default()
                }
            ),
            control
        )
    })
    .await
    .expect("web journey parent deadline");
    let outcome = outcome.unwrap();
    assert_eq!(
        outcome.run.status,
        if case.cancel_on_request.is_some() {
            RunStatus::Cancelled
        } else {
            RunStatus::Finished
        }
    );
    if case.cancel_on_request.is_none() {
        assert!(provider.responses.lock().unwrap().is_empty());
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            case.calls.len() + 3
        );
    }
    engine.close().await.unwrap();
    assert_closed_web_evidence(&root.join("state"), &provider, &outcome).await;
    outcome
}

async fn assert_closed_web_evidence(
    state: &Path,
    provider: &arany::test_support::scenario::ScriptedProvider,
    outcome: &arany::RunOutcome,
) {
    let (events, replay) = arany::test_support::goals::replayed(state, outcome.session_id).await;
    assert!(
        replay.runs[0] == outcome.run,
        "closed-store replay preserves the web result"
    );
    let payloads = events
        .iter()
        .map(|event| event.event.payload().unwrap())
        .collect::<String>();
    let requests = format!("{:?}", provider.requests.lock().unwrap());
    let database = std::fs::read(state.join("events.sqlite3")).unwrap();
    for bytes in [
        payloads.as_bytes(),
        requests.as_bytes(),
        database.as_slice(),
    ] {
        assert!(
            !bytes
                .windows(SEARCH_KEY.len())
                .any(|window| window == SEARCH_KEY.as_bytes()),
            "search credential escaped its private task pipe"
        );
    }
    for output in [
        arany_cli::presentation::Output::Text,
        arany_cli::presentation::Output::Jsonl,
    ] {
        let rendered = arany_cli::presentation::render_session(&replay, &events, output);
        assert!(
            !rendered.contains(SEARCH_KEY),
            "search credential reached rendered output"
        );
    }
    for tool in outcome
        .run
        .tools
        .iter()
        .chain(outcome.run.agents.iter().flat_map(|agent| &agent.tools))
    {
        assert_seed_retired(tool.intent.id);
        assert!(
            !tool
                .observation
                .as_ref()
                .unwrap()
                .output
                .contains(SEARCH_KEY),
            "search credential reached an observation"
        );
    }
}

fn first_result(outcome: &arany::RunOutcome) -> &arany::ToolObservation {
    outcome.run.tools[0].observation.as_ref().unwrap()
}

#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native Guard web policy, search privacy and verified HTTPS journeys"
)]
fn web_policy_search_privacy_and_https() {
    let _gate = crate::process::NATIVE_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    web_certificate(root.path());
    let executable = std::env::current_exe().unwrap();
    let output = Command::new(executable)
        .args([
            "--exact",
            "tools::network::web_journeys::web_journey_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("ARANY_WEB_SEARCH_JOURNEY_KEY", SEARCH_KEY)
        .env("ARANY_WEB_JOURNEY_ROOT", root.path())
        .env("SSL_CERT_FILE", root.path().join("ca.pem"))
        .bounded_output_for(Duration::from_secs(360), 128 * 1024)
        .unwrap();
    for bytes in [&output.stdout, &output.stderr] {
        assert!(
            !bytes
                .windows(SEARCH_KEY.len())
                .any(|window| window == SEARCH_KEY.as_bytes())
        );
    }
    assert!(output.status.success(), "web journey helper failed");
    crate::process::assert_single_test_completed(&output.stdout, "web journey helper");
}

#[tokio::test]
#[ignore = "private helper owned by web_policy_search_privacy_and_https"]
async fn web_journey_helper() {
    let root = std::env::var_os("ARANY_WEB_JOURNEY_ROOT").expect("owned web fixture root");
    assert!(std::env::var_os("ARANY_ACCOUNT_ISOLATED_TEST").is_some());
    let root = Path::new(&root);
    researcher_journeys(root).await;
    search_journeys(root).await;
    fetch_policy_journeys(root).await;
    redirect_failure_and_cancellation(root).await;
    verified_https(root).await;
}

async fn search_journeys(root: &Path) {
    for (name, body, success) in [
        (
            "search",
            br#"{"web":{"results":[{"title":"Rust","url":"https://example.com/","description":"Guide"}]}}"#.to_vec(),
            true,
        ),
        (
            "literal",
            format!(r#"{{"web":{{"results":[{{"title":"{SEARCH_KEY}"}}]}}}}"#).into_bytes(),
            false,
        ),
        (
            "escaped",
            format!(
                r#"{{"web":{{"results":[{{"description":"\u0053{}"}}]}}}}"#,
                &SEARCH_KEY[1..]
            )
            .into_bytes(),
            false,
        ),
    ] {
        let server = page_upstream("application/json", body);
        let outcome = web_case(&root.join(name), WebCase::search(server.port)).await;
        let result = first_result(&outcome);
        assert_eq!(
            result.disposition,
            if success {
                ToolDisposition::Succeeded
            } else {
                ToolDisposition::Failed
            }
        );
        let output: Value = serde_json::from_str(&result.output).unwrap();
        if success {
            assert_eq!(
                output["results"],
                json!([{"title":"Rust","url":"https://example.com/","description":"Guide"}])
            );
        } else {
            assert_eq!(
                output,
                json!({"error":"the search backend returned an invalid response"})
            );
        }
        assert_eq!(
            server.requests.load(Ordering::SeqCst),
            1,
            "search and closed replay issue one request total"
        );
        assert_search_headers(&server);
    }
}

fn assert_search_headers(server: &PageServer) {
    let headers = server.headers.lock().unwrap();
    assert_eq!(headers.len(), 1);
    let request = String::from_utf8(headers[0].clone()).unwrap();
    let mut lines = request.lines();
    let target = lines
        .next()
        .unwrap()
        .strip_prefix("GET ")
        .unwrap()
        .strip_suffix(" HTTP/1.1")
        .unwrap();
    let url = url::Url::parse(&format!("http://127.0.0.1{target}")).unwrap();
    assert_eq!(url.path(), "/search");
    assert_eq!(
        url.query_pairs().into_owned().collect::<Vec<_>>(),
        [
            ("q".into(), "rust & café/?".into()),
            ("count".into(), "8".into())
        ]
    );
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim()))
        .collect();
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "accept" && *value == "application/json")
    );
    let tokens: Vec<_> = headers
        .iter()
        .filter(|(name, _)| name == "x-subscription-token")
        .collect();
    assert_eq!(tokens.len(), 1);
    assert!(
        tokens[0].1 == SEARCH_KEY,
        "the backend receives the exact credential"
    );
}

async fn fetch_policy_journeys(root: &Path) {
    use ToolDisposition::{Denied, Succeeded};

    for (name, listed, permissions, decisions, expected) in [
        (
            "domain-allow",
            false,
            json!({"allow":["WebFetch(domain:127.0.0.1)"]}),
            vec![],
            vec![Succeeded],
        ),
        (
            "domain-deny",
            true,
            json!({"allow":["WebFetch"],"deny":["WebFetch(domain:127.0.0.1)"]}),
            vec![],
            vec![Denied],
        ),
        (
            "domain-ask",
            true,
            json!({"allow":["WebFetch"],"ask":["WebFetch(domain:127.0.0.1)"]}),
            vec![false],
            vec![Denied],
        ),
        (
            "once",
            false,
            json!({}),
            vec![true, false],
            vec![Succeeded, Denied],
        ),
        (
            "dont-ask",
            false,
            json!({"defaultMode":"dontAsk"}),
            vec![],
            vec![Denied],
        ),
    ] {
        let server = page_upstream("text/plain", b"policy page".to_vec());
        let mut case = WebCase::fetch(format!("http://127.0.0.1:{}/page", server.port));
        if !listed {
            case.config["network"] = json!({"allow":[]});
        }
        if decisions.len() == 2 {
            case.calls.push(case.calls[0].clone());
        }
        case.permissions = json!({"permissions":permissions});
        case.decisions = decisions;
        let outcome = web_case(&root.join(name), case).await;
        let web: Vec<_> = outcome
            .run
            .tools
            .iter()
            .filter(|tool| matches!(tool.intent.call, ToolCall::WebFetch { .. }))
            .collect();
        assert_eq!(web.len(), expected.len(), "{name}");
        for (tool, disposition) in web.iter().zip(&expected) {
            assert_eq!(
                tool.observation.as_ref().unwrap().disposition,
                *disposition,
                "{name}"
            );
        }
        assert_eq!(
            server.requests.load(Ordering::SeqCst),
            expected
                .iter()
                .filter(|disposition| **disposition == Succeeded)
                .count(),
            "{name}"
        );
    }
    let server = page_upstream("text/plain", b"must never be fetched".to_vec());
    let mut case = WebCase::fetch(format!("http://127.0.0.1:{}/page", server.port));
    case.config["network"] = json!({"allow":["127.0.0.1"],"deny":["127.0.0.1"]});
    case.permissions = json!({"permissions":{"allow":["WebFetch"]}});
    let outcome = web_case(&root.join("network-deny"), case).await;
    assert_eq!(first_result(&outcome).disposition, Denied);
    assert_eq!(server.requests.load(Ordering::SeqCst), 0);
}

async fn redirect_failure_and_cancellation(root: &Path) {
    let forbidden = page_upstream("text/plain", b"redirect target".to_vec());
    let server = page_server(PageReply {
        content_type: "text/plain",
        body: Vec::new(),
        status: "302 Found",
        location: format!("Location: http://127.0.0.1:{}/escaped\r\n", forbidden.port),
        hold: false,
    });
    let outcome = web_case(
        &root.join("redirect"),
        WebCase::fetch(format!("http://127.0.0.1:{}/page", server.port)),
    )
    .await;
    assert_eq!(first_result(&outcome).disposition, ToolDisposition::Failed);
    assert!(
        first_result(&outcome)
            .output
            .contains("redirects are refused")
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    assert_eq!(forbidden.requests.load(Ordering::SeqCst), 0);
    let server = page_server(PageReply {
        content_type: "application/json",
        body: b"backend failure".to_vec(),
        status: "503 Unavailable",
        location: String::new(),
        hold: false,
    });
    let outcome = web_case(&root.join("failure"), WebCase::search(server.port)).await;
    assert_eq!(first_result(&outcome).disposition, ToolDisposition::Failed);
    assert_eq!(
        serde_json::from_str::<Value>(&first_result(&outcome).output).unwrap(),
        json!({"error":"the search backend answered HTTP 503"})
    );
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    cancel_web_request(root).await;
}

async fn cancel_web_request(root: &Path) {
    let server = page_server(PageReply {
        content_type: "text/plain",
        body: Vec::new(),
        status: "200 OK",
        location: String::new(),
        hold: true,
    });
    let mut case = WebCase::fetch(format!("http://127.0.0.1:{}/held", server.port));
    case.cancel_on_request = Some(server.received.clone());
    let outcome = web_case(&root.join("cancel"), case).await;
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert_eq!(
        first_result(&outcome).disposition,
        ToolDisposition::Cancelled
    );
    let closed = server.closed.clone();
    let count = server.requests.clone();
    drop(server);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(
        closed.load(Ordering::SeqCst),
        1,
        "cancellation closes the upstream connection"
    );
}

fn web_certificate(root: &Path) {
    let mut ca = Command::new("/usr/bin/openssl");
    ca.env_clear()
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=web-journey-root",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
            "-keyout",
        ])
        .arg(root.join("ca.key"))
        .arg("-out")
        .arg(root.join("ca.pem"));
    assert!(ca.bounded_output().unwrap().status.success(), "fixture CA");
    let mut leaf = Command::new("/usr/bin/openssl");
    leaf.env_clear()
        .args([
            "req",
            "-new",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-addext",
            "extendedKeyUsage=serverAuth",
            "-keyout",
        ])
        .arg(root.join("server.key"))
        .arg("-out")
        .arg(root.join("server.csr"));
    assert!(
        leaf.bounded_output().unwrap().status.success(),
        "fixture leaf"
    );
    let mut sign = Command::new("/usr/bin/openssl");
    sign.env_clear()
        .args(["x509", "-req", "-in"])
        .arg(root.join("server.csr"))
        .arg("-CA")
        .arg(root.join("ca.pem"))
        .arg("-CAkey")
        .arg(root.join("ca.key"))
        .args([
            "-CAcreateserial",
            "-days",
            "1",
            "-copy_extensions",
            "copy",
            "-out",
        ])
        .arg(root.join("server.pem"));
    assert!(
        sign.bounded_output().unwrap().status.success(),
        "fixture signing"
    );
}

struct TlsPage {
    process: std::process::Child,
    reader: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

impl Drop for TlsPage {
    fn drop(&mut self) {
        let _ = self.process.kill();
        self.process.wait().expect("TLS peer reaped");
        self.reader
            .take()
            .unwrap()
            .join()
            .expect("TLS output reader joined");
    }
}

fn tls_page(root: &Path) -> TlsPage {
    use std::io::BufRead;
    const TLS_LOG_BYTES: u64 = 64 * 1024;
    const TLS_START_TIMEOUT: Duration = Duration::from_secs(10);

    let mut process = Command::new("/usr/bin/openssl")
        .env_clear()
        .args([
            "s_server",
            "-www",
            "-accept",
            "127.0.0.1:0",
            "-naccept",
            "1",
            "-cert",
        ])
        .arg(root.join("server.pem"))
        .arg("-key")
        .arg(root.join("server.key"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = process.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        let mut output = std::io::BufReader::new(stdout.take(TLS_LOG_BYTES));
        let mut line = Vec::new();
        let mut sender = Some(sender);
        while output.read_until(b'\n', &mut line).unwrap_or(0) != 0 {
            if let Some(port) = String::from_utf8_lossy(&line)
                .trim()
                .strip_prefix("ACCEPT 127.0.0.1:")
                .and_then(|value| value.parse::<u16>().ok())
                && let Some(sender) = sender.take()
            {
                let _ = sender.send(port);
            }
            line.clear();
        }
    });
    let mut peer = TlsPage {
        process,
        reader: Some(reader),
        port: 0,
    };
    peer.port = receiver
        .recv_timeout(TLS_START_TIMEOUT)
        .expect("TLS peer ready");
    peer
}

async fn verified_https(root: &Path) {
    let peer = tls_page(root);
    let outcome = web_case(
        &root.join("https"),
        WebCase::fetch(format!("https://127.0.0.1:{}/", peer.port)),
    )
    .await;
    let result = first_result(&outcome);
    assert_eq!(result.disposition, ToolDisposition::Succeeded);
    let page: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(page["status"], 200);
    assert_eq!(page["content_type"], "text/html");
    assert!(page["text"].as_str().unwrap().contains("s_server"));
    drop(peer);
}

async fn researcher_journeys(root: &Path) {
    for (name, enabled, listed, search) in [
        ("child-fetch", true, true, false),
        ("child-search", true, true, true),
        ("child-no-grant", false, true, false),
        ("child-unlisted", true, false, false),
    ] {
        let server = page_upstream("application/json", br#"{"web":{"results":[]}}"#.to_vec());
        let mut case = if search {
            WebCase::search(server.port)
        } else {
            WebCase::fetch(format!("http://127.0.0.1:{}/page", server.port))
        };
        case.config["child_web"] = json!(enabled);
        if !listed {
            case.config["network"] = json!({"allow":[]});
        }
        let outcome = researcher_case(&root.join(name), case, enabled && listed).await;
        assert_eq!(
            server.requests.load(Ordering::SeqCst),
            usize::from(enabled && listed)
        );
        assert_eq!(
            outcome.run.agents[1].tools.len(),
            usize::from(enabled && listed)
        );
    }
}

async fn researcher_case(root: &Path, case: WebCase, allowed: bool) -> arany::RunOutcome {
    let (engine, workspace) = web_engine(root, &case);
    let mut script = vec![
        ProviderOutcome::Delegate(arany::Delegate {
            agent: None,
            children: vec![arany::ChildAssignment {
                editor: None,
                objective: "Research one page".into(),
                profile: None,
            }],
        }),
        crate::tool_outcome(case.calls[0].clone()),
    ];
    if allowed {
        script.extend(
            ["Child report", "Research settled"]
                .map(|text| ProviderOutcome::Answer(Answer { text: text.into() })),
        );
    }
    let provider = Arc::new(arany::test_support::scenario::ScriptedProvider::new(script));
    let mut request = proposal_request(&workspace);
    request.policy = CollaborationPolicy::Auto {
        max_active_children: 1,
    };
    let outcome = tokio::time::timeout(
        JOURNEY_TIMEOUT,
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
    .expect("child web parent deadline")
    .unwrap();
    engine.close().await.unwrap();
    assert_closed_web_evidence(&root.join("state"), &provider, &outcome).await;
    assert_eq!(
        outcome.run.status,
        if allowed {
            RunStatus::Finished
        } else {
            RunStatus::Failed
        }
    );
    assert!(provider.responses.lock().unwrap().is_empty());
    if allowed {
        let observation = outcome.run.agents[1].tools[0].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert_eq!(
            observation.intent.limits.network,
            arany::NetworkGrant::Proxy
        );
    }
    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), if allowed { 4 } else { 2 });
    let child = requests
        .iter()
        .find(|request| request.phase == AgentPhase::ChildWork)
        .unwrap();
    let catalog: Value =
        serde_json::from_str(child.input.tools.as_ref().unwrap().catalog()).unwrap();
    assert_eq!(
        catalog.get("web").is_some(),
        case.config["child_web"] == true
    );
    outcome
}
