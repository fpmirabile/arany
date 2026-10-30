use super::*;

fn mcp_definition() -> Value {
    let schema = json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false});
    json!({"name":"echo","description":"Return a synthetic value; no host I/O","inputSchema":schema,"outputSchema":schema})
}

#[tokio::test]
#[ignore = "native Linux real MCP version/framing/schema/capability failure corpus"]
async fn mcp_stdio_corpus_rejects_unnegotiated_authority_and_hostile_results() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let peer = temp.path().join("mcp.py");
    let peer_body = include_bytes!("../../fixtures/mcp-tools.py");
    private_file(&peer, peer_body);
    let python = std::fs::canonicalize("/usr/bin/python3").unwrap();
    let python_hash = hash(&std::fs::read(&python).unwrap());
    let schema_digest = hash(&serde_json::to_vec(&mcp_definition()).unwrap());
    let rows = [
        ("wrong_version", false, ToolDisposition::Failed),
        ("wrong_id", false, ToolDisposition::Failed),
        ("duplicate_json", false, ToolDisposition::Failed),
        ("oversized_frame", false, ToolDisposition::Limit),
        ("notification_flood", false, ToolDisposition::Limit),
        ("duplicate_tool", false, ToolDisposition::Failed),
        ("schema_ref", false, ToolDisposition::Failed),
        ("schema_pattern", false, ToolDisposition::Failed),
        ("list_changed", false, ToolDisposition::Failed),
        ("schema_drift", true, ToolDisposition::Failed),
        ("bad_arguments", true, ToolDisposition::Failed),
        ("output_schema", true, ToolDisposition::Failed),
        ("embedded_resource", true, ToolDisposition::Failed),
        ("bad_is_error", true, ToolDisposition::Failed),
        ("is_error", true, ToolDisposition::Failed),
        ("resource_link", true, ToolDisposition::Succeeded),
        ("disabled_capabilities", true, ToolDisposition::Succeeded),
    ];
    for (mode, call, expected) in rows {
        let state_path = temp.path().join(format!("state-{mode}"));
        let state = StateRoot::admit(&state_path).unwrap();
        let config = json!({"workspace_paths":["src"],"write":false,"commands":[],"skills":[],"mcp":[{"name":"synthetic","program":{"name":"python","executable":python,"sha256":python_hash,"interpreter":true,"inputs":[{"path":peer,"destination":"mcp.py","sha256":hash(peer_body)}]},"args":["/inputs/python/mcp.py",mode],"tools":["echo"]}]});
        private_file(
            &state_path.join("tools.json"),
            &serde_json::to_vec(&config).unwrap(),
        );
        let proposal = if call {
            ToolCall::McpCall {
                server: "synthetic".into(),
                tool: "echo".into(),
                schema_digest: schema_digest.clone(),
                arguments: if mode == "bad_arguments" {
                    "{\"value\":\"invalid\"}".into()
                } else {
                    "{\"value\":42}".into()
                },
            }
        } else {
            ToolCall::McpList {
                server: "synthetic".into(),
            }
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Proposal {
            call: proposal,
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
        .expect("MCP case parent deadline")
        .unwrap();
        engine.close().await.unwrap();
        assert_eq!(
            outcome.run.status,
            if !call || expected == ToolDisposition::Succeeded {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            },
            "MCP case {mode}"
        );
        let observation = outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(
            observation.disposition, expected,
            "MCP case {mode}: bounded disposition/output {:?}",
            observation.output
        );
        assert_proposal_requests(&requests.lock().unwrap(), 2);
        if mode == "resource_link" {
            let result: Value = serde_json::from_str(&observation.output).unwrap();
            assert_eq!(result["content"][0]["fetch"], "not authorized or performed");
        }
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
        assert_eq!(view.runs[0], outcome.run);
        if !call {
            assert_eq!(view.goals[0].status, arany::GoalStatus::Completed);
            assert!(view.goals[0].plan_contract.is_none());
            assert!(view.goals[0].report.is_some());
        }
        store.close().await.unwrap();
    }
}
