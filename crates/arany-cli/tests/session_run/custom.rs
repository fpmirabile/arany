use super::*;

#[derive(Clone, Copy)]
struct RouteCase {
    base_path: &'static str,
    model: &'static str,
    auth: Option<&'static str>,
    credential_env: &'static str,
    key: &'static str,
}

const RESPONSES: RouteCase = RouteCase {
    base_path: "",
    model: "model-1",
    auth: None,
    credential_env: "ARANY_PROVIDER_LOCAL_KEY",
    key: "test-key",
};
const MESSAGES_ROUTES: [RouteCase; 4] = [
    RouteCase {
        base_path: "/anthropic",
        model: "kimi-k3",
        auth: Some("bearer"),
        credential_env: "ARANY_PROVIDER_KIMI_API_KEY",
        key: "kimi-api-canary",
    },
    RouteCase {
        base_path: "/coding/",
        model: "k3-256k",
        auth: Some("x-api-key"),
        credential_env: "ARANY_PROVIDER_KIMI_CODE_KEY",
        key: "kimi-code-canary",
    },
    RouteCase {
        base_path: "/api/anthropic",
        model: "glm-5.3",
        auth: Some("bearer"),
        credential_env: "ARANY_PROVIDER_GLM_STANDARD_KEY",
        key: "glm-standard-canary",
    },
    RouteCase {
        base_path: "/api/anthropic",
        model: "glm-5.3-flash",
        auth: Some("bearer"),
        credential_env: "ARANY_PROVIDER_GLM_STANDARD_KEY",
        key: "glm-standard-canary",
    },
];

impl RouteCase {
    fn keys(self, command: &mut Command, selected: Option<&str>) {
        command
            .env("ANTHROPIC_API_KEY", "omitted-native-key")
            .env("ANTHROPIC_WORKSPACE_ID", "OMITTED_NATIVE_SCOPE");
        for route in MESSAGES_ROUTES {
            if route.credential_env != self.credential_env {
                command.env(route.credential_env, route.key);
            }
        }
        if let Some(key) = selected {
            command.env(self.credential_env, key);
        }
    }
}

#[derive(Clone, Copy)]
enum RecheckFailure {
    SchemaRejected,
    SchemaIgnored,
    EffortRejected,
    Unauthorized,
    Redirect,
    WrongModel,
}

#[test]
fn custom_profile_process_gate_proves_conformance_and_durable_runs() {
    custom_profile_process_gate(RESPONSES, RecheckFailure::SchemaRejected);
}

#[test]
fn messages_custom_profile_process_gate_proves_conformance_and_durable_runs() {
    for case in MESSAGES_ROUTES {
        for failure in [
            RecheckFailure::SchemaRejected,
            RecheckFailure::SchemaIgnored,
            RecheckFailure::EffortRejected,
            RecheckFailure::Unauthorized,
            RecheckFailure::Redirect,
            RecheckFailure::WrongModel,
        ] {
            custom_profile_process_gate(case, failure);
        }
        #[cfg(target_os = "linux")]
        messages_attached_case(false, case);
    }
}

