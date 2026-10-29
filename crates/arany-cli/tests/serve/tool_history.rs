use super::*;

#[test]
fn tool_items_preserve_journaled_dispositions_and_unfinished_history_is_uncertain() {
    use arany::{CacheUpdate, ToolCall, ToolDisposition, ToolObservation};
    let intent = arany::EffectIntent {
        host: None,
        rewrite: Vec::new(),
        host_command: None,
        target: arany::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: arany::RunId::new(),
        agent_run_id: arany::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: ToolCall::Command {
            program: "check".into(),
            args: vec!["--check".into()],
            cwd: ".".into(),
            resource_class: None,
            timeout_ms: None,
        },
        limits: arany::ToolLimits::default(),
        use_count: 1,
    };
    for (disposition, status) in [
        (ToolDisposition::Succeeded, "completed"),
        (ToolDisposition::Denied, "declined"),
        (ToolDisposition::Failed, "failed"),
        (ToolDisposition::Conflict, "failed"),
        (ToolDisposition::Limit, "failed"),
        (ToolDisposition::Cancelled, "failed"),
        (ToolDisposition::Uncertain, "uncertain"),
    ] {
        let observation = ToolObservation {
            spill: None,
            intent: intent.clone(),
            disposition,
            enforcement_cause: None,
            limit_cause: (disposition == arany::ToolDisposition::Limit)
                .then_some(arany::LimitCause::Resource),
            output: "bounded\noutput".into(),
            attestation: None,
            approval_cause: None,
            guard_stage: None,
            cache_update: CacheUpdate::NotApplicable,
        };
        let item =
            serde_json::to_value(items::tool_item(&intent, Some(&observation), false)).unwrap();
        assert_eq!(item["status"], status);
        assert_eq!(
            item["disposition"],
            serde_json::to_value(disposition).unwrap()
        );
        assert_eq!(item["output"], "bounded\noutput");
    }
    let observation = ToolObservation {
        spill: None,
        intent: intent.clone(),
        disposition: ToolDisposition::Succeeded,
        enforcement_cause: None,
        limit_cause: None,
        output: String::new(),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::Uncertain,
    };
    assert_eq!(
        serde_json::to_value(items::tool_item(&intent, Some(&observation), false)).unwrap()["status"],
        "uncertain"
    );
    assert_eq!(
        serde_json::to_value(items::tool_item(&intent, None, true)).unwrap()["status"],
        "uncertain"
    );
    let mut run = arany::RunView {
        skill: None,
        supervisor: Default::default(),
        workers: Default::default(),
        id: intent.run_id,
        objective: "Inspect".into(),
        images: Vec::new(),
        config: None,
        provider_fallbacks: Vec::new(),
        provider_retries: Vec::new(),
        agents: Vec::new(),
        assistant_message: None,
        status: arany::RunStatus::Interrupted,
        accepted_sequence: 1,
        finished_sequence: None,
        failure: None,
        tools: vec![arany::ToolView {
            checkpoint: None,
            intent: intent.clone(),
            observation: None,
            review_call_index: None,
            active_time_reserved: false,
            narrowing_source: None,
            permission_verdict: None,
            started_sequence: Some(1),
            hooks: Vec::new(),
            hook_sequence: None,
            finished_sequence: None,
        }],
        tool_compaction_summary: None,
        tool_compaction_recent_files: Vec::new(),
        tool_compaction_covered_count: 0,
        tool_results_cleared_count: 0,
        definitions: Default::default(),
        display: Vec::new(),
        steering_notes: Vec::new(),
        lifecycle_hooks: Vec::new(),
        lifecycle_hook_sequence: None,
        provider_sequences: Default::default(),
    };
    let history = serde_json::to_value(items::history_items(&run)).unwrap();
    assert_eq!(history[1]["status"], "uncertain");
    run.tools[0].started_sequence = None;
    assert_eq!(
        serde_json::to_value(items::history_items(&run)).unwrap(),
        json!([history[0]])
    );
    run.tools[0].started_sequence = Some(1);
    for (call, kind) in [
        (
            ToolCall::CommandUnguarded {
                program: "check".into(),
                args: vec!["--check".into()],
                cwd: ".".into(),
                justification: "Inspect the same command after explicit approval".into(),
                prefix_rule: None,
            },
            "toolCall",
        ),
        (
            ToolCall::Write {
                path: "new".into(),
                expected_digest: None,
                content: "data".into(),
            },
            "fileChange",
        ),
        (ToolCall::Mkdir { path: "dir".into() }, "fileChange"),
        (
            ToolCall::McpList {
                server: "local".into(),
            },
            "mcpToolCall",
        ),
        (
            ToolCall::McpCall {
                server: "local".into(),
                tool: "echo".into(),
                schema_digest: "x".into(),
                arguments: "{\"text\":\"x\"}".into(),
            },
            "mcpToolCall",
        ),
        (
            ToolCall::Read {
                path: "file".into(),
                offset: None,
                limit: Some(32),
            },
            "toolCall",
        ),
    ] {
        run.tools[0].intent.call = call;
        let item = serde_json::to_value(items::history_items(&run)).unwrap();
        assert_eq!(item[1]["type"], kind);
        assert_eq!(item[1]["status"], "uncertain");
        if matches!(run.tools[0].intent.call, ToolCall::CommandUnguarded { .. }) {
            assert_eq!(item[1]["call"]["operation"], "command_unguarded");
            assert!(item[1].get("attestation").is_none());
        }
    }
    let canary = "historical protocol hook canary";
    run.tools[0].observation = Some(ToolObservation {
        spill: None,
        intent: run.tools[0].intent.clone(),
        disposition: ToolDisposition::Denied,
        enforcement_cause: None,
        limit_cause: None,
        output: format!("Hook denied this Tool: {canary}"),
        attestation: None,
        approval_cause: None,
        guard_stage: None,
        cache_update: CacheUpdate::NotApplicable,
    });
    run.tools[0].permission_verdict = Some(arany::permissions::PermissionVerdict {
        host: None,
        intent_digest: [1; 32],
        decision: arany::permissions::Decision::Deny,
        mode: None,
        basis: arany::permissions::PermissionSource::Hook,
        source: arany::permissions::PermissionSource::Hook,
        approved: false,
        hook_restriction: Some(arany::permissions::Decision::Deny),
        hooks: 1,
        narrowed: false,
    });
    let history = serde_json::to_value(items::history_items(&run)).unwrap();
    assert_eq!(history[1]["status"], "declined");
    assert_eq!(
        history[1]["output"],
        "hook_denied: no operation was dispatched"
    );
    assert!(!history.to_string().contains(canary));

    assert_admission_history_items(&run);
    run.tools[0].observation = None;
    run.tools[0].permission_verdict = None;

    let mut child_tool = run.tools.pop().unwrap();
    child_tool.intent.id = uuid::Uuid::now_v7();
    run.agents.push(arany::AgentView {
        tool_refusal: None,
        tool_refusal_sequence: None,
        profile: None,
        assignment: None,
        id: child_tool.intent.agent_run_id,
        role: arany::AgentRole::Child,
        ordinal: 1,
        objective: Some("Inspect".into()),
        summary: None,
        result: None,
        provider_calls: Vec::new(),
        steering_notes: Vec::new(),
        grant: None,
        definition: None,
        tools: vec![child_tool],
        status: arany::AgentStatus::Interrupted,
    });
    let history = serde_json::to_value(items::history_items(&run)).unwrap();
    assert_eq!(history[1]["status"], "uncertain");
    assert_eq!(
        history[1]["id"],
        run.agents[0].tools[0].intent.id.to_string()
    );
}

