#[test]
#[cfg_attr(
    not(arany_ci),
    ignore = "native macOS researcher journey; release requires CI's disposable account"
)]
fn researcher_tools_survive_closed_replay_without_widening_authority() {
    #[cfg(debug_assertions)]
    crate::tools::native_journey("tools_researcher::journey::native_researcher_journey_helper");
    #[cfg(all(not(debug_assertions), arany_ci))]
    journey::release_researcher_journey();
    #[cfg(all(not(debug_assertions), not(arany_ci)))]
    panic!("release researcher journey requires CI's disposable account");
}

#[cfg(any(debug_assertions, arany_ci))]
mod journey {
    use crate::engine_support::run_with;
    use crate::{hash, private_file};
    use arany::{
        AgentPhase, Answer, Attestation, ChildGrant, CollaborationPolicy, CompactionRequest,
        CompactionResponse, Event, Provider, ProviderError, ProviderOutcome, ProviderRequest,
        ProviderResponse, RunRequest, RunStatus, SessionView, StateRoot, Store, ToolCall,
        ToolDisposition, ToolResourceProfile,
    };
    use arany::{EngineConfig, RunControls, ToolGrant};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const FACTS: &str = "native researcher facts\n";
    const SKILL: &str =
        "---\nname: synthetic\ndescription: Synthetic research guidance\n---\nUse typed reads.\n";
    const RESOURCE: &str = "Pinned researcher resource.\n";
    const MCP_SCRIPT: &str = r#"set -eu
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"research-fixture","version":"1"}}}' ;;
        *'"method":"notifications/initialized"'*) ;;
        *'"method":"tools/list"'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}]}}' ;;
        *'"method":"tools/call"'*) printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"forbidden call dispatched"}]}}' ;;
        *) exit 1 ;;
    esac
