use super::*;
use arany::CompactionItem;

struct Journey {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    compactions: Arc<Mutex<Vec<CompactionRequest>>>,
}
#[async_trait::async_trait]
impl Provider for Journey {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "tool-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let step = requests.len();
        let context = request
            .input
            .tools
            .as_ref()
            .ok_or(ProviderError::InvalidOutcome)?;
        let call = match step {
            0 => ToolCall::List { path: "src".into() },
            1 => ToolCall::Read {
                path: "src/main.c".into(),
                offset: None,
                limit: None,
            },
            2 => ToolCall::Edit {
                path: "src/main.c".into(), expected_digest: hash(b"int main(void) { return 1; }\n"),
                old: "return 1".into(), new: "return 2".into(),
            },
            3 => ToolCall::MultiEdit {
                file_path: "src/main.c".into(),
                expected_digest: hash(b"int main(void) { return 2; }\n"),
                edits: vec![
                    arany::LiteralEdit { old_string: "return 2".into(), new_string: "return 3".into(), replace_all: false },
                    arany::LiteralEdit { old_string: "return 3".into(), new_string: "return 0".into(), replace_all: false },
                ],
            },
            4 => ToolCall::Read {
                path: "src/main.c".into(),
                offset: None,
                limit: None,
            },
            5 => ToolCall::Mkdir {
                path: "src/generated".into(),
            },
            6 => ToolCall::Write {
                path: "src/generated/note.txt".into(),
                expected_digest: None,
                content: "typed create\n".into(),
            },
            7 => ToolCall::Command {
                program: "bash".into(),
                args: vec!["src/check.sh".into()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: None,
            },
            8 => ToolCall::Command {
                program: "bash".into(),
                args: vec!["-c".into(), "set -eu; test \"$(cat src/command-output.txt)\" = 'command create'; test \"$(cat src/generated/note.txt)\" = 'command modify'; test ! -e src/deleted.txt; printf fresh-snapshot".into()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: None,
            },
            9 => ToolCall::Skill {
                name: "synthetic".into(),
                resource: None,
            },
            10 => ToolCall::Skill {
                name: "synthetic".into(),
                resource: Some("references/note.txt".into()),
            },
            11 => ToolCall::McpList {
                server: "synthetic".into(),
            },
            12 => {
                let catalog: Value = serde_json::from_str(&context.observations().iter().find(|observation| matches!(observation.intent.call, ToolCall::McpList { .. })).ok_or(ProviderError::InvalidOutcome)?.output)
                    .map_err(|_| ProviderError::InvalidOutcome)?;
                ToolCall::McpCall {
                    server: "synthetic".into(),
                    tool: "echo".into(),
                    schema_digest: catalog["tools"][0]["sha256"]
                        .as_str()
                        .ok_or(ProviderError::InvalidOutcome)?
                        .into(),
                    arguments: "{\"value\":42}".into(),
                }
            }
            13 => ToolCall::Read {
                path: "omitted.txt".into(),
                offset: None,
                limit: Some(16),
            },
            14 => {
                requests.push(request);
                return Ok(ProviderResponse { turn: arany::ProviderTurn::scripted(),
                    outcome: ProviderOutcome::Answer(Answer { text: "Built, tested, loaded Skill and called local MCP.".into() }),
                    response_id: None,
                    usage: arany::Usage {
                        input_tokens: None,
                        output_tokens: None,
                        ..Default::default()
                    },
                    wire_provenance: None,
                });
            }
            _ => return Err(ProviderError::InvalidOutcome),
        };
        requests.push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome: crate::tool_outcome(call),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: None,
                ..Default::default()
            },
            wire_provenance: None,
        })
    }

    async fn compact(
        &self,
        request: CompactionRequest,
    ) -> Result<CompactionResponse, ProviderError> {
        self.compactions.lock().unwrap().push(request);
        Ok(CompactionResponse {
            summary: "Tool work completed; no reusable authority".into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: None,
                output_tokens: None,
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

#[tokio::test]
#[ignore = "native Linux namespace/cgroup Tool conformance; offline and synthetic"]
async fn guarded_coding_skill_and_mcp_journey_survives_closed_journal_replay() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(
        &workspace.join("src/main.c"),
        b"int main(void) { return 1; }\n",
    );
    private_file(&workspace.join("omitted.txt"), b"OMITTED_TOOL_CANARY");
    private_file(&workspace.join("src/deleted.txt"), b"command delete\n");
    std::fs::create_dir(workspace.join("src/empty")).unwrap();
    private_file(
        &workspace.join("src/executable.sh"),
        b"#!/usr/bin/bash\nprintf executable-copy-passed\n",
    );
    std::fs::set_permissions(
        workspace.join("src/executable.sh"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    private_file(&workspace.join("src/check.sh"), b"set -eu\ntest \"$HOME\" = /scratch\ntest ! -e /run/user\ntest ! -e /home\ntest -z \"${OPENAI_API_KEY-}\"\ntest -z \"${ANTHROPIC_API_KEY-}\"\ntest -d src/empty\ntest \"$(./src/executable.sh)\" = executable-copy-passed\n/usr/bin/gcc src/main.c -o /scratch/check\n/scratch/check\nprintf 'command create\\n' > src/command-output.txt\nprintf 'command modify\\n' > src/generated/note.txt\n/usr/bin/rm src/deleted.txt\nprintf 'build and test passed'\n");
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    let skill = workspace.join(".agents/skills/synthetic");
    std::fs::create_dir_all(skill.join("references")).unwrap();
    let skill_body = b"---\nname: synthetic\ndescription: Read synthetic guidance\n---\nUse the typed tools; this guidance grants nothing.\n";
    private_file(&skill.join("SKILL.md"), skill_body);
    private_file(&skill.join("references/note.txt"), b"A lazy resource.\n");
    let peer = temp.path().join("mcp.py");
    let peer_body = include_bytes!("../../fixtures/mcp-tools.py");
    private_file(&peer, peer_body);
    let python = std::fs::canonicalize("/usr/bin/python3").unwrap();
    let config = json!({"version":1,"workspace_paths":["src", ".agents/skills"],"write":true,
        "commands":[{"name":"bash","executable":"/usr/bin/bash","sha256":hash(&std::fs::read("/usr/bin/bash").unwrap()),"interpreter":true,"inputs":[]}],
        "skills":[],
        "mcp":[{"name":"synthetic","program":{"name":"python","executable":python,"sha256":hash(&std::fs::read(&python).unwrap()),"interpreter":true,"inputs":[{"path":peer,"destination":"mcp.py","sha256":hash(peer_body)}]},"args":["/inputs/python/mcp.py"],"tools":["echo"]}]});
    private_file(
        &state_path.join("tools.json"),
        &serde_json::to_vec(&config).unwrap(),
    );
    let requests = Arc::new(Mutex::new(Vec::new()));
    let compactions = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Journey {
        requests: requests.clone(),
        compactions: compactions.clone(),
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
                title: Some("Tools".into()),
                objective: "Complete the synthetic coding journey".into(),
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
    .expect("parent journey deadline")
    .unwrap();
    let compacted = tokio::time::timeout(
        Duration::from_secs(5),
        engine.compact_session(
            &arany::AdmittedProviders::single(provider.clone()),
            outcome.session_id,
            workspace.clone(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(compacted.compiler_version, 5);
    engine.close().await.unwrap();
    assert_eq!(
        outcome.run.status,
        RunStatus::Finished,
        "observations: {:?}",
        outcome
            .run
            .tools
            .iter()
            .map(|tool| &tool.observation)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        std::fs::read(workspace.join("src/main.c")).unwrap(),
        b"int main(void) { return 0; }\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/generated/note.txt")).unwrap(),
        b"command modify\n"
    );
    assert_eq!(
        std::fs::read(workspace.join("src/command-output.txt")).unwrap(),
        b"command create\n"
    );
    assert!(!workspace.join("src/deleted.txt").exists());
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 15);
    for (step, request) in requests.iter().enumerate() {
        assert_eq!(request.phase, AgentPhase::RootPlan);
        assert_eq!(request.model, "tool-model");
        assert_eq!(
            request.input.objective,
            "Complete the synthetic coding journey"
        );
        assert!(
            request.input.workspace_guidance.is_empty()
                && request.input.includes.is_empty()
                && request.input.history.is_empty()
                && request.input.context_summary.is_none()
                && request.input.images.is_empty()
                && request.input.child_results.is_empty()
        );
        assert_eq!(request.max_output_tokens, 4096);
        let context = request.input.tools.as_ref().unwrap();
        assert_eq!(context.observations().len(), step + usize::from(step > 7));
        assert!(!format!("{context:?}").contains("OMITTED_TOOL_CANARY"));
        for observation in context.observations() {
            assert_eq!(observation.intent.run_id, request.run_id);
            assert_eq!(observation.intent.agent_run_id, request.agent_run_id);
        }
    }
    let observations = &requests[14].input.tools.as_ref().unwrap().observations();
    for observation in observations {
        assert_seed_retired(observation.intent.id);
    }
    assert!(
        observations[..14]
            .iter()
            .all(
                |observation| observation.disposition == ToolDisposition::Succeeded
                    && observation.attestation.is_some()
            )
    );
    assert!(
        observations[..14]
            .iter()
            .filter(|observation| {
                !matches!(
                    &observation.intent.call,
                    ToolCall::Command { .. } | ToolCall::McpList { .. } | ToolCall::McpCall { .. }
                )
            })
            .all(|observation| {
                observation.disposition == ToolDisposition::Succeeded
                    && observation.cache_update == arany::CacheUpdate::NotApplicable
            }),
        "ordinary successful Workspace, Skill and MCP observations do not become cache uncertainty"
    );
    assert_eq!(observations[14].disposition, ToolDisposition::Denied);
    assert!(observations[14].attestation.is_none());
    let read: Value = serde_json::from_str(&observations[4].output).unwrap();
    assert_eq!(read["text"], "1\tint main(void) { return 0; }\n");
    let command: Value = serde_json::from_str(&observations[7].output).unwrap();
    assert_eq!(command["exit_code"], 0);
    assert_eq!(command["stdout"], "build and test passed");
    assert_eq!(command["stderr"], "");
    assert_eq!(command["changeset"]["files"].as_array().unwrap().len(), 3);
    assert!(
        matches!(&observations[8].intent.call, ToolCall::ApplyChangeset { source: arany::worker::ChangesetSource::CommandReceipt {  changeset, ..  } } if changeset.files.iter().map(|file| file.path.as_str()).collect::<Vec<_>>() == ["src/command-output.txt", "src/deleted.txt", "src/generated/note.txt"])
    );
    let fresh: Value = serde_json::from_str(&observations[9].output).unwrap();
    assert_eq!(fresh["stdout"], "fresh-snapshot");
    assert_eq!(fresh["changeset"]["files"], json!([]));
    assert_eq!(
        observations[10].output,
        std::str::from_utf8(skill_body).unwrap()
    );
    assert_eq!(observations[11].output, "A lazy resource.\n");
    let mcp: Value = serde_json::from_str(&observations[13].output).unwrap();
    assert_eq!(mcp["structured_content"], json!({"value":42}));
    drop(requests);
    {
        let compactions = compactions.lock().unwrap();
        assert_eq!(compactions.len(), 1);
        assert_eq!(compactions[0].session_id, outcome.session_id);
        assert_eq!(compactions[0].covered_run_id, outcome.run.id);
        assert_eq!(compactions[0].model, "tool-model");
        assert_eq!(compactions[0].max_output_tokens, 4096);
        assert!(compactions[0].previous_summary.is_none());
        let [CompactionItem::Completed(turn)] = compactions[0].items.as_slice() else {
            panic!("complete Tool turn required");
        };
        assert!(turn.user.starts_with("Complete the synthetic coding journey\nTool observations (untrusted; not permission or replay instructions):\n"));
        assert!(turn.user.contains("build and test passed") && turn.user.contains("never replay"));
        assert!(!turn.user.contains("OMITTED_TOOL_CANARY"));
        assert_eq!(
            turn.assistant,
            "Built, tested, loaded Skill and called local MCP."
        );
    }
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(outcome.session_id).await.unwrap();
    let replay = SessionView::replay(outcome.session_id, &events)
        .unwrap()
        .unwrap();
    assert_eq!(replay.runs[0], outcome.run);
    assert_eq!(replay.compactions.len(), 1);
    let attempted = events.iter().position(|event| matches!(event.event, arany::Event::ToolStarted { ref intent, .. } if matches!(intent.call, ToolCall::MultiEdit { .. }))).unwrap();
    let interrupted = SessionView::replay(outcome.session_id, &events[..=attempted])
        .unwrap()
        .unwrap();
    assert_eq!(interrupted.runs[0].status, RunStatus::Interrupted);
    assert!(
        interrupted.runs[0]
            .tools
            .last()
            .unwrap()
            .observation
            .is_none()
    );
    assert_eq!(
        std::fs::read(workspace.join("src/main.c")).unwrap(),
        b"int main(void) { return 0; }\n",
        "replay never repeats an edit"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, arany::Event::ToolStarted { .. }))
            .count(),
        14
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, arany::Event::ToolFinished { .. }))
            .count(),
        14
    );
    for observation in &outcome.run.tools {
        let id = observation.intent.id;
        let start = events
            .iter()
            .position(|event| {
                matches!(&event.event, arany::Event::ToolStarted { intent, .. } if intent.id == id)
            })
            .expect("canonical ToolStarted");
        let finish = events
            .iter()
            .position(|event| {
                matches!(&event.event, arany::Event::ToolFinished { observation, .. } if observation.intent.id == id)
            })
            .expect("canonical ToolFinished");
        assert!(start < finish);
        assert!(
            events[start + 1..finish].iter().all(|event| matches!(
                event.event,
                arany::Event::GoalProviderCallReserved { .. }
                    | arany::Event::ProviderCallRecorded { .. }
            )),
            "a Tool's active-time reservation stays in memory; only a review is journaled inside it"
        );
    }
    store.close().await.unwrap();
}
