use super::*;
use std::os::unix::fs::PermissionsExt;

fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[tokio::test]
async fn permission_dispatch_invalid_or_unsafe_settings_refuse_before_provider() {
    for case in ["malformed", "unknown", "public", "symlink", "oversized"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let path = state.join("permissions.json");
        match case {
            "malformed" => private_file(&path, b"{"),
            "unknown" => private_file(&path, br#"{"permissions":{"unknown":true}}"#),
            "public" => {
                private_file(&path, b"{}");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            "symlink" => {
                let target = state.join("permissions-target.json");
                private_file(&target, b"{}");
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "oversized" => {
                let mut bytes = vec![b' '; 64 * 1024 - 1];
                bytes.extend_from_slice(b"{}");
                private_file(&path, &bytes);
            }
            _ => unreachable!(),
        }
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_outcome(arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: Some(32),
            }),
            arany::test_support::goals::finish_with("answer"),
        ]));
        let mut input = request(&workspace, "Inspect the task");
        input.include_paths.clear();
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await;
        assert!(
            matches!(
                result,
                Err(EngineError::Tool(arany::ToolError::Configuration))
            ),
            "{case}"
        );
        assert!(provider.requests.lock().unwrap().is_empty(), "{case}");
        engine.close().await.unwrap();
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let started: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN ('RunStarted', 'ToolStarted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(started, 0, "{case}");
    }
}

#[tokio::test]
async fn inactive_host_rules_archive_private_bytes_without_admitting_authority() {
    for (case, admitted) in [
        ("group-readable", false),
        ("symlink", false),
        ("hardlink", false),
        ("oversized", false),
        ("valid", true),
        ("malformed", true),
        ("129-entries", true),
        ("unknown", true),
        ("duplicate", true),
        ("version", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let path = state.join("unguarded-commands.json");
        let entry = serde_json::json!({
            "workspace": {"path":workspace, "device":1, "inode":2},
            "program":"sh", "args":["-c", "cargo test"], "cwd":"",
            "created_unix_ms":1
        });
        let mut record = serde_json::json!({"version":1, "commands":[entry.clone()]});
        match case {
            "unknown" => record["commands"][0]["extra"] = serde_json::json!(true),
            "duplicate" => record["commands"] = serde_json::json!([entry.clone(), entry]),
            "129-entries" => {
                record["commands"] = (0..129)
                    .map(|index| {
                        let mut entry = entry.clone();
                        entry["cwd"] = serde_json::json!(format!("dir{index}"));
                        entry
                    })
                    .collect();
            }
            "version" => record["version"] = serde_json::json!(2),
            _ => (),
        }
        private_file(&path, &serde_json::to_vec(&record).unwrap());
        match case {
            "group-readable" => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            }
            "symlink" => {
                let target = state.join("rules-target.json");
                std::fs::rename(&path, &target).unwrap();
                std::os::unix::fs::symlink(target, &path).unwrap();
            }
            "hardlink" => std::fs::hard_link(&path, state.join("rules-link.json")).unwrap(),
            "oversized" => private_file(&path, &vec![b' '; 64 * 1024 + 1]),
            "malformed" => private_file(&path, b"{"),
            _ => (),
        }
        let original = std::fs::read(&path).unwrap();
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            arany::test_support::goals::finish_with("answer"),
        ]));
        let mut input = request(&workspace, "Inspect the task");
        input.include_paths.clear();
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await;
        if admitted {
            let outcome = result.unwrap();
            assert_eq!(outcome.run.status, RunStatus::Finished, "{case}");
            assert!(outcome.run.tools.is_empty(), "{case}");
            assert_eq!(provider.requests.lock().unwrap().len(), 1, "{case}");
        } else {
            assert!(
                matches!(
                    result,
                    Err(EngineError::Tool(arany::ToolError::Configuration))
                ),
                "{case}"
            );
            assert!(provider.requests.lock().unwrap().is_empty(), "{case}");
        }
        engine.close().await.unwrap();
        let preserved = if admitted {
            state.join("archive/inactive-host-commands/unguarded-commands.json")
        } else {
            path.clone()
        };
        assert_eq!(std::fs::read(preserved).unwrap(), original, "{case}");
        assert_eq!(path.exists(), !admitted, "{case}");
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let started: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN ('RunStarted', 'ToolStarted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(started, i64::from(admitted), "{case}");
    }
}

