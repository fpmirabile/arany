use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};

const SCRIPT: &str = r#"set -eu
port=${HTTPS_PROXY##*:}
echo "proxy:${HTTPS_PROXY%:*}:${NO_PROXY-unset}"
if (exec 4<>/dev/tcp/127.0.0.1/"$2") 2>/dev/null; then echo direct:reachable; else echo direct:blocked; fi
if (exec 4<>/dev/tcp/127.0.0.1/$((port + 1))) 2>/dev/null; then echo neighbor:reachable; else echo neighbor:blocked; fi
exec 3<>/dev/tcp/127.0.0.1/"$port"
printf 'CONNECT %s HTTP/1.1\r\n\r\n' "$1" >&3
IFS= read -r status <&3
echo "status:${status%$'\r'}"
case "$status" in *" 200 "*)
  IFS= read -r blank <&3
  printf 'GET / HTTP/1.0\r\n\r\n' >&3
  IFS= read -r line <&3
  IFS= read -r line <&3
  IFS= read -r line <&3
  echo "$line"
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
#[ignore = "isolated helper owned by native macOS egress proxy journey"]
async fn native_network_proxy_helper() {
    let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
    let root = Path::new(&root);
    assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/egress.sh"), SCRIPT.as_bytes());
    let peer = root.join("mcp.sh");
    private_file(&peer, MCP_SCRIPT.as_bytes());
    let (upstream, accepted) = fixture_upstream();
    let (direct, direct_accepted) = fixture_upstream();
    let sh_digest = hash(&std::fs::read("/bin/sh").unwrap());
    let bash_digest = hash(&std::fs::read("/bin/bash").unwrap());
    for (target, listed) in [
        (format!("127.0.0.1:{upstream}"), true),
        ("evil.example:443".to_owned(), false),
    ] {
        let state_path = root.join(format!("state-{listed}"));
        let state = StateRoot::admit(&state_path).unwrap();
        let config = json!({
            "workspace_paths":["src"],"write":true,
            "commands":[
                {"name":"shell","executable":"/bin/sh","sha256":sh_digest,"interpreter":true,"inputs":[]},
                {"name":"bash","executable":"/bin/bash","sha256":bash_digest,"interpreter":true,"inputs":[]}
            ],
            "skills":[],
            "mcp":[{"name":"synthetic","program":{"name":"mcp-shell","executable":"/bin/sh","sha256":sh_digest,"interpreter":true,
                "inputs":[{"path":peer,"destination":"mcp.sh","sha256":hash(MCP_SCRIPT.as_bytes())}]},
                "args":["/inputs/mcp-shell/mcp.sh"],"tools":["echo"]}],
            "network":{"allow":["127.0.0.1"]}
        });
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let provider = Arc::new(Journey {
            requests: Arc::new(Mutex::new(Vec::new())),
            calls: vec![ToolCall::Command {
                program: "bash".into(),
                args: vec!["src/egress.sh".into(), target.clone(), direct.to_string()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: None,
            }],
            linked_file: None,
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
            Duration::from_secs(120),
            run_with(
                &engine,
                &provider,
                RunRequest {
                    skill: None,
                    session_id: None,
                    title: None,
                    objective: "Guard egress corpus".into(),
                    images: Vec::new(),
                    workspace: workspace.clone(),
                    launch_directory: None,
                    include_paths: Vec::new(),
                    policy: CollaborationPolicy::Single,
                    goal: arany::GoalRequest::default(),
                },
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
            "{target}: {}",
            observation.output
        );
        let output: serde_json::Value = serde_json::from_str(&observation.output).unwrap();
        let stdout = output["stdout"].as_str().unwrap();
        assert!(
            stdout.starts_with("proxy:http://127.0.0.1:\ndirect:blocked\nneighbor:blocked\n"),
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
        let store = Store::open_read_only(StateRoot::admit(&state_path).unwrap()).unwrap();
        let events = store.load_session(outcome.session_id).await.unwrap();
        let replay = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        assert_eq!(replay.runs.last().unwrap(), &outcome.run);
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            connections,
            "replay is offline"
        );
    }
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(direct_accepted.load(Ordering::SeqCst), 0);
}