fn custom_profile_process_gate(case: RouteCase, failure: RecheckFailure) {
    let messages = case.auth;
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpListener},
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Output, Stdio},
        str::FromStr,
        thread,
        time::{Duration, Instant},
    };

    fn run_product(command: &mut Command) -> Output {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        super::loopback::wait_product(command.spawn().expect("product process"))
    }

    fn run_check(
        case: RouteCase,
        cwd: &Path,
        state: &Path,
        key: Option<&str>,
        proxy: Option<SocketAddr>,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(cwd)
            .args([
                "provider",
                "check",
                if proxy.is_some() {
                    "custom:local"
                } else {
                    "local"
                },
                "--state-dir",
            ])
            .arg(state);
        case.keys(&mut command, key);
        if let Some(proxy) = proxy {
            let proxy = format!("http://{proxy}");
            command
                .env("HTTP_PROXY", &proxy)
                .env("http_proxy", &proxy)
                .env("ALL_PROXY", &proxy)
                .env("NO_PROXY", "");
        }
        run_product(&mut command)
    }

    fn run_exec(
        case: RouteCase,
        cwd: &Path,
        state: &Path,
        key: Option<&str>,
        model: &str,
    ) -> Output {
        run_exec_policy(case, cwd, state, key, model, "single", None)
    }

    fn run_exec_policy(
        case: RouteCase,
        cwd: &Path,
        state: &Path,
        key: Option<&str>,
        model: &str,
        collaboration: &str,
        session_id: Option<SessionId>,
    ) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(cwd)
            .args(["exec", "--state-dir"])
            .arg(state)
            .arg("--workspace")
            .arg(cwd)
            .args([
                "--provider",
                "custom:local",
                "--model",
                model,
                "--collaboration",
                collaboration,
                "answer",
            ]);
        if collaboration == "team" {
            command.args(["--profile", "coding"]);
        }
        case.keys(&mut command, key);
        if let Some(session_id) = session_id {
            command.arg("--session-id").arg(session_id.to_string());
        }
        run_product(&mut command)
    }

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    std::fs::write(workspace.join("private.txt"), b"OMITTED_WORKSPACE_CANARY")
        .expect("Workspace canary");
    std::fs::write(workspace.join("AGENTS.md"), b"EXPECTED_WORKSPACE_GUIDANCE")
        .expect("Workspace guidance");
    let state_path = temp.path().join("state");
    StateRoot::admit(&state_path).expect("state root");
    {
        use std::os::unix::fs::PermissionsExt;
        let user_file = state_path.join("AGENTS.md");
        std::fs::write(&user_file, b"EXPECTED_USER_GUIDANCE").expect("user guidance");
        std::fs::set_permissions(&user_file, std::fs::Permissions::from_mode(0o600))
            .expect("user guidance mode");
    }
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    let port = listener.local_addr().expect("listener address").port();
    let hostile_proxy = TcpListener::bind("127.0.0.1:0").expect("hostile proxy");
    hostile_proxy
        .set_nonblocking(true)
        .expect("nonblocking proxy observation");
    let proxy_addr = hostile_proxy.local_addr().expect("proxy address");
    let mut profile = serde_json::json!({
        "version": 1,
        "profiles": [{
            "name": "local",
            "protocol": "openai-responses",
            "endpoint": format!("http://127.0.0.1:{port}/v1/responses"),
            "model": case.model,
            "credential_env": case.credential_env,
            "outcome_encoding": "json_schema",
            "privacy": "user_authorized",
            "max_output_tokens": 4096,
            "capability_evidence_version": 2, "efforts": []
        }]
    });
    if let Some(auth) = messages {
        profile["version"] = serde_json::json!(2);
        let entry = profile["profiles"][0].as_object_mut().unwrap();
        entry.remove("endpoint");
        entry.insert("protocol".into(), "anthropic-messages".into());
        entry.insert(
            "base_url".into(),
            format!("http://127.0.0.1:{port}{}", case.base_path).into(),
        );
        entry.insert("auth".into(), auth.into());
        entry.insert("outcome_encoding".into(), "native_tools".into());
        if matches!(failure, RecheckFailure::EffortRejected) {
            entry.insert("efforts".into(), serde_json::json!(["low", "high"]));
        }
    }
    let profile_path = state_path.join("provider-profiles.json");
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).expect("profile file");
    std::fs::set_permissions(&profile_path, std::fs::Permissions::from_mode(0o600))
        .expect("private profile file");

    let mutation_path = state_path.join("provider-roster.json");
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).expect("bounded accept");
        for wire_index in 0..if messages.is_some() {
            if matches!(failure, RecheckFailure::EffortRejected) {
                20
            } else if matches!(
                failure,
                RecheckFailure::Unauthorized
                    | RecheckFailure::Redirect
                    | RecheckFailure::WrongModel
            ) {
                10
            } else {
                12
            }
        } else {
            10
        } {
            let effort_case =
                messages.is_some() && matches!(failure, RecheckFailure::EffortRejected);
            let initial_effort = effort_case && (3..7).contains(&wire_index);
            let index = if initial_effort {
                wire_index + 9
            } else if effort_case && wire_index >= 7 {
                wire_index - 4
            } else {
                wire_index
            };
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
            stream
                .set_nonblocking(false)
                .expect("blocking accepted stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request deadline");
            let mut request = Vec::new();
            let (header_end, body_len) = loop {
                let mut chunk = [0; 4096];
                let count = stream.read(&mut chunk).expect("request bytes");
                assert!(count > 0 && request.len() + count <= 64 * 1024);
                request.extend_from_slice(&chunk[..count]);
                if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    let header_end = end + 4;
                    let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
                    let body_len = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .expect("Content-Length");
                    if request.len() >= header_end + body_len {
                        break (header_end, body_len);
                    }
                }
            };
            let header = std::str::from_utf8(&request[..header_end]).expect("HTTP header");
            let path = if messages.is_some() {
                &format!("{}/v1/messages", case.base_path.trim_end_matches('/'))
            } else {
                "/v1/responses"
            };
            assert!(header.starts_with(&format!("POST {path} HTTP/1.1\r\n")));
            let header = header.to_ascii_lowercase();
            assert!(header.contains(concat!("user-agent: arany/", env!("CARGO_PKG_VERSION"))));
            assert!(!header.contains("anthropic-workspace-id:"));
            if messages == Some("x-api-key") {
                assert!(header.contains(&format!("x-api-key: {}\r\n", case.key)));
                assert!(!header.contains("authorization:"));
            } else {
                assert!(header.contains(&format!("authorization: bearer {}\r\n", case.key)));
                assert!(!header.contains("x-api-key:"));
            }
            assert_eq!(
                header.contains("anthropic-version: 2023-06-01\r\n"),
                messages.is_some()
            );
            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end..header_end + body_len])
                    .expect("synthetic JSON");
            assert_eq!(body["model"], case.model);
            let compact = matches!(index, 2 | 11 | 13 | 15);
            if messages.is_some() {
                assert_eq!(body.get("output_config").is_some(), compact || index >= 12);
                assert_eq!(
                    body["output_config"]
                        .get("effort")
                        .and_then(|value| value.as_str()),
                    match index {
                        12 | 13 => Some("low"),
                        14 | 15 => Some("high"),
                        _ => None,
                    }
                );
                assert!(body.get("thinking").is_none());
                if compact {
                    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
                }
            } else {
                assert_eq!(body.get("text").is_some(), compact);
            }
            assert_eq!(
                body[if messages.is_some() {
                    "max_tokens"
                } else {
                    "max_output_tokens"
                }],
                if index == 0 || (3..=9).contains(&index) {
                    4096
                } else {
                    128
                }
            );
            let input: serde_json::Value = crate::semantic::input(&body);
            if (3..9).contains(&index) {
                assert_eq!(
                    input["phase"],
                    match index {
                        5 => "child_work",
                        6 => "root_synthesis",
                        _ => "root_plan",
                    }
                );
                assert_eq!(
                    input["workspace_guidance"],
                    serde_json::json!([{"path": "user AGENTS.md", "text": "EXPECTED_USER_GUIDANCE"}, {"path": "AGENTS.md", "text": "EXPECTED_WORKSPACE_GUIDANCE"}])
                );
                if matches!(index, 3 | 4 | 6 | 8) {
                    assert_eq!(input["objective"], "answer");
                }
                if index == 7 {
                    assert_eq!(
                        input["history"],
                        serde_json::json!([{"user": "answer", "assistant": "verified answer"}])
                    );
                }
            } else {
                assert!(
                    input
                        .get("workspace_guidance")
                        .is_none_or(|value| value.is_null())
                );
            }
            assert!(
                !request
                    .windows(b"OMITTED_WORKSPACE_CANARY".len())
                    .any(|bytes| { bytes == b"OMITTED_WORKSPACE_CANARY" })
            );
            for omitted in MESSAGES_ROUTES
                .map(|route| route.key)
                .into_iter()
                .chain(["omitted-native-key"])
            {
                if omitted != case.key {
                    assert!(
                        !request
                            .windows(omitted.len())
                            .any(|bytes| bytes == omitted.as_bytes()),
                        "unselected key entered request"
                    );
                }
            }
            if index == 9 && messages.is_some() {
                let status = match failure {
                    RecheckFailure::Unauthorized => Some("401 Unauthorized"),
                    RecheckFailure::Redirect => Some("302 Found"),
                    _ => None,
                };
                if let Some(status) = status {
                    write!(stream, "HTTP/1.1 {status}\r\nLocation: http://{proxy_addr}/coding/v1/messages\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                }
            }
            if index == 9 && messages.is_none() {
                write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nLocation: http://{proxy_addr}/v1/responses\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .expect("synthetic redirect");
                continue;
            }
            if (index == 11 && matches!(failure, RecheckFailure::SchemaRejected))
                || (index == 15 && !initial_effort)
            {
                stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                continue;
            }
            if index == 4 {
                std::fs::write(&mutation_path, b"invalid roster after admission").unwrap();
            }
            let text = match index {
                0 | 9 | 12 | 14 => serde_json::json!("ok"),
                1 | 10 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                }
                2 | 11 | 13 | 15 => serde_json::json!(r#"{"summary":"plausible summary"}"#),
                3 => {
                    serde_json::json!("verified answer")
                }
                4 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"one read-only question","profile":null}]}})
                }
                5 => {
                    serde_json::json!("child result")
                }
                6 => {
                    serde_json::json!("team answer")
                }
                7 => {
                    serde_json::json!(case.key)
                }
                8 => {
                    serde_json::json!("roster answer")
                }
                _ => unreachable!(),
            };
            let text = if index == 11 && matches!(failure, RecheckFailure::SchemaIgnored) {
                let nonce =
                    body["output_config"]["format"]["schema"]["properties"]["summary"]["enum"][0]
                        .as_str()
                        .unwrap();
                assert_eq!(nonce.len(), 32);
                let mut without_schema = body.clone();
                without_schema["output_config"]["format"]
                    .as_object_mut()
                    .unwrap()
                    .remove("schema");
                assert!(!without_schema.to_string().contains(nonce));
                text
            } else {
                arany::semantic::compaction_probe_reply(&body, text)
            };
            let mut response = serde_json::json!({
                "id": format!("resp_{}", index + 1),
                "status": "completed",
                "model": case.model,
                "output": arany::semantic::responses_output(&text),
                "usage": {"input_tokens":20,"output_tokens":10}
            });
            if messages.is_some() {
                response = serde_json::json!({"id":format!("msg_{}",index+1),"type":"message","role":"assistant","model":case.model,
                    "stop_reason":if text.is_object() { "tool_use" } else { "end_turn" },
                    "content":arany::semantic::messages_content(&text),"usage":{"input_tokens":20,"output_tokens":10}});
            }
            if index == 9 && matches!(failure, RecheckFailure::WrongModel) {
                response["model"] = "mistyped-highspeed-fallback".into();
            }
            let (content_type, response) = if body["stream"] == true {
                (
                    "text/event-stream",
                    if messages.is_some() {
                        arany::semantic::messages_stream(&response)
                    } else {
                        super::loopback::responses_stream(response)
                    },
                )
            } else {
                ("application/json", response.to_string())
            };
            let response = if index == 7 {
                response.replace(
                    case.key,
                    &format!(
                        "\\u{:04x}{}",
                        u32::from(case.key.as_bytes()[0]),
                        &case.key[1..]
                    ),
                )
            } else {
                response
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .expect("synthetic response");
        }
        listener
    });

    let unverified = run_exec(case, &workspace, &state_path, Some(case.key), case.model);
    assert!(!unverified.status.success());
    assert_eq!(unverified.stdout, b"");
    assert_eq!(
        unverified.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let current_profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile_path).unwrap()).unwrap();
    assert_eq!(current_profile["version"], 2);
    if messages.is_none() {
        assert_eq!(current_profile["profiles"], profile["profiles"]);
        profile = current_profile;
    }
    let checked = run_check(
        case,
        &workspace,
        &state_path,
        Some(case.key),
        Some(proxy_addr),
    );
    assert!(checked.status.success());
    assert_eq!(
        checked.stdout,
        b"Provider: local\nStatus: custom verified\n"
    );
    assert_eq!(checked.stderr, b"");
    assert!(matches!(
        hostile_proxy.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));

    let mut changed_profile = profile.clone();
    changed_profile["profiles"][0][if messages.is_some() {
        "base_url"
    } else {
        "endpoint"
    }] = if messages.is_some() {
        format!("http://127.0.0.1:{port}/other")
    } else {
        format!("http://127.0.0.1:{port}/other/responses")
    }
    .into();
    std::fs::write(&profile_path, serde_json::to_vec(&changed_profile).unwrap())
        .expect("changed profile");
    let changed = run_exec(case, &workspace, &state_path, Some(case.key), case.model);
    assert!(!changed.status.success());
    assert_eq!(changed.stdout, b"");
    assert_eq!(
        changed.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).expect("restore profile");

    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect evidence tuple");
    let (addresses, expiry): (String, i64) = connection
        .query_row(
            "SELECT addresses, expires_at_ms FROM provider_evidence WHERE profile_name='local'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("current evidence tuple");
    connection
        .execute(
            "UPDATE provider_evidence SET addresses=?1 WHERE profile_name='local'",
            [format!("[\"127.0.0.2:{port}\"]")],
        )
        .expect("changed address evidence");
    drop(connection);
    let changed_address = run_exec(case, &workspace, &state_path, Some(case.key), case.model);
    assert!(!changed_address.status.success());
    assert_eq!(changed_address.stdout, b"");
    assert_eq!(
        changed_address.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("restore evidence tuple");
    connection
        .execute(
            "UPDATE provider_evidence SET addresses=?1, expires_at_ms=1 WHERE profile_name='local'",
            [addresses],
        )
        .expect("expired evidence");
    drop(connection);
    let expired = run_exec(case, &workspace, &state_path, Some(case.key), case.model);
    assert!(!expired.status.success());
    assert_eq!(expired.stdout, b"");
    assert_eq!(
        expired.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("restore evidence expiry");
    connection
        .execute(
            "UPDATE provider_evidence SET expires_at_ms=?1 WHERE profile_name='local'",
            [expiry],
        )
        .expect("restore expiry");
    let event_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("pre-Run Event count");
    assert_eq!(event_count, 0);
    drop(connection);

    use sha2::Digest;
    let mut old = sha2::Sha256::new();
    let endpoint = format!("http://127.0.0.1:{port}/v1/responses");
    for field in [
        env!("CARGO_PKG_VERSION"),
        "openai-responses-goal-contract-conformance-effort-stream-v30",
        "local",
        &endpoint,
        case.model,
        "ARANY_PROVIDER_LOCAL_KEY",
        "openai-responses",
        "json_schema",
        "user_authorized",
        "4096",
    ] {
        old.update(u64::try_from(field.len()).unwrap().to_be_bytes());
        old.update(field.as_bytes());
    }
    old.update(2_u32.to_be_bytes());
    let old: [u8; 32] = old.finalize().into();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    let current: Vec<u8> = connection
        .query_row(
            "SELECT profile_digest FROM provider_evidence WHERE profile_name='local'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(current, old);
    connection
        .execute(
            "UPDATE provider_evidence SET profile_digest=?1 WHERE profile_name='local'",
            [old.as_slice()],
        )
        .unwrap();
    drop(connection);
    let stale_without_key = run_exec(case, &workspace, &state_path, None, case.model);
    assert!(!stale_without_key.status.success());
    assert_eq!(stale_without_key.stdout, b"");
    assert_eq!(
        stale_without_key.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE provider_evidence SET profile_digest=?1 WHERE profile_name='local'",
            [current],
        )
        .unwrap();
    drop(connection);

    let wrong_model = run_exec(case, &workspace, &state_path, Some(case.key), "model-2");
    assert!(!wrong_model.status.success());
    assert_eq!(wrong_model.stdout, b"");
    assert_eq!(
        wrong_model.stderr,
        b"error: selected custom Provider model does not match its profile\n"
    );
    let no_key = run_exec(case, &workspace, &state_path, None, case.model);
    assert!(!no_key.status.success());
    assert_eq!(no_key.stdout, b"");
    assert_eq!(
        no_key.stderr,
        b"error: custom Provider credential unavailable\n"
    );

    let success = run_exec(case, &workspace, &state_path, Some(case.key), case.model);
    assert!(success.status.success());
    let continued_session_id = SessionId::from_str(
        std::str::from_utf8(&success.stderr)
            .expect("first receipt UTF-8")
            .lines()
            .next()
            .expect("first Session receipt")
            .strip_prefix("Session: ")
            .expect("first Session ID"),
    )
    .expect("first Session ID format");
    let roster = serde_json::json!({"version":1,"profiles":[
        {"name":"main","routes":[{"provider":"custom:local","model":case.model,"effort":null,"account_id":null}]},
        {"name":"inspect","routes":[{"provider":"custom:local","model":case.model,"effort":null,"account_id":null}]}
    ],"presets":[{"name":"coding","providers":{"primary":"main","child":"inspect","review":null,"compaction":null},"child_profiles":["inspect","main"]}]});
    let roster_path = state_path.join("provider-roster.json");
    std::fs::write(&roster_path, serde_json::to_vec(&roster).unwrap()).unwrap();
    std::fs::set_permissions(&roster_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let team = run_exec_policy(
        case,
        &workspace,
        &state_path,
        Some(case.key),
        case.model,
        "team",
        None,
    );
    let reflected = run_exec_policy(
        case,
        &workspace,
        &state_path,
        Some(case.key),
        case.model,
        "single",
        Some(continued_session_id),
    );
    std::fs::write(&roster_path, serde_json::to_vec(&roster).unwrap()).unwrap();
    let mut jsonl_command = Command::new(env!("CARGO_BIN_EXE_arany"));
    jsonl_command
        .env_clear()
        .env(case.credential_env, case.key)
        .current_dir(&workspace)
        .args([
            "exec",
            "--profile",
            "coding",
            "--output",
            "jsonl",
            "--collaboration",
            "single",
            "--state-dir",
        ])
        .arg(&state_path)
        .arg("answer");
    let roster_jsonl = run_product(&mut jsonl_command);
    assert!(roster_jsonl.status.success());
    let jsonl_lines: Vec<serde_json::Value> = roster_jsonl
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let jsonl_session =
        SessionId::from_str(jsonl_lines[0]["session_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        jsonl_lines
            .iter()
            .find(|line| line["kind"] == "RunStarted")
            .unwrap()["event_version"],
        20
    );
    std::fs::write(&roster_path, b"invalid roster after admission").unwrap();
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect pre-recheck Events");
    let events_before_recheck: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("pre-recheck Event count");
    let evidence_before_recheck: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_evidence", [], |row| {
            row.get(0)
        })
        .expect("pre-recheck evidence count");
    assert_eq!(evidence_before_recheck, 1);
    drop(connection);
    let redirected = run_check(
        case,
        &workspace,
        &state_path,
        Some(case.key),
        Some(proxy_addr),
    );
    let listener = server.join().expect("synthetic server completed");
    assert!(!redirected.status.success());
    assert_eq!(redirected.stdout, b"");
    assert_eq!(
        redirected.stderr,
        b"error: custom Provider conformance failed\n"
    );
    assert!(matches!(
        hostile_proxy.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    let denied_after_recheck = run_exec(case, &workspace, &state_path, Some(case.key), case.model);
    assert!(!denied_after_recheck.status.success());
    assert_eq!(denied_after_recheck.stdout, b"");
    assert_eq!(
        denied_after_recheck.stderr,
        b"error: custom Provider evidence unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect post-recheck Events");
    let events_after_recheck: i64 = connection
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .expect("post-recheck Event count");
    assert_eq!(events_after_recheck, events_before_recheck);
    let remaining: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(remaining, 0);

    drop(connection);
    assert!(success.status.success());
    assert_eq!(success.stdout, b"Answer:\n  verified answer\n");
    let receipt = std::str::from_utf8(&success.stderr).expect("receipt UTF-8");
    let receipt: Vec<_> = receipt.lines().collect();
    assert_eq!(receipt.len(), 4);
    let session_id =
        SessionId::from_str(receipt[0].strip_prefix("Session: ").unwrap()).expect("Session ID");
    let run_id = RunId::from_str(receipt[1].strip_prefix("Run: ").unwrap()).expect("Run ID");
    assert_eq!(receipt[2], "Status: finished");
    assert_eq!(receipt[3], "Provider: custom verified");
    assert_eq!(
        success.stderr,
        format!(
            "Session: {session_id}\nRun: {run_id}\nStatus: finished\nProvider: custom verified\n"
        )
        .as_bytes()
    );
    assert!(team.status.success());
    assert_eq!(team.stdout, b"Answer:\n  team answer\n");
    let manifest = "Roster preset: coding\nRoles: primary=main, child=inspect, review=main, compaction=main\nEligible child profiles: inspect, main\n  inspect[0]: custom:local/model-1 effort=default (exact custom; unknown pricing)\n  main[0]: custom:local/model-1 effort=default (exact custom; unknown pricing)\n".replace("model-1", case.model);
    assert_eq!(
        roster_jsonl.stderr,
        manifest.as_bytes(),
        "successful JSONL keeps the entire manifest on stderr"
    );
    let team_receipt = std::str::from_utf8(&team.stderr)
        .expect("team receipt UTF-8")
        .strip_prefix(&manifest)
        .expect("complete manifest on stderr");
    let team_receipt: Vec<_> = team_receipt.lines().collect();
    assert_eq!(team_receipt.len(), 4);
    let team_session_id = SessionId::from_str(team_receipt[0].strip_prefix("Session: ").unwrap())
        .expect("team Session ID");
    assert_eq!(team_receipt[2], "Status: finished");
    assert_eq!(team_receipt[3], "Provider: custom verified");
    assert!(!reflected.status.success());
    assert_eq!(reflected.stdout, b"");
    let reflected_receipt = std::str::from_utf8(&reflected.stderr).expect("failure receipt UTF-8");
    let reflected_receipt: Vec<_> = reflected_receipt.lines().collect();
    assert_eq!(reflected_receipt.len(), 4);
    let reflected_session_id =
        SessionId::from_str(reflected_receipt[0].strip_prefix("Session: ").unwrap())
            .expect("failed Session ID");
    let reflected_run_id = RunId::from_str(reflected_receipt[1].strip_prefix("Run: ").unwrap())
        .expect("failed Run ID");
    assert_eq!(reflected_session_id, session_id);
    assert_eq!(
        reflected.stderr,
        format!(
            "Session: {reflected_session_id}\nRun: {reflected_run_id}\nStatus: failed\nProvider: custom verified\n"
        )
        .as_bytes()
    );
    let diagnostic_path = state_path.join("development.log");
    if cfg!(debug_assertions) {
        let log = std::fs::read_to_string(&diagnostic_path).expect("automatic development log");
        assert!(log.contains("runtime stage=DiagnosticsEnabled"));
        assert!(log.contains("provider phase=RootPlan disposition=InvalidResponse"));
        assert!(log.contains("run disposition=Failed"));
        for forbidden in [
            case.key,
            MESSAGES_ROUTES[0].key,
            MESSAGES_ROUTES[1].key,
            MESSAGES_ROUTES[2].key,
            "omitted-native-key",
            "OMITTED_WORKSPACE_CANARY",
            "EXPECTED_WORKSPACE_GUIDANCE",
            "EXPECTED_USER_GUIDANCE",
            "verified answer",
            "team answer",
            case.model,
        ] {
            assert!(
                !log.contains(forbidden),
                "sensitive data entered development log"
            );
        }
    } else {
        assert!(
            !diagnostic_path.exists(),
            "optimized process must not create debug log"
        );
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state reopen"))
        .expect("read-only Store");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let events = runtime
        .block_on(store.load_session(session_id))
        .expect("canonical Events");
    let view = SessionView::replay(session_id, &events)
        .expect("replay")
        .expect("Session");
    assert_eq!(view.runs.len(), 2);
    assert_eq!(view.runs[0].id, run_id);
    assert_eq!(view.runs[0].status, RunStatus::Finished);
    assert_eq!(
        view.runs[0].agents[0].provider_calls[0].wire_provenance,
        Some(if messages.is_some() {
            arany::ProviderWireProvenance::MessagesEndTurnStorageUnspecified
        } else {
            arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested
        })
    );
    let config = view.runs[0].config.as_ref().expect("Run config");
    assert_eq!(config.provider, "custom:local");
    assert_eq!(config.model, case.model);
    assert_eq!(config.provider_concurrency, 1);
    let provenance = config
        .custom_profile_provenance
        .as_ref()
        .expect("verified custom provenance");
    assert_eq!(
        provenance.endpoint,
        if messages.is_some() {
            format!(
                "http://127.0.0.1:{port}{}/v1/messages",
                case.base_path.trim_end_matches('/')
            )
        } else {
            format!("http://127.0.0.1:{port}/v1/responses")
        }
    );
    assert_eq!(provenance.capability_evidence_version, 2);
    let team_events = runtime
        .block_on(store.load_session(team_session_id))
        .expect("team Events");
    let team_view = SessionView::replay(team_session_id, &team_events)
        .expect("team replay")
        .expect("team Session");
    assert_eq!(team_view.runs.len(), 1);
    assert_eq!(team_view.runs[0].status, RunStatus::Finished);
    assert_eq!(team_view.runs[0].agents.len(), 2);
    let snapshot = team_view.runs[0]
        .config
        .as_ref()
        .unwrap()
        .roster
        .as_ref()
        .unwrap();
    assert!(snapshot.valid());
    assert_eq!(snapshot.preset, "coding");
    assert_eq!(snapshot.providers.child, "inspect");
    assert_eq!(snapshot.profiles.len(), 2);
    assert_eq!(snapshot.child_profiles, ["inspect", "main"]);
    assert_eq!(
        std::fs::read(state_path.join("provider-roster.json")).unwrap(),
        b"invalid roster after admission"
    );
    assert_eq!(
        team_view.runs[0]
            .agents
            .iter()
            .map(|agent| agent.provider_calls.len())
            .sum::<usize>(),
        3
    );
    assert!(
        team_view.runs[0]
            .agents
            .iter()
            .flat_map(|agent| &agent.provider_calls)
            .all(|call| if messages.is_some() {
                matches!(
                    call.wire_provenance,
                    Some(
                        arany::ProviderWireProvenance::MessagesEndTurnStorageUnspecified
                            | arany::ProviderWireProvenance::MessagesToolUseStorageUnspecified
                    )
                )
            } else {
                call.wire_provenance
                    == Some(arany::ProviderWireProvenance::ResponsesCompletedStoreFalseRequested)
            })
    );
    assert_eq!(
        team_view.runs[0]
            .config
            .as_ref()
            .unwrap()
            .provider_concurrency,
        1
    );
    let jsonl_events = runtime.block_on(store.load_session(jsonl_session)).unwrap();
    let jsonl_view = SessionView::replay(jsonl_session, &jsonl_events)
        .unwrap()
        .unwrap();
    assert_eq!(
        jsonl_view.runs[0].assistant_message.as_deref(),
        Some("roster answer")
    );
    assert_eq!(jsonl_view.runs[0].agents[0].provider_calls.len(), 1);
    assert_eq!(
        roster_jsonl.stdout,
        arany_cli::presentation::render_exec(
            &jsonl_view.runs[0],
            &jsonl_events,
            arany_cli::presentation::Output::Jsonl,
            true
        )
        .as_bytes()
    );
    let reflected_events = runtime
        .block_on(store.load_session(reflected_session_id))
        .expect("failed Run Events");
    let reflected_view = SessionView::replay(reflected_session_id, &reflected_events)
        .expect("failed Run replay")
        .expect("failed Session");
    assert_eq!(reflected_view.runs[1].id, reflected_run_id);
    assert_eq!(reflected_view.runs[1].status, RunStatus::Failed);
    assert!(reflected_view.runs[1].assistant_message.is_none());
    assert_eq!(
        reflected_view.runs[1].agents[0].provider_calls[0].wire_provenance,
        None
    );
    runtime
        .block_on(store.close())
        .expect("close read-only Store");
    std::fs::write(
        &profile_path,
        b"invalid profile during historical inspection",
    )
    .unwrap();
    let journal_before = std::fs::read(state_path.join("events.sqlite3"))
        .expect("journal before read-only inspection");
    for (run_id, events, view) in [
        (run_id, &events, &view),
        (team_view.runs[0].id, &team_events, &team_view),
        (reflected_run_id, &reflected_events, &reflected_view),
        (jsonl_view.runs[0].id, &jsonl_events, &jsonl_view),
    ] {
        let selected: Vec<_> = events
            .iter()
            .filter(|event| event.run_id == Some(run_id))
            .cloned()
            .collect();
        assert!(!selected.is_empty());
        for (format, output) in [
            ("text", arany_cli::presentation::Output::Text),
            ("jsonl", arany_cli::presentation::Output::Jsonl),
        ] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
            command
                .env_clear()
                .current_dir(&workspace)
                .args(["show", "--state-dir"])
                .arg(&state_path)
                .args(["--output", format, &run_id.to_string()]);
            let shown = run_product(&mut command);
            assert!(shown.status.success(), "show rejected valid Run {run_id}");
            assert_eq!(shown.stderr, b"");
            assert_eq!(
                shown.stdout,
                arany_cli::presentation::render_session(view, &selected, output).as_bytes(),
                "show selected different committed facts for Run {run_id}"
            );
        }
    }
    for (selector, error) in [
        (
            RunId::new().to_string(),
            b"error: Session or Run not found\n".as_slice(),
        ),
        (
            "not-an-id".to_owned(),
            b"error: invalid Session or Run ID\n".as_slice(),
        ),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
        command
            .env_clear()
            .current_dir(&workspace)
            .args(["show", "--state-dir"])
            .arg(&state_path)
            .args(["--output", "jsonl", &selector]);
        let rejected = run_product(&mut command);
        assert!(!rejected.status.success());
        assert_eq!(rejected.stdout, b"");
        assert_eq!(rejected.stderr, error);
    }
    assert!(
        std::fs::read(state_path.join("events.sqlite3")).expect("journal after inspection")
            == journal_before,
        "show changed the canonical journal"
    );
    let connection =
        rusqlite::Connection::open(state_path.join("events.sqlite3")).expect("inspect evidence");
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_evidence WHERE profile_name='local'",
            [],
            |row| row.get(0),
        )
        .expect("persisted evidence");
    assert_eq!(count, 0);
    drop(connection);
    let database_bytes = std::fs::read(state_path.join("events.sqlite3")).expect("database bytes");
    for artifact in [
        success.stdout.as_slice(),
        success.stderr.as_slice(),
        team.stdout.as_slice(),
        team.stderr.as_slice(),
        reflected.stdout.as_slice(),
        reflected.stderr.as_slice(),
        database_bytes.as_slice(),
    ] {
        for key in [
            case.key,
            MESSAGES_ROUTES[0].key,
            MESSAGES_ROUTES[1].key,
            MESSAGES_ROUTES[2].key,
            "omitted-native-key",
        ] {
            assert!(
                !artifact
                    .windows(key.len())
                    .any(|part| part == key.as_bytes())
            );
        }
    }

    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "replay must make no HTTP requests"
    );
    std::fs::write(&profile_path, serde_json::to_vec(&profile).unwrap()).unwrap();
    let unavailable = run_check(case, &workspace, &state_path, None, None);
    assert!(!unavailable.status.success());
    assert_eq!(unavailable.stdout, b"");
    assert_eq!(
        unavailable.stderr,
        b"error: custom Provider credential unavailable\n"
    );
    let connection = rusqlite::Connection::open(state_path.join("events.sqlite3"))
        .expect("inspect invalidated evidence");
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM provider_evidence", [], |row| {
            row.get(0)
        })
        .expect("evidence count");
    assert_eq!(count, 0);

    connection
        .execute(
            "UPDATE events SET run_id=?1 WHERE session_id=?2 AND run_id=?3",
            rusqlite::params![
                run_id.to_string(),
                team_session_id.to_string(),
                team_view.runs[0].id.to_string()
            ],
        )
        .expect("inject a Run ID shared by two Sessions");
    drop(connection);
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).expect("state reopen"))
        .expect("ambiguous-owner read-only Store");
    for session in [session_id, team_session_id] {
        let events = runtime
            .block_on(store.load_session(session))
            .expect("each ambiguous-owner Session remains valid");
        let view = SessionView::replay(session, &events)
            .expect("each Session strictly replays")
            .expect("Session exists");
        assert_eq!(view.runs[0].id, run_id);
    }
    runtime
        .block_on(store.close())
        .expect("close ambiguous Store");
    let mut command = Command::new(env!("CARGO_BIN_EXE_arany"));
    command
        .env_clear()
        .current_dir(&workspace)
        .args(["show", "--state-dir"])
        .arg(&state_path)
        .args(["--output", "jsonl", &run_id.to_string()]);
    let ambiguous = run_product(&mut command);
    assert!(!ambiguous.status.success());
    assert_eq!(ambiguous.stdout, b"");
    assert_eq!(
        ambiguous.stderr,
        b"error: Session history unavailable or invalid\n"
    );
    #[cfg(target_os = "linux")]
    {
        let export_state = temp.path().join("export-state");
        let export_id = runtime
            .block_on(create_session(
                StateRoot::admit(&export_state).expect("export State"),
                workspace.clone(),
                Some("Bounded export".into()),
            ))
            .expect("export Session");
        for index in 0..24 {
            runtime
                .block_on(rename_session(
                    StateRoot::open_existing(&export_state).expect("export State"),
                    workspace.clone(),
                    export_id,
                    format!("Export {index:02} {}", "x".repeat(100)),
                ))
                .expect("export title Event");
        }
        let store = Store::open_read_only(StateRoot::open_existing(&export_state).unwrap())
            .expect("closed export Store");
        let events = runtime.block_on(store.load_session(export_id)).unwrap();
        let view = SessionView::replay(export_id, &events)
            .expect("strict export replay")
            .expect("export Session");
        runtime.block_on(store.close()).expect("close export Store");
        let expected = arany_cli::presentation::render_session(
            &view,
            &events,
            arany_cli::presentation::Output::Jsonl,
        );
        assert!(expected.len() < 64 * 1024);
        let (stdout_reader, stdout_writer) = rustix::pipe::pipe().expect("export pipe");
        let capacity =
            rustix::pipe::fcntl_setpipe_size(&stdout_reader, 4096).expect("reduced export pipe");
        assert!(
            expected.len() > capacity,
            "export must exceed pipe capacity"
        );
        let (stderr_reader, stderr_writer) = rustix::pipe::pipe().expect("error pipe");
        rustix::pipe::fcntl_setpipe_size(&stderr_reader, 4096).expect("reduced error pipe");
        let mut child = super::loopback::ChildGuard::new(
            Command::new(env!("CARGO_BIN_EXE_arany"))
                .env_clear()
                .current_dir(&workspace)
                .args(["show", "--output", "jsonl", "--state-dir"])
                .arg(&export_state)
                .arg(export_id.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout_writer))
                .stderr(Stdio::from(stderr_writer))
                .spawn()
                .expect("bounded export process"),
        );
        child.child().stdout = Some(stdout_reader.into());
        child.child().stderr = Some(stderr_reader.into());
        let exported = super::loopback::wait_product(child.take());
        assert!(exported.status.success());
        assert!(exported.stderr.is_empty());
        assert_eq!(exported.stdout, expected.as_bytes());
        let store = Store::open_read_only(StateRoot::open_existing(&export_state).unwrap())
            .expect("reopen export Store");
        assert_eq!(
            runtime.block_on(store.load_session(export_id)).unwrap(),
            events
        );
        runtime
            .block_on(store.close())
            .expect("close export replay");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn custom_effort_profile_pins_selected_effort_after_synthetic_conformance() {
    use super::loopback::{read_request, wait_product, write_profile};
    use std::{
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        process::Stdio,
        str::FromStr,
        thread,
        time::{Duration, Instant},
    };

    let temp = tempfile::tempdir().expect("private test root");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).expect("Workspace");
    let state = temp.path().join("state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback endpoint");
    write_profile(&state, listener.local_addr().unwrap().port());
    let path = state.join("provider-profiles.json");
    let mut profile: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    profile["profiles"][0]["capability_evidence_version"] = serde_json::json!(2);
    profile["profiles"][0]["efforts"] = serde_json::json!(["low", "high"]);
    std::fs::write(&path, serde_json::to_vec(&profile).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        for index in 0..8 {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "request {index} missing");
                        thread::yield_now();
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            let body: serde_json::Value =
                serde_json::from_slice(&read_request(&mut stream)).unwrap();
            assert_eq!(body["model"], "model-1");
            assert_eq!(body.get("text").is_some(), matches!(index, 2 | 4 | 6));
            assert_eq!(
                body.get("reasoning")
                    .map(|value| value["effort"].as_str().unwrap()),
                match index {
                    3 | 4 => Some("low"),
                    5..=7 => Some("high"),
                    _ => None,
                }
            );
            let outcome = match index {
                1 => {
                    serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"read-only question","profile":null}]}})
                }
                2 | 4 | 6 => serde_json::json!(r#"{"summary":"synthetic summary"}"#),
                _ => {
                    serde_json::json!("answer")
                }
            };
            super::loopback::send_response_for(&mut stream, index, outcome, body["stream"] == true);
        }
    });

    let mut check = Command::new(env!("CARGO_BIN_EXE_arany"));
    check
        .env_clear()
        .env("ARANY_PROVIDER_LOCAL_KEY", "test-key")
        .current_dir(&workspace)
        .args(["provider", "check", "local", "--state-dir"])
        .arg(&state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let checked = wait_product(check.spawn().unwrap());
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    let mut rejected = Command::new(env!("CARGO_BIN_EXE_arany"));
    rejected
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
            "--effort",
            "max",
            "answer",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rejected = wait_product(rejected.spawn().unwrap());
    assert!(!rejected.status.success());
    assert_eq!(rejected.stdout, b"");
    assert_eq!(
        rejected.stderr,
        b"error: selected custom Provider effort is not conformed for its profile\n"
    );

    let mut run = Command::new(env!("CARGO_BIN_EXE_arany"));
    run.env_clear()
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
            "--effort",
            "high",
            "answer",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = wait_product(run.spawn().unwrap());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.join().expect("synthetic server");

    assert_eq!(output.stdout, b"Answer:\n  answer\n");
    let receipt = std::str::from_utf8(&output.stderr).unwrap();
    let session_id = SessionId::from_str(
        receipt
            .lines()
            .next()
            .unwrap()
            .strip_prefix("Session: ")
            .unwrap(),
    )
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        let events = store.load_session(session_id).await.unwrap();
        let view = SessionView::replay(session_id, &events).unwrap().unwrap();
        assert_eq!(view.runs.len(), 1);
        assert_eq!(view.runs[0].status, RunStatus::Finished);
        let config = view.runs[0].config.as_ref().unwrap();
        assert_eq!(config.effort, Some(arany::Effort::High));
        assert_eq!(
            config
                .custom_profile_provenance
                .as_ref()
                .unwrap()
                .capability_evidence_version,
            2
        );
        store.close().await.unwrap();
    });
}