#[tokio::test]
async fn inactive_host_archive_never_pins_authority_and_recreated_sources_refuse() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
    let path = state.join("unguarded-commands.json");
    let original = br#"{"version":1,"commands":[]}"#;
    let edited = b"malformed inactive rules";
    private_file(&path, original);
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        arany::test_support::goals::finish_with("first answer"),
        arany::test_support::goals::finish_with("second answer"),
    ]));
    let mut input = request(&workspace, "Inspect the task");
    input.include_paths.clear();
    let handle = engine
        .submit(
            AdmittedProviders::single(provider.clone()),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .unwrap();
    private_file(&path, edited);
    let first = handle.await.unwrap().run;
    let mut input = request(&workspace, "Inspect the task again");
    input.include_paths.clear();
    let second = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await;
    assert!(matches!(
        second,
        Err(EngineError::Tool(
            arany::ToolError::InactiveHostArchiveConflict
        ))
    ));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    engine.close().await.unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state).unwrap()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), edited);
    assert_eq!(
        std::fs::read(state.join("archive/inactive-host-commands/unguarded-commands.json"))
            .unwrap(),
        original
    );
    for outcome in [&first] {
        let events = store.load_session(outcome.session_id).await.unwrap();
        let view = SessionView::replay(outcome.session_id, &events)
            .unwrap()
            .unwrap();
        let run = view
            .runs
            .iter()
            .find(|run| run.id == outcome.run.id)
            .unwrap();
        assert_eq!(run, &outcome.run);
        assert_eq!(run.status, RunStatus::Finished);
        let policy = run.config.as_ref().unwrap().tool_policy.as_ref().unwrap();
        assert_eq!(policy.unguarded_rules_digest, None);
        assert!(events.iter().any(|event| matches!(&event.event,
            Event::RunStarted { config, .. }
                if config.tool_policy.as_ref().unwrap().unguarded_rules_digest.is_none()
        )));
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn agent_definition_unsafe_sources_refuse_before_provider() {
    for case in [
        "symlink",
        "hardlink",
        "public-private",
        "oversized",
        "linked-project-root",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        let agents = state.join("agents");
        std::fs::create_dir(&agents).unwrap();
        std::fs::set_permissions(&agents, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = agents.join("reader.md");
        let valid = b"---\nname: reader\ndescription: Inspect\n---\nBody";
        match case {
            "symlink" => std::os::unix::fs::symlink(temp.path().join("outside"), &path).unwrap(),
            "hardlink" => {
                private_file(&temp.path().join("outside"), valid);
                std::fs::hard_link(temp.path().join("outside"), &path).unwrap();
            }
            "public-private" => {
                private_file(&path, valid);
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            "oversized" => private_file(&path, &vec![b' '; 64 * 1024 + 1]),
            "linked-project-root" => {
                std::os::unix::fs::symlink(&agents, workspace.join(".claude")).unwrap()
            }
            _ => unreachable!(),
        }
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            arany::test_support::goals::finish_with("forbidden egress reached a valid response"),
        ]));
        let mut input = request(&workspace, "Inspect");
        input.include_paths.clear();
        let result = run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await;
        assert!(
            matches!(
                result,
                Err(EngineError::Tool(
                    arany::ToolError::Path | arany::ToolError::Limit
                ))
            ),
            "unsafe source rejected: {case}"
        );
        assert!(
            provider.requests.lock().unwrap().is_empty(),
            "no Provider disclosure"
        );
        engine.close().await.unwrap();
        let connection = rusqlite::Connection::open(state.join("events.sqlite3")).unwrap();
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM events WHERE kind IN ('RunStarted', 'ToolStarted')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[tokio::test]
async fn a_read_denied_skill_resource_is_denied_before_any_guard_task() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let skill = workspace.join(".agents/skills/x");
    std::fs::create_dir_all(skill.join("private")).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\ndescription: Guide\n---\n").unwrap();
    std::fs::write(skill.join("private/key"), "DENY_CANARY\n").unwrap();
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(
        &state.join("tools.json"),
        br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    );
    private_file(
        &state.join("permissions.json"),
        br#"{"permissions":{"deny":["Read(./.agents/skills/x/private/**)"]}}"#,
    );
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(arany::ToolCall::Skill {
            name: "x".into(),
            resource: Some("private/key".into()),
        }),
        arany::test_support::goals::finish_with("done"),
    ]));
    let outcome = run_with(
        &engine,
        &provider,
        request(&workspace, "Use the Skill"),
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (events, _) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    let verdict = events
        .iter()
        .find_map(|envelope| match &envelope.event {
            Event::ToolStarted {
                verdict: Some(verdict),
                ..
            } => Some(verdict),
            _ => None,
        })
        .expect("Skill verdict");
    assert_eq!(
        (
            format!("{:?}", verdict.decision),
            format!("{:?}", verdict.basis)
        ),
        ("Deny".to_owned(), "Rule".to_owned())
    );
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
    assert!(observation.attestation.is_none(), "no Guard task ran");
    let requests = provider.requests.lock().unwrap();
    assert!(
        !format!("{:?}", requests.last().unwrap().input).contains("DENY_CANARY"),
        "the resource never reached the model"
    );
}

