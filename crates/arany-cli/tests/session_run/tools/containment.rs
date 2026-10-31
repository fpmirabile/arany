use super::*;

struct BoundedLoop {
    requests: Arc<Mutex<Vec<ProviderRequest>>>,
    team: bool,
}

#[async_trait::async_trait]
impl Provider for BoundedLoop {
    fn identity(&self) -> &arany::ProviderIdentity {
        static IDENTITY: std::sync::LazyLock<arany::ProviderIdentity> =
            std::sync::LazyLock::new(|| {
                arany::ProviderIdentity::new("openai", "guard-model", arany::Provenance::native())
            });
        &IDENTITY
    }

    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let mut requests = self.requests.lock().unwrap();
        let outcome = if self.team {
            match requests.len() {
                0 => ProviderOutcome::Delegate(arany::Delegate {
                    agent: None,
                    children: vec![arany::ChildAssignment {
                        editor: None,
                        objective: "Review the stated task without effects".into(),
                        profile: None,
                    }],
                }),
                1 => ProviderOutcome::Answer(Answer {
                    text: "Read-only child result".into(),
                }),
                2 => crate::tool_outcome(ToolCall::Edit {
                    path: "src/current.txt".into(),
                    expected_digest: Some(hash(b"current")),
                    old: "current".into(),
                    new: "current\n2026-10-06\n".into(),
                }),
                3 => {
                    assert_eq!(
                        request
                            .input
                            .tools
                            .as_ref()
                            .unwrap()
                            .observations()
                            .last()
                            .unwrap()
                            .disposition,
                        ToolDisposition::Succeeded
                    );
                    ProviderOutcome::Answer(Answer {
                        text: "Appended 2026-10-06 after the attested Edit; child only reasoned."
                            .into(),
                    })
                }
                _ => return Err(ProviderError::InvalidOutcome),
            }
        } else {
            crate::tool_outcome(ToolCall::Read {
                path: "src/unselected.txt".into(),
                offset: None,
                limit: Some(1),
            })
        };
        requests.push(request);
        Ok(ProviderResponse {
            turn: arany::ProviderTurn::scripted(),
            outcome,
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
#[ignore = "native Linux aggregate Tool budget and primary-only team continuation"]
async fn tool_loop_exhaustion_and_team_children_do_not_widen_authority() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    let outside = temp.path().join("unselected.txt");
    private_file(&outside, b"OUTSIDE_GUARD_CANARY");
    std::os::unix::fs::symlink(&outside, workspace.join("src/unselected.txt")).unwrap();
    for team in [true, false] {
        let state_path = temp
            .path()
            .join(if team { "state-team" } else { "state-budget" });
        let state = StateRoot::admit(&state_path).unwrap();
        basic_config(&state_path);
        if team {
            let mut config: Value =
                serde_json::from_slice(&std::fs::read(state_path.join("tools.json")).unwrap())
                    .unwrap();
            config["workspace_paths"] = json!(["."]);
            private_file(
                &state_path.join("tools.json"),
                &serde_json::to_vec(&config).unwrap(),
            );
        }
        let previous = if team {
            let failed_requests = Arc::new(Mutex::new(Vec::new()));
            let failed_provider = Arc::new(Proposal {
                call: ToolCall::Edit {
                    path: "src/current.txt".into(),
                    expected_digest: Some(hash(b"current")),
                    old: "current".into(),
                    new: "must not run".into(),
                },
                requests: failed_requests.clone(),
                inspect_first: false,
                plan_on_inspection: false,
                omit_plan: false,
            });
            let failed = arany::test_support::engine::open(
                state,
                EngineConfig {
                    guard_executable: Some("/usr/bin/false".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let outcome = tokio::time::timeout(
                Duration::from_secs(15),
                run_with(
                    &failed,
                    &failed_provider,
                    proposal_request(&workspace),
                    RunControls {
                        tools: ToolGrant::Configured,
                        ..Default::default()
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
            failed.close().await.unwrap();
            assert_eq!(outcome.run.status, RunStatus::Failed);
            let observation = outcome.run.tools[0].observation.as_ref().unwrap();
            assert_eq!(
                observation.disposition,
                ToolDisposition::Failed,
                "a helper that never receives GO cannot edit the file"
            );
            assert!(observation.attestation.is_none());
            assert!(observation.output.contains("no operation was dispatched"));
            assert_eq!(
                failed_requests.lock().unwrap().len(),
                1,
                "no automatic retry after failed bootstrap"
            );
            assert_eq!(
                std::fs::read(workspace.join("src/current.txt")).unwrap(),
                b"current"
            );
            assert!(
                arany_cli::presentation::run_feedback(&outcome.run)
                    .unwrap()
                    .contains("before the action started")
            );
            Some(outcome)
        } else {
            None
        };
        let state = StateRoot::open_existing(&state_path).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(BoundedLoop {
            requests: requests.clone(),
            team,
        });
        let engine = arany::test_support::engine::open(
            state,
            EngineConfig {
                guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                ..Default::default()
            },
        )
        .unwrap();
        let (approvals, _inbox) = arany::ToolApprovals::new(arany::ApprovalMode::AutoEdits);
        let mut request = proposal_request(&workspace);
        if team {
            request.session_id = Some(previous.as_ref().unwrap().session_id);
            request.title = None;
            request.include_paths = vec!["src/current.txt".into()];
            request.policy = CollaborationPolicy::Team {
                max_active_children: 1,
            };
        }
        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            run_with(
                &engine,
                &provider,
                request,
                RunControls {
                    tools: ToolGrant::Configured,
                    approvals: Some(approvals.clone()),
                    ..Default::default()
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        engine.close().await.unwrap();
        let requests = requests.lock().unwrap().clone();
        if team {
            assert_eq!(outcome.run.status, RunStatus::Finished);
            assert_eq!(
                std::fs::read_to_string(workspace.join("src/current.txt")).unwrap(),
                "current\n2026-10-06\n"
            );
            assert_eq!(requests.len(), 4);
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(
                    request.phase,
                    [
                        AgentPhase::RootPlan,
                        AgentPhase::ChildWork,
                        AgentPhase::RootSynthesis,
                        AgentPhase::RootSynthesis
                    ][index]
                );
                assert_eq!(request.model, "guard-model");
                assert_eq!(request.max_output_tokens, 4096);
                if let Some(tools) = &request.input.tools {
                    let catalog: Value = serde_json::from_str(tools.catalog()).unwrap();
                    assert_eq!(catalog["host_clock"]["timezone"], "UTC");
                    let timestamp = catalog["host_clock"]["unix_ms"].as_u64().unwrap();
                    let current = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    assert!(timestamp <= current && current - timestamp < 20_000);
                }
                assert!(
                    request.input.workspace_guidance.is_empty()
                        && request.input.images.is_empty()
                        && request.input.context_summary.is_none()
                );
                assert_eq!(
                    request.input.includes,
                    vec![format!(
                        "File: \"src/current.txt\"\nSHA-256: {}\nContent (untrusted data):\ncurrent",
                        hash(b"current")
                    )]
                );
                if index == 1 {
                    assert!(request.input.history.is_empty());
                    assert_eq!(
                        request.input.objective,
                        "Review the stated task without effects"
                    );
                    assert!(request.input.child_results.is_empty());
                    let research: Value =
                        serde_json::from_str(request.input.tools.as_ref().unwrap().catalog())
                            .unwrap();
                    assert_eq!(research["write"], false);
                    assert_eq!(research["commands"], serde_json::json!([]));
                    assert!(
                        request
                            .input
                            .tools
                            .as_ref()
                            .unwrap()
                            .observations()
                            .is_empty()
                    );
                } else {
                    assert_eq!(request.input.history.len(), 1);
                    let history: Value =
                        serde_json::from_str(&request.input.history[0].assistant).unwrap();
                    assert!(
                        history["scope"]
                            .as_str()
                            .unwrap()
                            .contains("historical Run")
                    );
                    assert_eq!(history["run_status"], "Failed");
                    assert_eq!(history["tool_records"][0]["disposition"], "failed");
                    assert!(history["tool_records"][0].get("cache_update").is_none());
                    assert_eq!(
                        history["tool_records"][0]["workspace_effect"],
                        "not_confirmed"
                    );
                    assert_eq!(request.input.objective, "Guard corpus");
                    assert_eq!(
                        request.input.tools.as_ref().unwrap().observations().len(),
                        index.saturating_sub(2)
                    );
                    assert_eq!(request.input.child_results.len(), usize::from(index >= 2));
                    if index >= 2 {
                        assert_eq!(
                            request.input.child_results[0].result,
                            "Read-only child result"
                        );
                    }
                }
            }
            assert_eq!(outcome.run.tools.len(), 1);
        } else {
            assert_eq!(outcome.run.status, RunStatus::Failed);
            assert_eq!(outcome.run.tools.len(), 16);
            assert!(outcome.run.assistant_message.is_none());
            assert_proposal_requests(&requests, 17);
            assert!(
                !arany_cli::presentation::run_feedback(&outcome.run)
                    .unwrap()
                    .contains("approval expired")
            );
            assert!(
                outcome
                    .run
                    .tools
                    .iter()
                    .all(|tool| tool.observation.as_ref().unwrap().disposition
                        == ToolDisposition::Denied)
            );
        }
        drop(requests);
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(
            store
                .load_view(outcome.session_id)
                .await
                .unwrap()
                .unwrap()
                .runs
                .last()
                .unwrap(),
            &outcome.run
        );
        if let Some(previous) = previous {
            assert_eq!(
                store
                    .load_view(previous.session_id)
                    .await
                    .unwrap()
                    .unwrap()
                    .runs[0],
                previous.run
            );
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "native Linux denied-path, atomic-write, descendant and quota Tool conformance"]
async fn guard_failure_corpus_does_not_widen_paths_or_lose_effect_facts() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let runtime_state_path = temp.path().join("state-runtime-overlap");
    let runtime_state = StateRoot::admit(&runtime_state_path).unwrap();
    basic_config(&runtime_state_path);
    let runtime_requests = Arc::new(Mutex::new(Vec::new()));
    let runtime_engine_provider = Arc::new(Proposal {
        call: ToolCall::List { path: "".into() },
        requests: runtime_requests.clone(),
        inspect_first: false,
        plan_on_inspection: false,
        omit_plan: false,
    });
    let runtime_engine = arany::test_support::engine::open(
        runtime_state,
        EngineConfig {
            guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            run_with(
                &runtime_engine,
                &runtime_engine_provider,
                proposal_request(Path::new("/usr")),
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                }
            ),
        )
        .await
        .expect("runtime-root admission deadline"),
        Err(arany::EngineError::Tool(arany::ToolError::Path))
    ));
    assert!(runtime_requests.lock().unwrap().is_empty());
    runtime_engine.close().await.unwrap();
    let connection = rusqlite::Connection::open(runtime_state_path.join("events.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM events", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    let outside = temp.path().join("outside");
    private_file(&outside, b"OUTSIDE_GUARD_CANARY");
    private_file(&workspace.join("src/current.txt"), b"current");
    std::fs::hard_link(&outside, workspace.join("src/hardlink")).unwrap();
    symlink(&outside, workspace.join("src/link")).unwrap();
    symlink(temp.path(), workspace.join("src/parent-link")).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        workspace.join("src/fifo"),
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let read = |path: &str| ToolCall::Read {
        path: path.into(),
        offset: None,
        limit: None,
    };
    let search = |path: &str| ToolCall::Search {
        path: path.into(),
        query: "OUTSIDE_GUARD_CANARY".into(),
        regex: false,
        include: None,
        case_sensitive: true,
        offset: 0,
        limit: 100,
    };
    let deep_path = format!("deep/{}note.txt", "d/".repeat(16));
    std::fs::create_dir_all(workspace.join(&deep_path).parent().unwrap()).unwrap();
    private_file(&workspace.join(&deep_path), b"OUTSIDE_GUARD_CANARY");
    let rows = [
        ("symlink", read("src/link"), ToolDisposition::Denied),
        ("hardlink", read("src/hardlink"), ToolDisposition::Denied),
        ("fifo", read("src/fifo"), ToolDisposition::Denied),
        (
            "root-search-symlink",
            search("src/link"),
            ToolDisposition::Denied,
        ),
        (
            "root-search-hardlink",
            search("src/hardlink"),
            ToolDisposition::Denied,
        ),
        (
            "root-search-fifo",
            search("src/fifo"),
            ToolDisposition::Denied,
        ),
        (
            "root-search-depth",
            search(&deep_path),
            ToolDisposition::Denied,
        ),
        (
            "scope",
            ToolCall::Write {
                path: "unselected.txt".into(),
                expected_digest: None,
                content: "must not be written".into(),
            },
            ToolDisposition::Denied,
        ),
        (
            "unplanned",
            ToolCall::Write {
                path: "src/unplanned.txt".into(),
                expected_digest: None,
                content: "written without a plan".into(),
            },
            ToolDisposition::Succeeded,
        ),
        (
            "parent",
            ToolCall::Write {
                path: "src/parent-link/created".into(),
                expected_digest: None,
                content: "must not escape".into(),
            },
            ToolDisposition::Denied,
        ),
        (
            "stale",
            ToolCall::Edit {
                path: "src/current.txt".into(),
                expected_digest: Some(hash(b"stale")),
                old: "current".into(),
                new: "overwritten".into(),
            },
            ToolDisposition::Conflict,
        ),
        (
            "create",
            ToolCall::Write {
                path: "src/current.txt".into(),
                expected_digest: None,
                content: "overwritten".into(),
            },
            ToolDisposition::Conflict,
        ),
        (
            "long-mkdir",
            ToolCall::Mkdir {
                path: format!("src/dir-{}", "x".repeat(200)),
            },
            ToolDisposition::Succeeded,
        ),
        (
            "long-write",
            ToolCall::Write {
                path: format!("src/file-{}", "x".repeat(200)),
                expected_digest: None,
                content: "bounded mutation receipt".into(),
            },
            ToolDisposition::Succeeded,
        ),
    ];
    for (name, call, expected) in rows {
        let state_path = temp.path().join(format!("state-{name}"));
        let state = StateRoot::admit(&state_path).unwrap();
        basic_config(&state_path);
        if name.starts_with("root-search-") {
            let mut config: Value =
                serde_json::from_slice(&std::fs::read(state_path.join("tools.json")).unwrap())
                    .unwrap();
            config["workspace_paths"] = json!(["."]);
            private_file(
                &state_path.join("tools.json"),
                &serde_json::to_vec(&config).unwrap(),
            );
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Proposal {
            call,
            requests: requests.clone(),
            inspect_first: matches!(name, "scope" | "unplanned" | "long-mkdir"),
            plan_on_inspection: name == "long-mkdir",
            omit_plan: name == "unplanned",
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
        .expect("path-case parent deadline")
        .unwrap();
        engine.close().await.unwrap();
        let rejected_by_plan = name == "scope";
        assert_eq!(outcome.run.status, RunStatus::Finished, "case {name}");
        for tool in &outcome.run.tools {
            if matches!(tool.intent.call, ToolCall::Read { .. }) {
                assert_eq!(tool.intent.limits.runtime_ms, 60_000, "case {name}");
            }
        }
        assert_proposal_requests(
            &requests.lock().unwrap(),
            if matches!(name, "scope" | "unplanned" | "long-mkdir") {
                4
            } else {
                3
            },
        );
        if rejected_by_plan {
            assert!(
                outcome.run.tools.len() == 2
                    && matches!(outcome.run.tools[0].intent.call, ToolCall::Read { .. })
                    && outcome.run.tools[0]
                        .observation
                        .as_ref()
                        .is_some_and(|observation| {
                            observation.disposition == ToolDisposition::Succeeded
                        }),
                "out-of-plan call is never dispatched"
            );
            assert!(!workspace.join("unselected.txt").exists());
        } else {
            let tool_index = if matches!(name, "long-mkdir" | "unplanned") {
                1
            } else {
                0
            };
            let observation = outcome.run.tools[tool_index].observation.as_ref().unwrap();
            assert_eq!(observation.disposition, expected, "case {name}");
            if name.starts_with("root-search-") {
                assert!(!observation.output.contains("OUTSIDE_GUARD_CANARY"));
            }
            if expected == ToolDisposition::Succeeded {
                assert!(observation.output.len() <= 128);
                match &observation.intent.call {
                    ToolCall::Mkdir { path } => assert!(workspace.join(path).is_dir()),
                    ToolCall::Write { path, content, .. } => {
                        assert_eq!(
                            std::fs::read(workspace.join(path)).unwrap(),
                            content.as_bytes()
                        );
                    }
                    _ => panic!("unexpected successful mutation case"),
                }
            }
        }
        assert_eq!(std::fs::read(&outside).unwrap(), b"OUTSIDE_GUARD_CANARY");
        assert_eq!(
            std::fs::read(workspace.join("src/current.txt")).unwrap(),
            b"current"
        );
        assert!(!temp.path().join("created").exists());
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        let view = store.load_view(outcome.session_id).await.unwrap().unwrap();
        assert_eq!(view.runs[0], outcome.run);
        if name == "long-mkdir" {
            assert!(view.goals[0].plan_contract.is_some());
            assert!(matches!(
                view.runs[0].tools[0].intent.call,
                ToolCall::Read { .. }
            ));
        }
        store.close().await.unwrap();
    }
    std::fs::remove_file(workspace.join("src/link")).unwrap();
    std::fs::remove_file(workspace.join("src/hardlink")).unwrap();
    std::fs::remove_file(workspace.join("src/parent-link")).unwrap();
    std::fs::remove_file(workspace.join("src/fifo")).unwrap();
    let command_rows = [
        ("flood", "printf '%100000s' x", ToolDisposition::Limit),
        (
            "disk",
            "/usr/bin/dd if=/dev/zero of=/scratch/fill bs=1048576 count=65 status=none",
            ToolDisposition::Failed,
        ),
        (
            "descendant",
            "(/usr/bin/setsid /usr/bin/sleep 100 >/dev/null 2>&1 </dev/null &) ; printf done",
            ToolDisposition::Succeeded,
        ),
    ];
    for (name, script, expected) in command_rows {
        let state_path = temp.path().join(format!("state-{name}"));
        let state = StateRoot::admit(&state_path).unwrap();
        basic_config(&state_path);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Proposal {
            call: ToolCall::Command {
                program: "bash".into(),
                args: vec!["-c".into(), script.into()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: None,
                additional_reads: None,
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
            Duration::from_secs(20),
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
        .expect("quota-case parent deadline")
        .unwrap();
        engine.close().await.unwrap();
        assert_eq!(
            outcome.run.status,
            RunStatus::Finished,
            "case {name}: {:?}",
            outcome.run.tools
        );
        assert_eq!(outcome.run.tools[0].intent.limits.runtime_ms, 120_000);
        let observation = outcome.run.tools[0].observation.as_ref().unwrap();
        assert_eq!(observation.disposition, expected, "case {name}");
        assert_proposal_requests(&requests.lock().unwrap(), 3);
        assert!(observation.attestation.is_some());
        if name == "descendant" {
            let unit = format!("arany-tool-{}.service", observation.intent.id);
            let mut command = std::process::Command::new("/usr/bin/systemctl");
            command.args([
                "--user",
                "show",
                "--property=ActiveState,ControlGroup",
                &unit,
            ]);
            let output = crate::process_output_before_deadline(command);
            let properties = String::from_utf8(output.stdout).unwrap();
            assert!(
                properties.contains("ActiveState=inactive")
                    && properties.contains("ControlGroup=\n"),
                "owned descendants must be gone"
            );
        }
    }
    let config: Value =
        serde_json::from_slice(&std::fs::read(temp.path().join("state-flood/tools.json")).unwrap())
            .unwrap();
    crate::tool_replay::native_command_changeset_corpus(temp.path(), &config, "bash").await;
}

async fn unit_group(unit: &str) -> Option<String> {
    let properties = unit_properties(unit, &["LoadState", "ControlGroup"]).await;
    let load = properties["LoadState"].as_str();
    let group = properties["ControlGroup"].as_str();
    if load == "not-found" {
        assert!(group.is_empty(), "absent unit cannot retain a cgroup");
        return None;
    }
    assert_eq!(
        load, "loaded",
        "native cgroup observer failed; absence is not proved"
    );
    (!group.is_empty()).then(|| group.to_owned())
}

async fn unit_properties(
    unit: &str,
    columns: &[&str],
) -> std::collections::BTreeMap<String, String> {
    use tokio::io::AsyncReadExt;
    let mut command = tokio::process::Command::new("/usr/bin/systemctl");
    command
        .args(["--user", "show", unit])
        .arg(format!("--property={}", columns.join(",")))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("native cgroup observer spawn");
    let mut stdout = child.stdout.take().unwrap();
    let collected = tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        (&mut stdout)
            .take(4097)
            .read_to_end(&mut bytes)
            .await
            .expect("native cgroup observer output");
        assert!(bytes.len() <= 4096, "native cgroup observer output cap");
        let status = child.wait().await.expect("native cgroup observer reap");
        (status, bytes)
    })
    .await;
    let (status, bytes) = match collected {
        Ok(value) => value,
        Err(_) => {
            let _ = child.start_kill();
            child
                .wait()
                .await
                .expect("native cgroup observer timeout reap");
            panic!("native cgroup observer deadline; absence is not proved");
        }
    };
    let text = String::from_utf8(bytes).expect("native cgroup observer UTF-8");
    let properties: std::collections::BTreeMap<_, _> = text
        .lines()
        .map(|line| {
            let (name, value) = line
                .split_once('=')
                .expect("native cgroup observer property");
            (name.to_owned(), value.to_owned())
        })
        .collect();
    assert_eq!(
        properties.len(),
        columns.len(),
        "native cgroup observer exact properties"
    );
    assert_eq!(
        text.lines().count(),
        columns.len(),
        "duplicate observer property"
    );
    assert!(
        columns
            .iter()
            .all(|column| properties.contains_key(*column))
    );
    assert!(
        status.success()
            || properties
                .get("LoadState")
                .is_some_and(|state| state == "not-found"),
        "native cgroup observer failed; absence is not proved"
    );
    properties
}

#[tokio::test]
#[ignore = "native Linux in-flight double-fork/setsid cancellation Tool conformance"]
async fn cancellation_owns_the_actual_descendant_unit_and_never_retries_it() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(
        &workspace.join("src/tree.c"),
        include_bytes!("../../fixtures/tool-tree.c"),
    );
    private_file(
        &workspace.join("src/tree.sh"),
        b"set -eu\n/usr/bin/gcc src/tree.c -o /scratch/tree\nexec /scratch/tree\n",
    );
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    basic_config(&state_path);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Proposal {
        call: ToolCall::Command {
            program: "bash".into(),
            args: vec!["src/tree.sh".into()],
            cwd: "".into(),
            resource_class: None,
            timeout_ms: None,
            additional_reads: None,
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
    let cancellation = arany::RunCancellation::new();
    let mut events = engine.subscribe();
    tokio::time::timeout(Duration::from_secs(30), async {
        let running = run_with(&engine, &provider, proposal_request(&workspace), RunControls { tools: ToolGrant::Configured, cancellation: cancellation.clone(), ..Default::default() });
        tokio::pin!(running);
        let intent = loop {
            tokio::select! {
                outcome = &mut running => panic!("Run ended before payload gate: {:?}", outcome.map(|outcome| outcome.run.status)),
                event = events.next() => if let Some(arany::Tapped::Committed(event)) = event
                    && let arany::Event::ToolStarted { intent, .. } = &event.event { break intent.clone(); },
            }
        };
        let unit = format!("arany-tool-{}.service", intent.id);
        let payload_gate = async {
            loop {
                if let Some(group) = unit_group(&unit).await {
                    assert!(group.starts_with("/user.slice/") && group.ends_with(&unit) && !group.contains(".."));
                    let path = Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
                    if let Ok(members) = std::fs::read_to_string(path.join("cgroup.procs")) {
                        assert!(members.len() <= 4096 && members.lines().count() <= 64);
                        let mut root = false;
                        let mut leaf = false;
                        for pid in members.lines() {
                            assert!(pid.bytes().all(|byte| byte.is_ascii_digit()));
                            if let Ok(name) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                                root |= name.trim() == "arany-test-root";
                                leaf |= name.trim() == "arany-test-leaf";
                            }
                        }
                        if root && leaf { break path; }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let group = tokio::select! {
            outcome = &mut running => panic!("Run ended before double-fork gate: {:?}", outcome.map(|outcome| outcome.run.status)),
            group = payload_gate => group,
        };
        assert!(cancellation.cancel());
        let outcome = running.await;
        if let Ok(events) = std::fs::read_to_string(group.join("cgroup.events")) {
            assert!(events.lines().any(|line| line == "populated 0"));
        } else { assert!(!group.exists()); }
        // An Uncertain outcome outranks the cancellation: the Run journals no terminal Event.
        assert!(
            matches!(outcome, Err(arany::EngineError::Tool(arany::ToolError::Uncertain))),
            "a cancelled in-flight command is reported unknown"
        );
        assert_seed_retired(intent.id);
    }).await.expect("complete cancellation parent deadline");
    engine.close().await.unwrap();
    assert_proposal_requests(&requests.lock().unwrap(), 1);
    let listed = arany::list_sessions(
        StateRoot::open_existing(&state_path).unwrap(),
        workspace.clone(),
    )
    .await
    .unwrap();
    let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
    let events = store.load_session(listed[0].id).await.unwrap();
    assert!(
        events
            .iter()
            .all(|envelope| !matches!(envelope.event, arany::Event::RunFinished { .. }))
    );
    let view = store.load_view(listed[0].id).await.unwrap().unwrap();
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert_eq!(view.runs[0].tools.len(), 1);
    assert!(view.runs[0].assistant_message.is_none());
    assert_eq!(
        view.runs[0].tools[0]
            .observation
            .as_ref()
            .unwrap()
            .disposition,
        ToolDisposition::Uncertain
    );
    store.close().await.unwrap();
}

#[tokio::test]
#[ignore = "native Linux host-network/credential isolation and inherited seccomp Tool conformance"]
async fn command_descendants_cannot_reach_host_canaries_or_guard_memory() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    use std::net::{TcpListener, UdpSocket};
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    tcp.set_nonblocking(true).unwrap();
    udp.set_nonblocking(true).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    private_file(
        &workspace.join("src/isolation.c"),
        include_bytes!("../../fixtures/tool-isolation.c"),
    );
    let outside = temp.path().join("outside-canary");
    private_file(&outside, b"HOST_ONLY_CANARY");
    let script = format!(
        "set -eu\n/usr/bin/gcc src/isolation.c -o /scratch/isolation\nexec /scratch/isolation {} {} '{}'\n",
        tcp.local_addr().unwrap().port(),
        udp.local_addr().unwrap().port(),
        outside.display()
    );
    private_file(&workspace.join("src/isolation.sh"), script.as_bytes());
    let state_path = temp.path().join("state");
    let state = StateRoot::admit(&state_path).unwrap();
    basic_config(&state_path);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(Proposal {
        call: ToolCall::Command {
            program: "bash".into(),
            args: vec!["src/isolation.sh".into()],
            cwd: "".into(),
            resource_class: None,
            timeout_ms: None,
            additional_reads: None,
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
    .unwrap()
    .unwrap();
    engine.close().await.unwrap();
    assert_eq!(outcome.run.status, RunStatus::Finished);
    let observation = outcome.run.tools[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    let output: Value = serde_json::from_str(&observation.output).unwrap();
    assert_eq!(output["exit_code"], 0, "bounded probe output: {output}");
    assert_eq!(
        output["stdout"],
        "isolation and inherited syscall restrictions passed\n"
    );
    assert_eq!(output["stderr"], "");
    assert!(
        tcp.accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(
        udp.recv_from(&mut [0; 32])
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"HOST_ONLY_CANARY");
    assert_proposal_requests(&requests.lock().unwrap(), 3);
}

#[tokio::test]
#[ignore = "native Linux aggregate memory/PID Tool enforcement, including OOM cleanup"]
async fn resource_quotas_apply_to_payload_descendants_and_oom_is_uncertain() {
    if !crate::isolation::enter_account_isolated_test() {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    private_file(&workspace.join("src/current.txt"), b"current");
    private_file(
        &workspace.join("src/quotas.c"),
        include_bytes!("../../fixtures/tool-quotas.c"),
    );
    private_file(
        &workspace.join("src/quotas.sh"),
        b"set -eu\n/usr/bin/gcc src/quotas.c -o /scratch/quotas\nexec /scratch/quotas \"$1\"\n",
    );
    for mode in ["pids", "memory"] {
        let state_path = temp.path().join(format!("state-{mode}"));
        let state = StateRoot::admit(&state_path).unwrap();
        basic_config(&state_path);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Proposal {
            call: ToolCall::Command {
                program: "bash".into(),
                args: vec!["src/quotas.sh".into(), mode.into()],
                cwd: "".into(),
                resource_class: None,
                timeout_ms: None,
                additional_reads: None,
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
        let mut events = engine.subscribe();
        let outcome = tokio::time::timeout(Duration::from_secs(25), async {
            let running = run_with(&engine, &provider, proposal_request(&workspace), RunControls { tools: ToolGrant::Configured, cancellation: arany::RunCancellation::new(), ..Default::default() });
            tokio::pin!(running);
            if mode == "memory" {
                let intent = loop {
                    tokio::select! {
                        outcome = &mut running => panic!("Run ended before the OOM payload start gate: {:?}", outcome.map(|value| value.run.status)),
                        event = events.next() => if let Some(arany::Tapped::Committed(event)) = event
                    && let arany::Event::ToolStarted { intent, .. } = &event.event { break intent.clone(); },
                    }
                };
                let unit = format!("arany-tool-{}.service", intent.id);
                let payload_gate = async {
                    loop {
                        if let Some(group) = unit_group(&unit).await {
                            assert!(group.starts_with("/user.slice/") && group.ends_with(&unit) && !group.contains(".."));
                            let group = Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
                            if let Ok(members) = std::fs::read_to_string(group.join("cgroup.procs")) {
                                assert!(members.len() <= 4096 && members.lines().count() <= 64);
                                for pid in members.lines() {
                                    assert!(pid.bytes().all(|byte| byte.is_ascii_digit()));
                                    if std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|name| name.trim() == "arany-oom-ready") {
                                        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
                                        if status.lines().any(|line| line.starts_with("State:\tT")) {
                                            return (group, rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap());
                                        }
                                    }
                                }
                            }
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                };
                let (group, pid) = tokio::select! {
                    outcome = &mut running => panic!("Run ended before the stopped OOM payload gate: {:?}", outcome.map(|value| value.run.status)),
                    ready = payload_gate => ready,
                };
                let columns = ["LoadState", "ControlGroup", "InvocationID", "Result", "OOMKills"];
                let before = unit_properties(&unit, &columns).await;
                assert_eq!(before["LoadState"], "loaded");
                assert_eq!(Path::new("/sys/fs/cgroup").join(before["ControlGroup"].trim_start_matches('/')), group);
                let invocation = &before["InvocationID"];
                assert!(invocation.len() == 32 && invocation.bytes().all(|byte| byte.is_ascii_hexdigit()) && invocation != &"0".repeat(32));
                assert_eq!(before["Result"], "success");
                assert_eq!(before["OOMKills"].parse::<u64>().unwrap(), 0, "fresh owned payload group");
                rustix::process::kill_process(pid, rustix::process::Signal::CONT).expect("release exact stopped test-owned OOM payload");
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let after = unit_properties(&unit, &columns).await;
                        assert_eq!(after["LoadState"], "loaded", "OOM evidence disappeared before observation");
                        assert_eq!(&after["InvocationID"], invocation, "OOM unit identity drift");
                        assert!(matches!(after["Result"].as_str(), "success" | "oom-kill"), "unexpected non-OOM service failure");
                        let kills = after["OOMKills"].parse::<u64>().unwrap();
                        assert_ne!(kills, u64::MAX, "unknown OOM counter is not kernel evidence");
                        if kills > 0 && after["Result"] == "oom-kill" {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }).await.expect("payload started but no exact-invocation OOM evidence was observed");
            }
            running.await
        }).await.unwrap();
        engine.close().await.unwrap();
        // An OOM-killed payload's effect is unknown: the Run journals no terminal Event (D3).
        let (session_id, run) = match outcome {
            Ok(outcome) => {
                assert_eq!(mode, "pids", "an OOM-killed payload never settles its Run");
                (outcome.session_id, outcome.run)
            }
            Err(arany::EngineError::Tool(arany::ToolError::Uncertain)) if mode == "memory" => {
                let listed = arany::list_sessions(
                    StateRoot::open_existing(&state_path).unwrap(),
                    workspace.clone(),
                )
                .await
                .unwrap();
                assert_eq!(listed.len(), 1);
                let store =
                    Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
                let events = store.load_session(listed[0].id).await.unwrap();
                assert!(
                    events.iter().all(|envelope| !matches!(
                        envelope.event,
                        arany::Event::RunFinished { .. }
                    )),
                    "an unknown outcome journals no terminal Event"
                );
                let run = store
                    .load_view(listed[0].id)
                    .await
                    .unwrap()
                    .unwrap()
                    .runs
                    .remove(0);
                store.close().await.unwrap();
                assert_eq!(run.status, RunStatus::Interrupted);
                (listed[0].id, run)
            }
            Err(error) => panic!("{mode}: unexpected Run error {error}"),
        };
        let observation = run.tools[0].observation.as_ref().unwrap();
        if mode == "pids" {
            assert_eq!(
                observation.disposition,
                ToolDisposition::Succeeded,
                "quota admission observation of {} bytes",
                observation.output.len()
            );
            let output: Value = serde_json::from_str(&observation.output).unwrap();
            assert_eq!(output["exit_code"], 0);
            assert_eq!(output["stdout"], "descendant quota enforced\n");
            assert_eq!(run.status, RunStatus::Finished);
            assert_proposal_requests(&requests.lock().unwrap(), 3);
        } else {
            assert_eq!(
                observation.disposition,
                ToolDisposition::Uncertain,
                "OOM is not a no-effect failure"
            );
            assert!(run.assistant_message.is_none());
            assert_proposal_requests(&requests.lock().unwrap(), 1);
        }
        assert!(
            unit_group(&format!("arany-tool-{}.service", observation.intent.id))
                .await
                .is_none()
        );
        let store = Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
        assert_eq!(
            store.load_view(session_id).await.unwrap().unwrap().runs[0],
            run
        );
        store.close().await.unwrap();
        if mode == "memory" {
            let fresh_requests = Arc::new(Mutex::new(Vec::new()));
            let resumed_provider = Arc::new(BoundedLoop {
                requests: fresh_requests.clone(),
                team: true,
            });
            let resumed = arany::test_support::engine::open(
                StateRoot::open_existing(&state_path).unwrap(),
                EngineConfig {
                    guard_executable: Some(env!("CARGO_BIN_EXE_arany").into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let mut next = proposal_request(&workspace);
            next.session_id = Some(session_id);
            next.title = None;
            next.include_paths = vec!["src/current.txt".into()];
            next.policy = CollaborationPolicy::Team {
                max_active_children: 1,
            };
            let fresh = tokio::time::timeout(
                Duration::from_secs(20),
                run_with(
                    &resumed,
                    &resumed_provider,
                    next,
                    RunControls {
                        tools: ToolGrant::Configured,
                        ..Default::default()
                    },
                ),
            )
            .await
            .unwrap()
            .unwrap();
            resumed.close().await.unwrap();
            assert_eq!(fresh.run.status, RunStatus::Finished);
            assert_eq!(
                std::fs::read(workspace.join("src/current.txt")).unwrap(),
                b"current\n2026-10-06\n"
            );
            {
                let calls = fresh_requests.lock().unwrap();
                assert_eq!(calls.len(), 6);
                assert!(
                    calls[0]
                        .input
                        .tools
                        .as_ref()
                        .unwrap()
                        .observations()
                        .is_empty()
                );
                let historical: Value =
                    serde_json::from_str(&calls[0].input.history[0].assistant).unwrap();
                assert!(
                    historical["scope"]
                        .as_str()
                        .unwrap()
                        .contains("historical Run")
                );
                assert_eq!(historical["tool_records"][0]["disposition"], "uncertain");
                assert!(fresh.run.tools.iter().all(|tool| !matches!(
                    tool.intent.call,
                    ToolCall::Command { .. }
                )
                    && tool.observation.as_ref().unwrap().disposition
                        == ToolDisposition::Succeeded));
            }
            let store =
                Store::open_read_only(StateRoot::open_existing(&state_path).unwrap()).unwrap();
            let view = store.load_view(session_id).await.unwrap().unwrap();
            assert_eq!(view.runs, vec![run, fresh.run]);
            store.close().await.unwrap();
        }
    }
}