#[cfg(target_os = "linux")]
#[test]
fn messages_preview_cancellation_discards_secret_tail() {
    messages_attached_case(
        true,
        RouteCase {
            base_path: "/anthropic",
            auth: Some("x-api-key"),
            ..RESPONSES
        },
    );
}

// An answer row alone is not settled: the agents panel and pending rows show the same text
// while the Run is active, and `ready` can be drawn before the answer row is printed.
#[cfg(target_os = "linux")]
#[track_caller]
fn wait_settled(
    output: &mut impl std::io::Read,
    input: &mut impl std::io::Write,
    transcript: &mut Vec<u8>,
    answered: &mut super::session_picker::PtyResponses,
    from: usize,
    answer: &str,
) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        super::session_picker::pump(output, input, transcript, answered);
        let rows = super::screen_rows(transcript, 100, 30);
        if transcript.len() > from
            && rows.iter().any(|row| row.trim() == answer)
            && rows.iter().any(|row| row.trim_start().starts_with("ready"))
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{answer} never settled: {}",
            super::session_picker::tail(transcript)
        );
        std::thread::yield_now();
    }
}

#[cfg(target_os = "linux")]
fn messages_attached_case(cancel: bool, case: RouteCase) {
    use super::active_terminal::{ProductGuard, product_child_of};
    use super::loopback::{ChildGuard, read_request_for, send_messages_response_for, wait_product};
    use super::session_picker::{PtyResponses, pump, wait_for_screen};
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    use std::{
        io::{Read, Write},
        net::TcpListener,
        os::unix::fs::PermissionsExt,
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("private.txt"), b"OMITTED_WORKSPACE_CANARY").unwrap();
    let state = temp.path().join("state");
    StateRoot::admit(&state).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let path = state.join("provider-profiles.json");
    std::fs::write(&path, serde_json::to_vec(&serde_json::json!({"version":2,"profiles":[{
        "name":"local","protocol":"anthropic-messages","base_url":format!("http://127.0.0.1:{port}{}", case.base_path),
        "auth":case.auth.unwrap(),"model":case.model,"credential_env":case.credential_env,
        "outcome_encoding":"native_tools","privacy":"user_authorized","max_output_tokens":4096,"capability_evidence_version":2,"efforts":[]
    }]})).unwrap()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let peer = thread::spawn(move || {
        for index in 0..if cancel { 4 } else { 8 } {
            let mut stream = crate::process::accept_with_deadline(
                &listener,
                Duration::from_secs(10),
                "Messages attached request",
            );
            let body: serde_json::Value = serde_json::from_slice(&read_request_for(
                &mut stream,
                &format!("{}/v1/messages", case.base_path.trim_end_matches('/')),
                &format!(
                    "{}{}",
                    if case.auth == Some("x-api-key") {
                        "x-api-key: "
                    } else {
                        "authorization: bearer "
                    },
                    case.key
                ),
            ))
            .unwrap();
            assert_eq!(body["model"], case.model);
            assert_eq!(
                body.get("output_config").is_some(),
                index == 2 || index == 7
            );
            assert!(body.get("thinking").is_none());
            if index == 3 && cancel {
                assert_eq!(body["stream"], true);
                let events = [
                    serde_json::json!({"type":"message_start","message":{"id":"msg_cancel","type":"message","role":"assistant","model":case.model,"content":[],"stop_reason":null,"usage":{"input_tokens":20,"output_tokens":0}}}),
                    serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"safe preview test-"}}),
                ];
                let prefix: String = events
                    .iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap()
                        )
                    })
                    .collect();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:X}\r\n{prefix}\r\n",prefix.len()).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                assert_eq!(
                    stream.read(&mut [0; 1]).unwrap(),
                    0,
                    "cancellation drops the held stream"
                );
            } else {
                let text = match index {
                    1 | 4 => {
                        serde_json::json!({"outcome":{"type":"delegate","children":[{"objective":"inspect","profile":null}]}})
                    }
                    2 | 7 => serde_json::json!(r#"{"summary":"strict saved summary"}"#),
                    5 => serde_json::json!("child result"),
                    6 => serde_json::json!("team answer"),
                    _ => serde_json::json!("verified answer"),
                };
                send_messages_response_for(
                    &mut stream,
                    index,
                    case.model,
                    text,
                    body["stream"] == true,
                );
            }
        }
        listener
    });
    let mut check = Command::new(env!("CARGO_BIN_EXE_arany"));
    check
        .env_clear()
        .current_dir(&workspace)
        .args(["provider", "check", "local", "--state-dir"])
        .arg(&state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    case.keys(&mut check, Some(case.key));
    let checked = wait_product(check.spawn().unwrap());
    assert!(checked.status.success());
    assert_eq!(
        checked.stdout,
        b"Provider: local\nStatus: custom verified\n"
    );
    assert!(checked.stderr.is_empty());
    let mut command = crate::isolation::account_isolated_script(temp.path());
    command.env_clear().env("ARANY_TEST_EXE",env!("CARGO_BIN_EXE_arany"))
        .env("ARANY_TEST_STATE",&state).env("ARANY_TEST_WORKSPACE",&workspace)
        .env(case.credential_env,case.key).env("ARANY_TEST_MODEL",case.model).env("TERM","xterm-256color")
        .current_dir(&workspace).args(["-q","-e","-c",
            "trap ':' INT; stty rows 30 cols 100; printf 'ARANY_TEST_SHELL_PID:%s\n' \"$$\"; \"$ARANY_TEST_EXE\" --classic --state-dir \"$ARANY_TEST_STATE\" --workspace \"$ARANY_TEST_WORKSPACE\" --provider custom:local --model \"$ARANY_TEST_MODEL\" answer",
            "/dev/null"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    case.keys(&mut command, Some(case.key));
    let mut child = ChildGuard::new(command.spawn().unwrap());
    let mut input = child.child().stdin.take().unwrap();
    let mut output = child.child().stdout.take().unwrap();
    let flags = fcntl_getfl(&output).unwrap();
    fcntl_setfl(&output, flags | OFlags::NONBLOCK).unwrap();
    let mut transcript = Vec::new();
    let mut answered = PtyResponses::default();
    let deadline = Instant::now() + Duration::from_secs(10);
    let shell_pid = loop {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        if let Some(pid) = String::from_utf8_lossy(&transcript)
            .lines()
            .find_map(|line| {
                line.trim_end_matches('\r')
                    .strip_prefix("ARANY_TEST_SHELL_PID:")
                    .and_then(|pid| pid.parse::<u32>().ok())
            })
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "attached shell PID deadline");
        thread::yield_now();
    };
    let _product = ProductGuard::new(product_child_of(shell_pid), state.clone());
    if cancel {
        wait_for_screen(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            (0, 100, 30),
            "safe preview",
        );
    } else {
        wait_settled(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            0,
            "verified answer",
        );
        let from = transcript.len();
        input.write_all(b"delegate once\r").unwrap();
        wait_settled(
            &mut output,
            &mut input,
            &mut transcript,
            &mut answered,
            from,
            "team answer",
        );
    }
    let from = transcript.len();
    input
        .write_all(if cancel { b"\x03" } else { b"/compact\r" })
        .unwrap();
    wait_for_screen(
        &mut output,
        &mut input,
        &mut transcript,
        &mut answered,
        (from, 100, 30),
        if cancel {
            "Goal paused"
        } else {
            "Compaction saved"
        },
    );
    input.write_all(b"/exit\r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.child().try_wait().unwrap().is_none() {
        pump(&mut output, &mut input, &mut transcript, &mut answered);
        assert!(Instant::now() < deadline, "attached exit deadline");
        thread::yield_now();
    }
    pump(&mut output, &mut input, &mut transcript, &mut answered);
    fcntl_setfl(&output, flags).unwrap();
    child.child().stdout = Some(output);
    let exited = wait_product(child.take());
    drop(input);
    transcript.extend_from_slice(&exited.stdout);
    assert!(
        exited.status.success(),
        "attached process exit {:?}",
        exited.status.code()
    );
    assert_eq!(exited.stderr, b"");
    let listener = peer.join().unwrap();
    assert!(
        listener
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    );
    for secret in [case.key.as_bytes(), b"test-", b"OMITTED_WORKSPACE_CANARY"] {
        assert!(
            !transcript.windows(secret.len()).any(|part| part == secret),
            "sensitive tail reached the terminal"
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let sessions = list_sessions(StateRoot::open_existing(&state).unwrap(), workspace.clone())
            .await
            .unwrap();
        let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = sessions[0].id;
        let events = store.load_session(session).await.unwrap();
        let view = SessionView::replay(session, &events).unwrap().unwrap();
        assert_eq!(view.runs.len(), if cancel { 1 } else { 2 });
        assert_eq!(
            view.runs[0].status,
            if cancel {
                RunStatus::Cancelled
            } else {
                RunStatus::Finished
            }
        );
        assert_eq!(view.runs[0].agents[0].provider_calls.len(), 1);
        if cancel {
            assert!(view.runs[0].assistant_message.is_none());
            assert_eq!(view.runs[0].agents[0].provider_calls[0].input_tokens, None);
        } else {
            assert_eq!(view.runs[1].status, RunStatus::Finished);
            assert_eq!(view.runs[1].agents.len(), 2);
            assert_eq!(
                view.runs[1]
                    .agents
                    .iter()
                    .map(|agent| agent.provider_calls.len())
                    .sum::<usize>(),
                3
            );
            assert_eq!(view.compactions.len(), 1);
            assert!(matches!(
                view.compactions[0].record.status,
                arany::CompactionStatus::Succeeded { .. }
            ));
        }
        store.close().await.unwrap();
        let database = std::fs::read(state.join("events.sqlite3")).unwrap();
        assert!(
            !database
                .windows(case.key.len())
                .any(|part| part == case.key.as_bytes())
        );
        std::fs::write(path, b"invalid historical config").unwrap();
        let mut show = Command::new(env!("CARGO_BIN_EXE_arany"));
        show.env_clear()
            .current_dir(&workspace)
            .args(["show", "--state-dir"])
            .arg(&state)
            .arg(session.to_string())
            .args(["--output", "jsonl"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let shown = wait_product(show.spawn().unwrap());
        assert!(shown.status.success());
        assert_eq!(shown.stderr, b"");
        assert_eq!(
            shown.stdout,
            arany_cli::presentation::render_session(
                &view,
                &events,
                arany_cli::presentation::Output::Jsonl
            )
            .as_bytes()
        );
        assert!(
            listener
                .accept()
                .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
        );
    });
}