#[tokio::test]
async fn primary_approval_wait_preserves_identity_and_refuses_withdrawal_or_drift() {
    use arany::permissions::PermissionSource;
    for case in [
        "late_allow",
        "rule_change",
        "decline",
        "decline_provider_error",
        "cancel",
        "pause",
        "disconnected",
        "headless",
        "drift",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let root = StateRoot::admit(&state).unwrap();
        private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
        private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"ask":["Read"]}}"#,
        );
        let engine = engine_support::open(
            root,
            EngineConfig {
                guard_executable: Some(std::env::current_exe().unwrap()),
                ..Default::default()
            },
        )
        .unwrap();
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_outcome(arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: Some(32),
            }),
            arany::test_support::goals::finish_with("done"),
        ]));
        if case == "decline_provider_error" {
            provider.responses.lock().unwrap()[1] = Err(ProviderError::LocalOutputLimit);
        }
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let cancellation = RunCancellation::new();
        let steering = arany::RunSteering::new();
        let mut input = request(&workspace, "Read the facts");
        input.include_paths.clear();
        let controls = RunControls {
            tools: ToolGrant::Configured,
            approvals: (case != "headless").then_some(approvals),
            cancellation: cancellation.clone(),
            steering: Some(steering.clone()),
            ..Default::default()
        };
        let answer = async {
            if case == "headless" {
                return None;
            }
            let approval = inbox.next().await.expect("pending approval");
            let presented = approval.intent.clone();
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(3600)).await;
            tokio::time::resume();
            assert_eq!(
                provider.requests.lock().unwrap().len(),
                1,
                "{case}: idle wait made no call"
            );
            match case {
                "cancel" => {
                    cancellation.cancel();
                    approval.decide(true);
                }
                "pause" => {
                    steering.pause();
                    drop(approval);
                }
                "disconnected" => drop(approval),
                "drift" => {
                    std::fs::rename(&workspace, temp.path().join("original")).unwrap();
                    std::fs::create_dir(&workspace).unwrap();
                    approval.decide(true);
                }
                "decline" | "decline_provider_error" => approval.decide(false),
                "rule_change" => {
                    private_file(
                        &state.join("permissions.json"),
                        br#"{"permissions":{"deny":["Read"]}}"#,
                    );
                    approval.decide(true);
                }
                "late_allow" => approval.decide(true),
                _ => unreachable!(),
            }
            Some(presented)
        };
        let (result, presented) =
            tokio::join!(run_with(&engine, &provider, input, controls), answer);
        let outcome = result.unwrap();
        let calls = provider.requests.lock().unwrap().len();
        engine.close().await.unwrap();
        let (events, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
        let run = view.runs.last().unwrap();
        assert_eq!(run.tools.len(), 1, "{case}");
        let tool = &run.tools[0];
        if let Some(presented) = presented {
            assert_eq!(tool.intent, presented, "{case}");
        }
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.intent, tool.intent);
        assert_eq!(
            observation.approval_cause,
            match case {
                "cancel" => Some(arany::ApprovalCause::Cancelled),
                "pause" => Some(arany::ApprovalCause::Paused),
                "disconnected" | "headless" => Some(arany::ApprovalCause::ChannelUnavailable),
                "decline" | "decline_provider_error" => Some(arany::ApprovalCause::Declined),
                "late_allow" | "rule_change" | "drift" => None,
                _ => unreachable!(),
            },
            "{case}"
        );
        assert_eq!(
            observation.attestation.is_some(),
            matches!(case, "late_allow" | "rule_change"),
            "{case}"
        );
        assert_eq!(
            calls,
            if matches!(
                case,
                "late_allow"
                    | "rule_change"
                    | "decline"
                    | "decline_provider_error"
                    | "disconnected"
                    | "headless"
            ) {
                2
            } else {
                1
            },
            "{case}"
        );
        let (verdict_index, verdict) = events
            .iter()
            .enumerate()
            .find_map(|(i, e)| match &e.event {
                Event::ToolStarted {
                    verdict: Some(verdict),
                    ..
                } => Some((i, verdict)),
                _ => None,
            })
            .unwrap();
        assert!(
            events[..verdict_index]
                .iter()
                .any(|e| matches!(e.event, Event::ProviderCallRecorded { .. }))
        );
        assert!(
            events[verdict_index + 1..]
                .iter()
                .any(|e| matches!(e.event, Event::ToolFinished { .. }))
        );
        assert_eq!(
            verdict.source,
            if matches!(case, "headless" | "disconnected" | "cancel" | "pause") {
                PermissionSource::UnavailableApproval
            } else {
                PermissionSource::Human
            },
            "{case}"
        );
        assert_eq!(
            run.status,
            if matches!(
                case,
                "late_allow" | "rule_change" | "decline" | "disconnected" | "headless"
            ) {
                RunStatus::Finished
            } else if case == "decline_provider_error" {
                assert_eq!(run.failure, Some(arany::RunFailure::ProviderCall));
                RunStatus::Failed
            } else if matches!(case, "cancel" | "pause") {
                RunStatus::Cancelled
            } else {
                RunStatus::Failed
            },
            "{case}"
        );
        if matches!(
            case,
            "decline" | "decline_provider_error" | "disconnected" | "headless"
        ) {
            let requests = provider.requests.lock().unwrap();
            let observation = requests[1]
                .input
                .tools
                .as_ref()
                .unwrap()
                .steps
                .last()
                .unwrap()
                .observations
                .last()
                .unwrap();
            assert_eq!(observation.disposition, arany::ToolDisposition::Denied);
            assert!(observation.output.contains(match case {
                "decline" | "decline_provider_error" => "user_declined",
                "disconnected" | "headless" => "channel_unavailable",
                _ => unreachable!(),
            }));
            assert_eq!(requests.len(), 2, "one ordinary continuation");
        }
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            calls,
            "replay never dispatches"
        );
        let active = events
            .iter()
            .find_map(|e| match e.event {
                Event::RunFinished { active_ms, .. } => Some(active_ms),
                _ => None,
            })
            .unwrap();
        assert!(active < 60_000, "{case}: human wait was charged");
    }
}