fn assert_admission_history_items(run: &arany::RunView) {
    let mut run = run.clone();
    for tool in arany::test_support::host_admission_fixtures()
        .into_iter()
        .chain(std::iter::once(
            arany::test_support::uncertain_hook_fixture(),
        ))
    {
        let output = tool.observation.as_ref().unwrap().output.clone();
        run.tools = vec![tool];
        let history = serde_json::to_value(items::history_items(&run)).unwrap();
        assert_eq!(history[1]["status"], "declined");
        assert_eq!(history[1]["output"], output);
        let expected = serde_json::to_string(&output).unwrap();
        assert!(arany::test_support::history_tool_block(&run).contains(&expected));
    }
}

#[tokio::test]
async fn thread_resume_lists_turns_and_tool_items_with_journaled_dispositions_including_uncertain()
{
    let _deadline = deadline();
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("facts.txt"), "Fact").unwrap();
    let source = root.path().join("source");
    let source_root = StateRoot::admit(&source).unwrap();
    private_json(
        &source.join("tools.json"),
        json!({"version":1,"workspace_paths":["facts.txt"],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    let engine = Engine::open(
        source_root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(
        ScriptedProvider::new(vec![
            arany::test_support::scenario::tool_outcome(arany::ToolCall::Read {
                path: "facts.txt".into(),
                offset: None,
                limit: Some(32),
            }),
            arany::ProviderOutcome::Answer(arany::Answer {
                text: "Fact".into(),
            }),
        ])
        .with_profile("openai", "gpt-5.4"),
    );
    let mut request = arany::test_support::scenario::request(&workspace, "Inspect");
    request.include_paths.clear();
    let outcome = arany::test_support::engine::run_with(
        &engine,
        &provider,
        request,
        arany::RunControls {
            tools: arany::ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (events, view) = closed_view(&source, outcome.session_id).await;
    assert_eq!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        arany::ToolDisposition::Succeeded
    );
    let interrupted = root.path().join("interrupted");
    let store = Store::open(StateRoot::admit(&interrupted).unwrap()).unwrap();
    for envelope in events {
        let last = matches!(envelope.event, Event::ToolStarted { .. });
        store
            .append(outcome.session_id, envelope.event)
            .await
            .unwrap();
        if last {
            break;
        }
    }
    store.close().await.unwrap();
    for (state, status) in [(source, "completed"), (interrupted, "uncertain")] {
        let threads = threads::Threads {
            provider: Some(scripted()),
            ..Default::default()
        };
        let (input, inbound) = mpsc::channel(INBOUND_CAPACITY);
        let (outbound, mut output) = mpsc::channel(OUTBOUND_CAPACITY);
        let client = async {
            input.send(initialize()).await.unwrap();
            next(&mut output).await;
            input.send(super::request(6,"thread/resume",json!({"threadId":outcome.session_id,"cwd":workspace,"provider":"openai","model":"gpt-5.4"}))).await.unwrap();
            let message = next(&mut output).await;
            assert_eq!(message["result"]["turns"][0]["items"][1]["status"], status);
            if status == "uncertain" {
                assert_eq!(message["result"]["turns"][0]["status"], "interrupted");
                assert!(
                    message["result"]["turns"][0]["items"][1]
                        .get("disposition")
                        .is_none()
                );
            }
            input.send(ClientMessage::Eof).await.unwrap();
        };
        let (result, ()) = tokio::join!(
            server::run_with_threads(super::engine(&state), state, inbound, outbound, threads),
            client
        );
        result.unwrap();
    }
}