done
"#;

    struct ResearchJourney {
        requests: Arc<Mutex<Vec<ProviderRequest>>>,
        outcomes: Vec<ProviderOutcome>,
    }

    #[async_trait::async_trait]
    impl Provider for ResearchJourney {
        fn identity(&self) -> &arany::ProviderIdentity {
            static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
                std::sync::LazyLock::new(|| {
                    arany::ProviderIdentity::new(
                        "openai",
                        "tool-model",
                        arany::Provenance::native(),
                    )
                });
            &IDENTITY
        }

        async fn invoke(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            let mut requests = self.requests.lock().unwrap();
            let step = requests.len();
            requests.push(request);
            Ok(ProviderResponse {
                outcome: self
                    .outcomes
                    .get(step)
                    .cloned()
                    .ok_or(ProviderError::InvalidOutcome)?,
                turn: arany::ProviderTurn::scripted(),
                response_id: None,
                usage: arany::Usage {
                    input_tokens: Some(1),
                    output_tokens: Some(1),
                    ..Default::default()
                },
                wire_provenance: None,
            })
        }

        async fn compact(
            &self,
            _request: CompactionRequest,
        ) -> Result<CompactionResponse, ProviderError> {
            Err(ProviderError::InvalidOutcome)
        }
    }

    #[cfg(all(not(debug_assertions), arany_ci))]
    pub(super) fn release_researcher_journey() {
        crate::tools_git::release_native_journey(
            "tools_researcher::journey::native_researcher_journey_helper",
            "native researcher journey",
        );
    }

    #[tokio::test]
    #[ignore = "isolated helper owned by native researcher journey"]
    async fn native_researcher_journey_helper() {
        let root = std::env::var_os("ARANY_TEST_FILE_JOURNEY_ROOT").expect("private fixture root");
        let root = Path::new(&root);
        #[cfg(debug_assertions)]
        assert_eq!(StateRoot::account_path().unwrap(), root.join("account"));
        #[cfg(not(debug_assertions))]
        assert!(std::env::var_os("ARANY_TEST_ACCOUNT_ROOT").is_none());
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        private_file(&workspace.join("src/facts.txt"), FACTS.as_bytes());
        let skill = workspace.join(".agents/skills/synthetic");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        private_file(&skill.join("SKILL.md"), SKILL.as_bytes());
        private_file(&skill.join("references/note.txt"), RESOURCE.as_bytes());
        let peer = root.join("mcp.sh");
        private_file(&peer, MCP_SCRIPT.as_bytes());
        let shell_digest = hash(&std::fs::read("/bin/sh").unwrap());
        let git = std::fs::canonicalize("/Library/Developer/CommandLineTools/usr/bin/git").unwrap();
        let config = json!({
            "workspace_paths": ["."], "write": true,
            "commands": [{"name":"shell","executable":"/bin/sh","sha256":shell_digest,"interpreter":true,"inputs":[]}],
            "skills": [],
            "mcp": [{"name":"synthetic","program":{"name":"mcp-shell","executable":"/bin/sh","sha256":shell_digest,"interpreter":true,
                "inputs":[{"path":peer,"destination":"mcp.sh","sha256":hash(MCP_SCRIPT.as_bytes())}]},
                "args":["/inputs/mcp-shell/mcp.sh"],"tools":["echo"]}],
            "git": {"name":"git","executable":git,"sha256":hash(&std::fs::read(&git).unwrap()),"interpreter":false,"inputs":[]}
        });
        let reads = vec![
            ToolCall::List { path: "src".into() },
            ToolCall::Read {
                path: "src/facts.txt".into(),
                offset: None,
                limit: None,
            },
            ToolCall::Search {
                path: "src/facts.txt".into(),
                query: "researcher".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            ToolCall::Skill {
                name: "synthetic".into(),
                resource: None,
            },
            ToolCall::Skill {
                name: "synthetic".into(),
                resource: Some("references/note.txt".into()),
            },
            ToolCall::McpList {
                server: "synthetic".into(),
            },
        ];
        let forbidden = [
            (
                "write",
                ToolCall::Write {
                    path: "src/facts.txt".into(),
                    expected_digest: Some(hash(FACTS.as_bytes())),
                    content: "changed".into(),
                },
            ),
            (
                "command",
                ToolCall::Command {
                    program: "shell".into(),
                    args: vec!["-c".into(), "printf changed > src/facts.txt".into()],
                    cwd: "".into(),
                    resource_class: None,
                    timeout_ms: None,
                },
            ),
            (
                "mcp-call",
                ToolCall::McpCall {
                    server: "synthetic".into(),
                    tool: "echo".into(),
                    schema_digest: hash(&serde_json::to_vec(&json!({
                        "name":"echo",
                        "inputSchema":{"type":"object","properties":{},"additionalProperties":false}
                    })).unwrap()),
                    arguments: "{}".into(),
                },
            ),
            ("git", ToolCall::GitStatus { path: ".".into() }),
        ];
        for (case, calls) in std::iter::once(("reads", reads))
            .chain(forbidden.into_iter().map(|(case, call)| (case, vec![call])))
        {
            #[cfg(debug_assertions)]
            crate::tools::trace_case(root, "journey");
            let state_path = root.join(format!("state-{case}"));
            let state = StateRoot::admit(&state_path).unwrap();
            private_file(
                &state_path.join("tools.json"),
                &serde_json::to_vec(&config).unwrap(),
            );
            let mut outcomes = vec![ProviderOutcome::Delegate(arany::Delegate {
                agent: None,
                children: vec![arany::ChildAssignment {
                    editor: None,
                    objective: "Inspect native facts".into(),
                    profile: None,
                }],
            })];
            outcomes.extend(calls.iter().cloned().map(crate::tool_outcome));
            outcomes.extend(
                ["Native research report", "Synthesized native facts"].map(|result| {
                    ProviderOutcome::Answer(Answer {
                        text: result.into(),
                    })
                }),
            );
            let requests = Arc::new(Mutex::new(Vec::new()));
            let provider = Arc::new(ResearchJourney {
                requests: requests.clone(),
                outcomes,
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
                        title: Some("Native researcher".into()),
                        objective: "Research native facts without mutation".into(),
                        images: Vec::new(),
                        workspace: workspace.clone(),
                        launch_directory: None,
                        include_paths: Vec::new(),
                        policy: CollaborationPolicy::Team {
                            max_active_children: 1,
                        },
                        goal: arany::GoalRequest {
                            budget: Some(arany::GoalBudget {
                                max_provider_calls: Some(32),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    },
                    RunControls {
                        tools: ToolGrant::Configured,
                        ..Default::default()
                    },
                ),
            )
            .await
            .expect("native researcher deadline")
            .unwrap();
            engine.close().await.unwrap();
            let store =
                Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
            let events = store.load_session(outcome.session_id).await.unwrap();
            store.close().await.unwrap();
            let replay = SessionView::replay(outcome.session_id, &events)
                .unwrap()
                .unwrap();
            assert_eq!(
                replay.runs.as_slice(),
                std::slice::from_ref(&outcome.run),
                "closed replay: {case}"
            );
            assert_eq!(replay.runs[0].agents.len(), 2, "{case}");
            let child = &replay.runs[0].agents[1];
            assert_eq!(
                child.grant,
                Some(ChildGrant::ReadOnly {
                    unguarded: arany::UnguardedGrant::Denied
                })
            );
            assert!(
                outcome.run.tools.is_empty(),
                "child facts stay out of primary tools: {case}"
            );
            let requests = requests.lock().unwrap().clone();
            assert_eq!(requests[0].phase, AgentPhase::RootPlan);
            assert_eq!(requests[1].phase, AgentPhase::ChildWork);
            assert_eq!(requests[1].collaboration, CollaborationPolicy::Single);
            let primary: Value =
                serde_json::from_str(requests[0].input.tools.as_ref().unwrap().catalog()).unwrap();
            assert_eq!(primary["write"], true);
            assert_eq!(primary["commands"][0]["name"], "shell");
            assert_eq!(
                primary["git"]["operations"],
                json!([
                    "git_status",
                    "git_diff",
                    "git_log",
                    "git_commit",
                    "git_branch",
                    "git_switch"
                ])
            );
            let catalog: Value =
                serde_json::from_str(requests[1].input.tools.as_ref().unwrap().catalog()).unwrap();
            assert_eq!(catalog["write"], false);
            assert_eq!(catalog["commands"], json!([]));
            assert!(catalog.get("git").is_none());
            assert_eq!(catalog["file_tools"], true);
            assert_eq!(catalog["skills"][0]["name"], "synthetic");
            assert_eq!(
                catalog["mcp_servers"],
                json!([{"name":"synthetic","tools":["echo"],"protocol":"2025-11-25 stdio; catalog listing with mcp_list only"}])
            );
            let started = events
                .iter()
                .filter(|event| {
                    matches!(&event.event,
                Event::ToolStarted { agent_run_id, .. } if *agent_run_id == child.id)
                })
                .count();
            let finished = events
                .iter()
                .filter(|event| {
                    matches!(&event.event,
                Event::ToolFinished { agent_run_id, .. } if *agent_run_id == child.id)
                })
                .count();
            if case == "reads" {
                assert_eq!(outcome.run.status, RunStatus::Finished);
                assert_eq!(replay.goals[0].status, arany::GoalStatus::Completed);
                assert!(replay.goals[0].plan_contract.is_none());
                assert_eq!((started, finished), (calls.len(), calls.len()));
                assert_research_reads(&requests, &calls, &child.tools, &catalog);
            } else {
                assert_eq!(outcome.run.status, RunStatus::Failed, "{case}");
                assert_eq!(replay.goals[0].status, arany::GoalStatus::Failed, "{case}");
                assert_eq!(
                    requests.len(),
                    2,
                    "no retry or synthesis after forbidden {case}"
                );
                assert_eq!((started, finished), (0, 0), "no dispatch: {case}");
                assert!(child.tools.is_empty(), "{case}");
                assert!(events.iter().any(|event| matches!(&event.event,
                    Event::ProviderCallRecorded { agent_run_id, record, .. }
                        if *agent_run_id == child.id && record.disposition == arany::ProviderCallDisposition::InvalidResponse)), "{case}");
                assert!(events.iter().any(|event| matches!(&event.event,
                    Event::AgentFinished { agent_run_id, disposition, .. }
                        if *agent_run_id == child.id && *disposition == arany::AgentDisposition::Failed)), "{case}");
            }
            assert_eq!(
                std::fs::read(workspace.join("src/facts.txt")).unwrap(),
                FACTS.as_bytes(),
                "{case}"
            );
            assert_eq!(
                std::fs::read(skill.join("SKILL.md")).unwrap(),
                SKILL.as_bytes(),
                "{case}"
            );
            assert_eq!(
                std::fs::read(skill.join("references/note.txt")).unwrap(),
                RESOURCE.as_bytes(),
                "{case}"
            );
        }
    }

    fn assert_research_reads(
        requests: &[ProviderRequest],
        calls: &[ToolCall],
        tools: &[arany::ToolView],
        catalog: &Value,
    ) {
        assert_eq!(requests.len(), calls.len() + 3);
        assert_eq!(tools.len(), calls.len());
        let observations: Vec<_> = tools
            .iter()
            .map(|tool| tool.observation.as_ref().unwrap())
            .collect();
        let initial_remaining = requests[1]
            .input
            .tools
            .as_ref()
            .unwrap()
            .calls_remaining()
            .unwrap();
        assert!(initial_remaining >= calls.len());
        for (index, request) in requests[1..=calls.len() + 1].iter().enumerate() {
            assert_eq!(request.phase, AgentPhase::ChildWork);
            assert_eq!(request.collaboration, CollaborationPolicy::Single);
            assert_eq!(request.input.objective, "Inspect native facts");
            assert!(request.input.child_results.is_empty());
            assert!(request.input.history.is_empty());
            let context = request.input.tools.as_ref().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(context.catalog()).unwrap(),
                *catalog,
                "catalog frozen at spawn"
            );
            assert_eq!(context.calls_remaining(), Some(initial_remaining - index));
            assert_eq!(context.observations(), observations[..index]);
        }
        let synthesis = requests.last().unwrap();
        assert_eq!(synthesis.phase, AgentPhase::RootSynthesis);
        assert_eq!(synthesis.input.child_results.len(), 1);
        assert_eq!(
            synthesis.input.child_results[0].result,
            "Native research report"
        );
        assert!(
            synthesis
                .input
                .tools
                .as_ref()
                .unwrap()
                .observations()
                .is_empty()
        );
        for (index, observation) in observations.iter().enumerate() {
            assert_eq!(observation.intent.call, calls[index]);
            assert_eq!(
                observation.disposition,
                ToolDisposition::Succeeded,
                "{index}: {}",
                observation.output
            );
            assert_eq!(
                observation.intent.limits.resource_profile,
                ToolResourceProfile::MacosSupervised
            );
            match observation.attestation.as_ref().unwrap() {
                Attestation::InProcess {
                    receipt: _,
                    policy_digest,
                    workspace_device,
                    workspace_inode,
                } => {
                    assert!(
                        matches!(
                            calls[index],
                            ToolCall::Read { .. }
                                | ToolCall::List { .. }
                                | ToolCall::Glob { .. }
                                | ToolCall::Search { .. }
                                | ToolCall::Skill { .. }
                        ),
                        "file reads, discovery and Skill reads require in-process attestation"
                    );
                    assert_eq!(*policy_digest, observation.intent.policy_digest);
                    assert_eq!(
                        (*workspace_device, *workspace_inode),
                        (
                            observation.intent.workspace_device,
                            observation.intent.workspace_inode
                        )
                    );
                }
                Attestation::Guard(receipt) => {
                    assert!(
                        matches!(calls[index], ToolCall::McpList { .. }),
                        "MCP catalog listing requires a Guard receipt"
                    );
                    assert_eq!(receipt.contract_version, 1);
                    let digest: [u8; 32] =
                        Sha256::digest(serde_json::to_vec(&observation.intent).unwrap()).into();
                    assert_eq!(receipt.intent_digest, digest);
                    assert_eq!(
                        receipt.enforcement_digest,
                        observation.intent.enforcement_digest
                    );
                    assert_eq!(receipt.limits, observation.intent.limits);
                }
                Attestation::Unguarded(_) => panic!("researchers never run unguarded"),
                Attestation::Host(_) => panic!("researchers never run host Tools"),
            }
        }
        let output =
            |index: usize| serde_json::from_str::<Value>(&observations[index].output).unwrap();
        assert_eq!(output(0)["entries"], json!(["src/facts.txt"]));
        assert_eq!(
            output(1)["text"],
            FACTS
                .lines()
                .enumerate()
                .map(|(i, line)| format!("{}\t{line}\n", i + 1))
                .collect::<String>()
        );
        assert_eq!(output(1)["sha256"], hash(FACTS.as_bytes()));
        assert_eq!(
            output(2)["entries"],
            json!([{"path":"src/facts.txt","line":1,"text":"native researcher facts"}])
        );
        assert_eq!(observations[3].output, SKILL);
        assert_eq!(observations[4].output, RESOURCE);
        assert_eq!(output(5)["tools"][0]["name"], "echo");
    }
}