#[tokio::test]
async fn declined_tool_summaries_preserve_other_facts_across_compactions() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
    private_file(
        &state.join("permissions.json"),
        br#"{"permissions":{"ask":["Read"]}}"#,
    );
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let summary = "The project uses Rust and UTC. Reading the facts was declined.";
    let provider = decline_summary_provider(summary);
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let mut input = request(&workspace, "The project uses Rust and UTC. Read the facts.");
    input.include_paths.clear();
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: Some(approvals),
        ..Default::default()
    };
    let decline = async { inbox.next().await.map(|approval| approval.decide(false)) };
    let (outcome, declined) = tokio::join!(run_with(&engine, &provider, input, controls), decline);
    let outcome = outcome.expect("Tool-enabled submission reaches approval and continuation");
    assert!(declined.is_some(), "the Read must reach human approval");
    assert_eq!(outcome.run.status, RunStatus::Finished);
    assert_eq!(outcome.run.tools.len(), 1);
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let tool = &outcome.run.tools[0];
    let observation = tool.observation.as_ref().unwrap();
    assert_eq!(
        observation.approval_cause,
        Some(arany::ApprovalCause::Declined)
    );
    assert!(observation.attestation.is_none());
    let expected = "user_declined: no operation was dispatched";
    assert_eq!(observation.output, expected);
    assert_eq!(tool.model_denial_output(), Some(expected));
    assert_decline_summaries_retained(&engine, &provider, outcome.session_id, &workspace, summary)
        .await;
    engine.close().await.unwrap();
    let (_, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    assert_eq!(view.compactions.len(), 2);
}

fn decline_summary_provider(summary: &str) -> Arc<ScriptedProvider> {
    let responses = vec![
        tool_outcome(arany::ToolCall::Read {
            path: "notes/facts.txt".into(),
            offset: None,
            limit: Some(32),
        }),
        arany::test_support::goals::finish_with(
            "The read was declined; the project uses Rust and UTC.",
        ),
        arany::test_support::goals::finish_with("First continuation"),
        arany::test_support::goals::finish_with("Second continuation"),
    ];
    let compact = || {
        Ok(CompactionResponse {
            summary: summary.into(),
            response_id: None,
            usage: arany::Usage {
                input_tokens: Some(24),
                output_tokens: Some(16),
                ..Default::default()
            },
            wire_provenance: None,
        })
    };
    Arc::new(ScriptedProvider::new(responses).with_compactions(vec![compact(), compact()]))
}

async fn assert_decline_summaries_retained(
    engine: &Engine,
    provider: &Arc<ScriptedProvider>,
    session: SessionId,
    workspace: &Path,
    summary: &str,
) {
    for _ in 0..2 {
        engine
            .compact_session(
                &AdmittedProviders::single(provider.clone()),
                session,
                workspace.into(),
            )
            .await
            .unwrap();
        let mut input = request(workspace, "Continue with the project facts");
        input.session_id = Some(session);
        input.title = None;
        input.include_paths.clear();
        run_with(
            engine,
            provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            provider
                .requests
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .input
                .context_summary
                .as_deref(),
            Some(summary)
        );
    }
    let requests = provider.compact_requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let sources: Vec<_> = requests[0]
        .items
        .iter()
        .map(|item| match item {
            CompactionItem::Completed(turn) => turn.user.as_str(),
            CompactionItem::Unanswered { user, .. } => user.as_str(),
        })
        .collect();
    assert!(
        sources
            .iter()
            .any(|source| source.contains("user_declined"))
    );
    assert!(sources.iter().any(|source| source.contains("Rust and UTC")));
    assert_eq!(requests[1].previous_summary.as_deref(), Some(summary));
}

#[tokio::test]
async fn hook_free_permission_step_commits_four_events_and_replays_its_verdict() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    let state = temp.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":false,"commands":[],"skills":[],"mcp":[]}"#);
    private_file(&state.join("permissions.json"), b"{}");
    let engine = engine_support::open(
        root,
        EngineConfig {
            guard_executable: Some(std::env::current_exe().unwrap()),
            ..Default::default()
        },
    )
    .unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_outcome(arany::ToolCall::Read {
            path: "notes/facts.txt".into(),
            offset: None,
            limit: Some(32),
        }),
        arany::test_support::goals::finish_with("done"),
    ]));
    let mut input = request(&workspace, "Read the facts");
    input.include_paths.clear();
    let outcome = run_with(
        &engine,
        &provider,
        input,
        RunControls {
            tools: ToolGrant::Configured,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    engine.close().await.unwrap();
    let (events, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    let start = events
        .iter()
        .position(|event| matches!(event.event, Event::ToolStarted { .. }))
        .unwrap();
    assert_eq!(
        events[start - 2..=start + 1]
            .iter()
            .map(|event| event.event.kind())
            .collect::<Vec<_>>(),
        [
            "GoalProviderCallReserved",
            "ProviderCallRecorded",
            "ToolStarted",
            "ToolFinished"
        ]
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.event, Event::ToolProposed { .. }))
    );
    assert_eq!(provider.requests.lock().unwrap().len(), 2);
    let verdict = view.runs[0].tools[0].permission_verdict.as_ref().unwrap();
    let expected: [u8; 32] =
        Sha256::digest(serde_json::to_vec(&view.runs[0].tools[0].intent).unwrap()).into();
    assert_eq!(verdict.intent_digest, expected);
    assert!(verdict.approved);
    assert_eq!(view.runs[0].tools, outcome.run.tools);
}

#[tokio::test]
async fn multi_call_cancellation_settles_the_undispatched_suffix() {
    const TEST_TIMEOUT: Duration = Duration::from_secs(180);
    let _deadline = engine_support::deadline("multi-call cancellation", TEST_TIMEOUT);
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    for name in ["pending", "final"] {
        std::fs::write(workspace.join(format!("notes/{name}.txt")), "Fact").unwrap();
    }
    let state = temp.path().join("state");
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    private_file(
        &state.join("permissions.json"),
        br#"{"permissions":{"ask":["Read(./notes/pending.txt)"]}}"#,
    );
    let requests = ["facts", "facts", "pending", "final"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| arany::ToolRequest {
            id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
            call: arany::ToolCall::Read {
                path: format!("notes/{name}.txt"),
                offset: None,
                limit: None,
            },
        })
        .collect();
    let provider = Arc::new(ScriptedProvider::new(vec![ProviderOutcome::Tools(
        requests,
    )]));
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let cancellation = RunCancellation::new();
    let mut input = request(&workspace, "Read four files");
    input.include_paths.clear();
    let controls = RunControls {
        tools: ToolGrant::Configured,
        approvals: Some(approvals),
        cancellation: cancellation.clone(),
        ..Default::default()
    };
    let mut events = engine.subscribe();
    let cancel = async {
        let approval = inbox.next().await.expect("pending read approval");
        assert!(
            matches!(&approval.intent.call, arany::ToolCall::Read { path, .. } if path == "notes/pending.txt")
        );
        let mut completed = 0;
        while completed < 2 {
            let event = next_committed(&mut events, "reads finish before cancellation").await;
            if let Event::ToolFinished { observation, .. } = &event.event {
                assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
                completed += 1;
            }
        }
        cancellation.cancel();
        approval.decide(true);
    };
    let (outcome, ()) = tokio::join!(run_with(&engine, &provider, input, controls), cancel);
    let outcome = outcome.unwrap();
    engine.close().await.unwrap();
    let (events, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    assert_eq!(view.runs[0], outcome.run);
    assert_eq!(outcome.run.status, RunStatus::Cancelled);
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    let observations: Vec<_> = outcome
        .run
        .tools
        .iter()
        .map(|tool| tool.observation.as_ref().unwrap())
        .collect();
    assert_eq!(observations.len(), 4);
    assert!(
        observations[..2]
            .iter()
            .all(|observation| observation.disposition == arany::ToolDisposition::Succeeded)
    );
    assert!(
        observations[2..]
            .iter()
            .all(|observation| observation.attestation.is_none())
    );
    assert!(observations[3].output.starts_with("not_dispatched:"));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, Event::ToolFinished { .. }))
            .count(),
        4
    );
}

#[tokio::test]
async fn concurrent_read_limit_withdraws_pending_approval_before_late_reply() {
    for late_reply in [false, true] {
        let _deadline =
            engine_support::deadline("read limit during approval", Duration::from_secs(180));
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        std::fs::write(workspace.join("notes/pending.txt"), "Pending fact").unwrap();
        let state = temp.path().join("state");
        let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
        private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"ask":["Read(./notes/pending.txt)"]}}"#,
        );
        let (provider, gates) = gated_read_approval_provider(&["facts", "pending"]);
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let mut events = engine.subscribe();
        let mut input = request(&workspace, "Read two files");
        input.include_paths.clear();
        input.goal.budget = Some(arany::GoalBudget {
            max_active_ms: Some(120_000),
            ..Default::default()
        });
        let drive = async {
            let gate = gates.await.unwrap().pop().unwrap();
            drive_read_limit_approval(gate, &mut inbox, &mut events, late_reply).await;
        };
        let (outcome, ()) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: Some(approvals),
                    ..Default::default()
                }
            ),
            drive
        );
        let outcome = outcome.unwrap();
        engine.close().await.unwrap();
        let (events, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
        assert_eq!(view.runs[0], outcome.run);
        assert_eq!(outcome.run.status, RunStatus::Failed);
        assert_eq!(outcome.run.failure, Some(arany::RunFailure::ToolStopped));
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        assert_stopped_read_approval(&events, &outcome.run);
    }
}

fn assert_stopped_read_approval(events: &[arany::EventEnvelope], run: &arany::RunView) {
    let observations: Vec<_> = run
        .tools
        .iter()
        .map(|tool| tool.observation.as_ref().unwrap())
        .collect();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].disposition, arany::ToolDisposition::Limit);
    assert_eq!(
        observations[0].limit_cause,
        Some(arany::LimitCause::Timeout)
    );
    assert_eq!(observations[1].disposition, arany::ToolDisposition::Denied);
    assert!(observations[1].output.starts_with("not_dispatched:"));
    let verdict = run.tools[1].permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.decision, arany::permissions::Decision::Ask);
    assert_eq!(
        verdict.source,
        arany::permissions::PermissionSource::UnavailableApproval
    );
    assert!(!verdict.approved);
    assert!(
        observations
            .iter()
            .all(|observation| observation.attestation.is_none())
    );
    let finished: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.event {
            Event::ToolFinished { observation, .. } => Some(observation.intent.id),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        observations
            .iter()
            .map(|observation| observation.intent.id)
            .collect::<Vec<_>>()
    );
}

async fn drive_read_limit_approval(
    (entered, release): (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ),
    inbox: &mut arany::ApprovalInbox,
    events: &mut arany::Subscription,
    late_reply: bool,
) {
    entered.await.unwrap();
    let mut approval = inbox.next().await.expect("second read approval");
    assert!(
        matches!(&approval.intent.call, arany::ToolCall::Read { path, .. } if path == "notes/pending.txt")
    );
    let runtime_ms = loop {
        let event = next_committed(events, "first read runtime").await;
        if let Event::ToolStarted { intent, .. } = &event.event
            && matches!(&intent.call, arany::ToolCall::Read { path, .. } if path == "notes/facts.txt")
        {
            break intent.limits.runtime_ms;
        }
    };
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(u64::from(runtime_ms) + 1)).await;
    tokio::time::resume();
    loop {
        let event = next_committed(events, "read limit commits without approval reply").await;
        if let Event::ToolFinished { observation, .. } = &event.event {
            assert!(
                matches!(&observation.intent.call, arany::ToolCall::Read { path, .. } if path == "notes/facts.txt")
            );
            assert_eq!(observation.disposition, arany::ToolDisposition::Limit);
            break;
        }
    }
    approval.abandoned().await;
    if late_reply {
        approval.decide(true);
    } else {
        assert_eq!(approval.withdrawal_committed().await, Some(true));
    }
    drop(release);
}

type ReadGate = (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
);

fn gated_read_approval_provider(
    names: &[&str],
) -> (
    Arc<ScriptedProvider>,
    tokio::sync::oneshot::Receiver<Vec<ReadGate>>,
) {
    let calls: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(index, name)| arany::ToolRequest {
            id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
            call: arany::ToolCall::Read {
                path: format!("notes/{name}.txt"),
                offset: None,
                limit: None,
            },
        })
        .collect();
    let gated: Vec<_> = calls[..calls.len() - 1]
        .iter()
        .map(|request| request.call.clone())
        .collect();
    let provider = ScriptedProvider::new(vec![
        ProviderOutcome::Tools(calls),
        arany::test_support::goals::finish_with("Read step complete"),
    ]);
    let recorded = Arc::clone(&provider.requests);
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    let provider = provider.with_before_call(0, move || {
        let run = recorded.lock().unwrap()[0].run_id;
        let gates = gated
            .iter()
            .map(|call| arany::FaultPlan::gate_in_process_read(run, call.clone()))
            .collect();
        send.lock().unwrap().take().unwrap().send(gates).unwrap();
    });
    (Arc::new(provider), receive)
}

const READ_APPROVAL_ACTIVE_MS: u64 = 10_000;
const READ_POST_APPROVAL_ACTIVE_MS: u64 = 10_000;
const READ_APPROVAL_IDLE_MS: u64 = 60_000;
const READ_ACCOUNTING_TOLERANCE_MS: u64 = 5_000;

#[tokio::test]
async fn concurrent_reads_charge_active_time_after_denied_or_unavailable_approval() {
    for (unavailable, idle) in [(false, false), (true, false), (false, true), (true, true)] {
        let _deadline =
            engine_support::deadline("read approval active time", Duration::from_secs(180));
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        for name in ["overlap", "pending"] {
            std::fs::write(workspace.join(format!("notes/{name}.txt")), "Other fact").unwrap();
        }
        let state = temp.path().join("state");
        let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
        private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"ask":["Read(./notes/pending.txt)"]}}"#,
        );
        let (provider, gates) = gated_read_approval_provider(&["facts", "overlap", "pending"]);
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let mut events = engine.subscribe();
        let mut input = request(&workspace, "Read three files");
        input.include_paths.clear();
        input.goal.budget = Some(arany::GoalBudget {
            max_active_ms: Some(120_000),
            ..Default::default()
        });
        let drive = async {
            drive_read_approval_accounting(
                gates.await.unwrap(),
                &mut inbox,
                &mut events,
                unavailable,
                idle,
            )
            .await
        };
        let (outcome, expected_ms) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: Some(approvals),
                    ..Default::default()
                }
            ),
            drive
        );
        let outcome = outcome.unwrap();
        assert_eq!(outcome.run.status, RunStatus::Finished);
        engine.close().await.unwrap();
        let (events, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
        let active_ms = events
            .iter()
            .find_map(|event| match event.event {
                Event::RunFinished { active_ms, .. } => Some(active_ms),
                _ => None,
            })
            .unwrap();
        assert!(
            (expected_ms..expected_ms + READ_ACCOUNTING_TOLERANCE_MS).contains(&active_ms),
            "unavailable={unavailable}, idle={idle}: {active_ms} versus {expected_ms}"
        );
        assert_eq!(view.runs[0], outcome.run);
        assert_eq!(view.goals[0].active_ms, active_ms);
        assert_eq!(provider.requests.lock().unwrap().len(), 2);
    }
}

async fn drive_read_approval_accounting(
    gates: Vec<ReadGate>,
    inbox: &mut arany::ApprovalInbox,
    events: &mut arany::Subscription,
    unavailable: bool,
    idle: bool,
) -> u64 {
    let [
        (first_entered, first_release),
        (overlap_entered, overlap_release),
    ]: [ReadGate; 2] = gates.try_into().unwrap();
    first_entered.await.unwrap();
    overlap_entered.await.unwrap();
    let approval = inbox.next().await.expect("last read approval");
    advance_read_clock(READ_APPROVAL_ACTIVE_MS).await;
    overlap_release.send(()).unwrap();
    let mut first_release = Some(first_release);
    if idle {
        first_release.take().unwrap().send(()).unwrap();
    }
    for _ in 0..if idle { 2 } else { 1 } {
        let observation = next_read_observation(events).await;
        assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
    }
    if idle {
        advance_read_clock(READ_APPROVAL_IDLE_MS).await;
    }
    if unavailable {
        drop(approval);
    } else {
        approval.decide(false);
    }
    let denied = next_read_observation(events).await;
    assert_eq!(denied.disposition, arany::ToolDisposition::Denied);
    assert!(denied.attestation.is_none());
    assert!(denied.output.contains(if unavailable {
        "channel_unavailable"
    } else {
        "user_declined"
    }));
    if let Some(release) = first_release {
        advance_read_clock(READ_POST_APPROVAL_ACTIVE_MS).await;
        release.send(()).unwrap();
        let observation = next_read_observation(events).await;
        assert_eq!(observation.disposition, arany::ToolDisposition::Succeeded);
        READ_APPROVAL_ACTIVE_MS + READ_POST_APPROVAL_ACTIVE_MS
    } else {
        READ_APPROVAL_ACTIVE_MS
    }
}

async fn next_read_observation(events: &mut arany::Subscription) -> arany::ToolObservation {
    loop {
        let event = next_committed(events, "read result").await;
        if let Event::ToolFinished { observation, .. } = &event.event {
            return observation.clone();
        }
    }
}

async fn advance_read_clock(milliseconds: u64) {
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(milliseconds)).await;
    tokio::time::resume();
}

#[tokio::test]
async fn reducer_only_preflight_refusals_commit_reasons_and_terminal_recovery() {
    const TEST_TIMEOUT: Duration = Duration::from_secs(180);
    let _deadline = engine_support::deadline("Tool preflight refusal", TEST_TIMEOUT);
    for kind in ["ToolProposed", "ToolStarted"] {
        let temp = tempfile::tempdir().unwrap();
        let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
        let state = temp.path().join("state");
        let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
        private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"ask":["Read"]}}"#,
        );
        let calls = if kind == "ToolStarted" { 2 } else { 1 };
        let provider = preflight_provider(calls);
        let initial = run_with(
            &engine,
            &provider,
            request(&workspace, "Prepare"),
            RunControls::default(),
        )
        .await
        .unwrap();
        let mut injected = arany::FaultPlan::corrupt_tool_preflight(initial.session_id, kind);
        let mut input = request(&workspace, "Read the facts");
        input.session_id = Some(initial.session_id);
        input.title = None;
        input.include_paths.clear();
        let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
        let approve = async {
            if kind == "ToolStarted" {
                inbox.next().await.expect("precommitted Read").decide(true);
            }
        };
        let (outcome, ()) = tokio::join!(
            run_with(
                &engine,
                &provider,
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: (kind == "ToolStarted").then_some(approvals),
                    ..Default::default()
                }
            ),
            approve
        );
        let outcome = outcome.unwrap();
        injected
            .try_recv()
            .expect("candidate pin corruption reached the real reducer validator");
        assert_eq!(provider.requests.lock().unwrap().len(), 2, "{kind}");
        engine.close().await.unwrap();
        let (events, view) = arany::test_support::goals::replayed(&state, initial.session_id).await;
        assert_eq!(view.runs.last().unwrap(), &outcome.run, "{kind}");
        assert_eq!(outcome.run.status, RunStatus::Failed, "{kind}");
        assert_eq!(
            outcome.run.failure,
            Some(arany::RunFailure::ProposalRefused),
            "{kind}"
        );
        assert_eq!(
            outcome.run.tools.len(),
            if kind == "ToolStarted" { calls } else { 0 }
        );
        assert!(
            outcome
                .run
                .tools
                .iter()
                .all(|tool| tool.started_sequence.is_none() && tool.observation.is_none())
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("notes/facts.txt")).unwrap(),
            "Fact"
        );
        assert_preflight_refusal_receipt(&events, &outcome.run);
        assert_preflight_followup(&state, &workspace, &provider, initial.session_id).await;
    }
}

fn preflight_provider(calls: usize) -> Arc<ScriptedProvider> {
    let requests = (0..calls)
        .map(|index| arany::ToolRequest {
            id: arany::ToolUseId::new(format!("call_{index}")).unwrap(),
            call: arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: None,
            },
        })
        .collect();
    Arc::new(ScriptedProvider::new(vec![
        arany::test_support::goals::finish_with("ready"),
        ProviderOutcome::Tools(requests),
        arany::test_support::goals::finish_with("recovered"),
    ]))
}

async fn assert_preflight_followup(
    state: &Path,
    workspace: &Path,
    provider: &Arc<ScriptedProvider>,
    session: SessionId,
) {
    let engine = engine_support::open(
        StateRoot::open_existing(state).unwrap(),
        EngineConfig::default(),
    )
    .unwrap();
    let mut input = request(workspace, "Explain the refused action");
    input.session_id = Some(session);
    input.title = None;
    input.include_paths.clear();
    let outcome = run_with(&engine, provider, input, RunControls::default())
        .await
        .unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    engine.close().await.unwrap();
    let calls = provider.requests.lock().unwrap();
    assert_eq!(calls.len(), 3);
    let feedback = calls[2].input.previous_run_failure.unwrap();
    assert_eq!(
        feedback.refusal_cause,
        Some(arany::ProposalRefusalCause::Phase)
    );
    assert_eq!(
        feedback.stop,
        arany::RunStop::Failed(arany::RunFailure::ProposalRefused)
    );
    assert_eq!(
        feedback.provider_disposition,
        Some(ProviderCallDisposition::ToolRequested)
    );
    assert_eq!(feedback.provider_failure, None);
}

fn assert_preflight_refusal_receipt(events: &[arany::EventEnvelope], run: &arany::RunView) {
    let (index, event) = events
        .iter()
        .enumerate()
        .find(|(_, event)| {
            event.run_id == Some(run.id)
                && matches!(event.event, Event::ToolPreflightRefused { .. })
        })
        .unwrap();
    let Event::ToolPreflightRefused { record, .. } = &event.event else {
        unreachable!()
    };
    assert_eq!(record.refusal.cause, arany::ProposalRefusalCause::Phase);
    assert_eq!(
        record.refusal.to_string(),
        "Phase: Tool start violates the committed Session transition."
    );
    assert_eq!(
        event.event.payload().unwrap(),
        format!(
            r#"{{"intent_digest":{},"refusal":{{"cause":"phase","reason":"Tool start violates the committed Session transition."}}{}}}"#,
            serde_json::to_string(&record.intent_digest).unwrap(),
            record
                .verdict
                .as_ref()
                .map(|verdict| format!(",\"verdict\":{}", serde_json::to_string(verdict).unwrap()))
                .unwrap_or_default(),
        )
    );
    assert_eq!(record.verdict.is_some(), !run.tools.is_empty());
    if let Some(verdict) = &record.verdict {
        assert_eq!(verdict.source, arany::permissions::PermissionSource::Human);
        assert!(verdict.approved);
        assert_eq!(verdict.intent_digest, record.intent_digest);
    }
    assert_eq!(
        run.agents
            .iter()
            .find(|agent| agent.role == arany::AgentRole::Primary)
            .unwrap()
            .tool_refusal
            .as_ref(),
        Some(record)
    );
    assert_eq!(events[index + 1].event.kind(), "AgentFinished");
    assert_eq!(events[index + 2].event.kind(), "RunFinished");
    assert!(!events.iter().any(|event| event.run_id == Some(run.id)
        && matches!(
            event.event,
            Event::ToolStarted { .. } | Event::ToolFinished { .. }
        )));
    let mut duplicate = events.to_vec();
    duplicate[index + 1].event = event.event.clone();
    assert!(SessionView::replay(event.session_id, &duplicate).is_err());
}

#[tokio::test]
async fn concurrent_preflight_refusal_drains_started_reads_without_starting_the_suffix() {
    let _deadline = engine_support::deadline(
        "concurrent Tool preflight refusal",
        Duration::from_secs(180),
    );
    let temp = tempfile::tempdir().unwrap();
    let workspace = arany::test_support::goals::facts_workspace(temp.path(), "Fact");
    std::fs::write(workspace.join("notes/pending.txt"), "Other fact").unwrap();
    let state = temp.path().join("state");
    let engine = super::goals::facts_engine(StateRoot::admit(&state).unwrap(), &state);
    private_file(
        &state.join("permissions.json"),
        br#"{"permissions":{"ask":["Read(./notes/pending.txt)"]}}"#,
    );
    let (provider, gates) = gated_read_approval_provider(&["facts", "pending"]);
    {
        let mut responses = provider.responses.lock().unwrap();
        let Ok(ProviderOutcome::Tools(requests)) = responses.front_mut().unwrap() else {
            unreachable!()
        };
        requests.push(arany::ToolRequest {
            id: arany::ToolUseId::new("call_suffix").unwrap(),
            call: arany::ToolCall::Read {
                path: "notes/facts.txt".into(),
                offset: None,
                limit: None,
            },
        });
    }
    let (approvals, mut inbox) = arany::ToolApprovals::new(arany::ApprovalMode::Request);
    let mut input = request(&workspace, "Read three files");
    input.include_paths.clear();
    let drive = async {
        let (entered, release) = gates.await.unwrap().pop().unwrap();
        entered.await.unwrap();
        let approval = inbox.next().await.expect("second read approval");
        let session = provider.requests.lock().unwrap()[0].session_id;
        let injected = arany::FaultPlan::corrupt_tool_preflight(session, "ToolStarted");
        approval.decide(true);
        injected.await.unwrap();
        release.send(()).unwrap();
    };
    let (outcome, ()) = tokio::join!(
        run_with(
            &engine,
            &provider,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals),
                ..Default::default()
            }
        ),
        drive
    );
    let outcome = outcome.unwrap();
    engine.close().await.unwrap();
    let (events, view) = arany::test_support::goals::replayed(&state, outcome.session_id).await;
    assert_concurrent_preflight_refusal(
        &events,
        &view,
        &outcome.run,
        provider.requests.lock().unwrap().len(),
    );
}

fn assert_concurrent_preflight_refusal(
    events: &[arany::EventEnvelope],
    view: &SessionView,
    run: &arany::RunView,
    calls: usize,
) {
    assert_eq!(&view.runs[0], run);
    assert_eq!(run.failure, Some(arany::RunFailure::ProposalRefused));
    assert_eq!(calls, 1);
    assert_eq!(run.tools.len(), 3);
    assert_eq!(
        run.tools[0].observation.as_ref().unwrap().disposition,
        arany::ToolDisposition::Succeeded
    );
    assert!(
        run.tools[1..]
            .iter()
            .all(|tool| tool.started_sequence.is_none() && tool.observation.is_none())
    );
    let refused = events
        .iter()
        .find(|event| matches!(event.event, Event::ToolPreflightRefused { .. }))
        .unwrap();
    let Event::ToolPreflightRefused { record, .. } = &refused.event else {
        unreachable!()
    };
    assert_eq!(record.refusal.cause, arany::ProposalRefusalCause::Phase);
    let digest: [u8; 32] = Sha256::digest(serde_json::to_vec(&run.tools[1].intent).unwrap()).into();
    assert_eq!(record.intent_digest, digest);
    assert!(record.verdict.as_ref().unwrap().approved);
    assert!(run.tools[0].finished_sequence.unwrap() < refused.sequence);
}
