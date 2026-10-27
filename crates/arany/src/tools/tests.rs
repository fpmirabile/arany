use super::catalog::catalog_call_schemas;
use super::config::Config;
use super::types::*;
use serde_json::{Value, json};

pub(super) fn config_value() -> Value {
    json!({"version":1,"workspace_paths":["src","Cargo.toml"],"write":false,"commands":[],"skills":[],"mcp":[]})
}

#[test]
fn guard_error_after_dispatch_marks_only_a_selected_command_cache_not_attempted() {
    let command = ToolCall::Command {
        program: "cargo".into(),
        args: vec!["build".into()],
        cwd: ".".into(),
    };
    assert_eq!(
        super::guard_error_cache_update(&command, true),
        CacheUpdate::NotAttempted
    );
    assert_eq!(
        super::guard_error_cache_update(&command, false),
        CacheUpdate::NotApplicable
    );
    assert_eq!(
        super::guard_error_cache_update(&ToolCall::List { path: "src".into() }, true),
        CacheUpdate::NotApplicable
    );

    let intent = EffectIntent {
        host: None,
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call: command,
        limits: ToolLimits::default(),
        use_count: 1,
    };
    let observation = super::uncertain_observation(
        intent,
        super::ToolError::Operation,
        CacheUpdate::NotAttempted,
    );
    assert!(observation.valid());
    assert!(observation.stops_run());
    assert!(observation.attestation.is_none());
    assert_eq!(observation.cache_update, CacheUpdate::NotAttempted);
}

#[test]
fn guard_failures_record_their_stage_and_check_per_intent_for_tests_in_every_build() {
    let intent = |call| EffectIntent {
        host: None,
        host_command: None,
        target: crate::session::worker::EffectTarget::Workspace,
        id: uuid::Uuid::now_v7(),
        run_id: crate::session::RunId::new(),
        agent_run_id: crate::session::AgentRunId::new(),
        policy_digest: [1; 32],
        enforcement_digest: [2; 32],
        workspace_device: 1,
        workspace_inode: 2,
        call,
        limits: ToolLimits::default(),
        use_count: 1,
    };
    let command = ToolCall::Command {
        program: "sh".into(),
        args: vec![],
        cwd: String::new(),
    };
    let cleanup = intent(command.clone());
    let failure = serde_json::from_str(r#"{"stage":"storage_cleanup","check":"detach_failed"}"#)
        .expect("closed storage cleanup failure");
    let observation = super::uncertain_observation(
        cleanup.clone(),
        super::ToolError::GuardBootstrap(failure),
        CacheUpdate::NotApplicable,
    );
    assert_eq!(
        observation.output,
        "Guard storage cleanup unconfirmed; effects may have occurred. Do not retry automatically."
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(cleanup.id).as_deref(),
        Some("Guard stage=storage cleanup check=detach failed")
    );
    let unstarted = intent(command);
    super::unstarted_observation(
        unstarted.clone(),
        super::ToolError::GuardRejected(super::guard::GuardStage::ConnectionDeadline),
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(unstarted.id).as_deref(),
        Some("Guard stage=connection deadline")
    );
    let expired =
        super::unstarted_observation(unstarted.clone(), super::ToolError::Timeout(String::new()));
    assert_eq!(expired.disposition, ToolDisposition::Limit);
    assert_eq!(expired.limit_cause, Some(super::LimitCause::Timeout));
    let selected = super::unstarted_observation(
        unstarted.clone(),
        super::ToolError::Timeout(String::new()).in_selection(),
    );
    assert_eq!(selected.disposition, ToolDisposition::Limit);
    assert_eq!(selected.limit_cause, Some(super::LimitCause::Timeout));
    let mut file = intent(ToolCall::Mkdir {
        path: "src/new".into(),
    });
    file.enforcement_digest = FileReceipt::enforcement_digest();
    let failed = super::unstarted_observation(file.clone(), super::ToolError::ChangedInput);
    assert!(failed.valid() && failed.stops_run() && failed.attestation.is_none());
    assert_eq!(
        failed.output,
        "File pinned input unavailable before dispatch; no operation was dispatched."
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(file.id).as_deref(),
        Some("File stage=pinned input")
    );
    assert_eq!(
        crate::test_support::recorded_tool_failure(uuid::Uuid::now_v7()),
        None
    );
}

#[test]
fn child_research_excludes_git_reads() {
    for call in [
        ToolCall::GitStatus { path: ".".into() },
        ToolCall::GitDiff {
            path: ".".into(),
            base: None,
            head: None,
            staged: false,
        },
        ToolCall::GitLog {
            path: ".".into(),
            revision: None,
            max_count: 1,
        },
        ToolCall::GitBranch {
            name: None,
            start_point: None,
            scope: Some("all".into()),
            base: Some("main".into()),
            max_count: Some(256),
        },
    ] {
        assert!(call.is_goal_plan_prelude());
        assert!(!call.is_child_research(), "{call:?}");
    }
    for call in [
        ToolCall::List { path: ".".into() },
        ToolCall::Read {
            path: "a".into(),
            offset: None,
            limit: Some(1),
        },
        ToolCall::Search {
            path: ".".into(),
            query: "x".into(),
            regex: false,
            include: None,
            case_sensitive: true,
            offset: 0,
            limit: 100,
        },
        ToolCall::Skill {
            name: "review".into(),
            resource: None,
        },
        ToolCall::McpList {
            server: "local".into(),
        },
    ] {
        assert!(call.is_child_research(), "{call:?}");
    }
    for call in [
        ToolCall::McpCall {
            server: "local".into(),
            tool: "probe".into(),
            schema_digest: "0".repeat(64),
            arguments: "{}".into(),
        },
        ToolCall::Write {
            path: "a".into(),
            expected_digest: None,
            content: "x".into(),
        },
    ] {
        assert!(!call.is_child_research(), "{call:?}");
    }
}

pub(super) fn in_process_runtime(workspace: &std::path::Path, config: Value) -> super::ToolRuntime {
    let root = super::fs::open_directory(workspace).unwrap();
    super::ToolRuntime {
        host_settings_admitted: true,
        host_settings_saved: Default::default(),
        guard_available: true,
        git_writes_available: false,
        host_authentication: Default::default(),
        handover: None,
        definitions: Default::default(),
        permissions: None,
        result_filter: None,
        unguarded_rules: None,
        context_limit_bytes: MAX_TOOL_CONTEXT_BYTES,
        cache_state: None,
        command_environment: std::sync::Arc::new(crate::tools::CommandEnvironment::for_path(
            "/usr/bin:/bin",
        )),
        spill_access: None,
        config: serde_json::from_value(config.clone()).unwrap(),
        workspace: workspace.to_owned(),
        workspace_identity: super::fs::workspace_identity(&root).unwrap(),
        reads: Some(super::InProcessReads {
            skills: serde_json::from_value::<Config>(config.clone())
                .unwrap()
                .skills
                .iter()
                .map(|skill| {
                    (
                        skill.name.clone(),
                        super::fs::open_directory(std::path::Path::new(&skill.directory))
                            .ok()
                            .map(std::sync::Arc::new),
                    )
                })
                .collect(),
            root: std::sync::Arc::new(root),
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(super::MAX_IN_PROCESS_READS)),
        }),
        guard_executable: std::path::PathBuf::new(),
        enforcement_digest: [1; 32],
        queue_progress: None,
        active_action_gate: None,
        hook_stdin: None,
        checkpoint_gate: None,
        #[cfg(target_os = "macos")]
        system_programs: None,
        process_enforcement_digest: [1; 32],
    }
}

pub(super) async fn inspect_read(
    runtime: &super::ToolRuntime,
    intent: EffectIntent,
) -> ToolObservation {
    if !runtime.guard_available
        || !runtime.config.allows(&intent.call)
        || !super::discovery::possible(&intent.call)
    {
        return runtime.execute(intent, super::Cancellation::new()).await;
    }
    let root = match runtime.pinned_root(&intent) {
        Ok(root) => root,
        Err(error) => return super::unstarted_observation(intent, error),
    };
    if super::discovery::exact_search(&root, &intent.call).unwrap_or(true) {
        return runtime.execute(intent, super::Cancellation::new()).await;
    }
    let hidden = runtime.hidden_paths(&intent.call);
    let result = super::discovery::inspect(super::discovery::Request {
        root: &root,
        call: &intent.call,
        config: &runtime.config,
        hidden: &|path| hidden.covers(path),
        result_bytes: intent.limits.result_bytes as usize,
        deadline: std::time::Instant::now() + super::selection::DISCOVERY_TIME,
        output: super::discovery::Output::Tool,
        cancellation: None,
    });
    let (disposition, output, enforcement_cause, limit_cause) =
        super::dispatch::outcome(&intent, result);
    ToolObservation {
        spill: None,
        attestation: Some(Attestation::Guard(super::GuardReceipt {
            contract_version: 1,
            intent_digest: intent.digest(),
            enforcement_digest: intent.enforcement_digest,
            limits: intent.limits.clone(),
        })),
        intent,
        disposition,
        output,
        enforcement_cause,
        limit_cause,
        approval_cause: None,
        guard_stage: None,
        cache_update: super::CacheUpdate::NotApplicable,
    }
}

pub(super) async fn run_in_process(
    runtime: &super::ToolRuntime,
    call: ToolCall,
) -> ToolObservation {
    let intent = runtime.intent(
        crate::session::RunId::new(),
        crate::session::AgentRunId::new(),
        call,
    );
    inspect_read(runtime, intent).await
}

#[test]
fn result_capacity_is_the_context_fit_formula_with_a_floor_and_the_general_ceiling() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = in_process_runtime(
        temp.path(),
        json!({"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}),
    );
    runtime.context_limit_bytes = 1 << 20;
    let limit = runtime.context_limit_bytes();
    let reserve = super::ToolRuntime::step_context_reserve(false, 0);
    let fitted = |overhead: usize| limit.saturating_sub(overhead + 512);
    let ceiling = |overhead: usize| {
        runtime
            .result_capacity(overhead, reserve)
            .map(|capacity| capacity.min(MAX_TOOL_RESULT_BYTES))
    };
    let below_ceiling = limit - 512 - (MAX_TOOL_RESULT_BYTES / 2);
    assert_eq!(
        runtime.result_capacity(below_ceiling, reserve),
        Some(fitted(below_ceiling))
    );
    assert_eq!(ceiling(below_ceiling), Some(MAX_TOOL_RESULT_BYTES / 2));
    assert!(
        fitted(0) > MAX_TOOL_RESULT_BYTES,
        "the context fit exceeds the ceiling at zero overhead"
    );
    assert_eq!(ceiling(0), Some(MAX_TOOL_RESULT_BYTES));
    let at_floor = limit - 512 - 128;
    assert_eq!(runtime.result_capacity(at_floor, reserve), Some(128));
    assert_eq!(runtime.result_capacity(at_floor + 1, reserve), None);
}

#[tokio::test]
async fn unavailable_guard_hides_non_host_tools_and_refuses_before_dispatch() {
    let temp = tempfile::tempdir().unwrap();
    let mut runtime = in_process_runtime(temp.path(), config_value());
    runtime.config.skills.push(super::config::Skill {
        disable_model_invocation: false,
        name: "root".into(),
        description: "root skill".into(),
        directory: temp.path().to_str().unwrap().to_owned(),
        files: std::collections::BTreeMap::from([("SKILL.md".into(), "0".repeat(64))]),
    });
    assert!(runtime.allows_skill("root"));
    assert!(runtime.context().unwrap().catalog().contains("root skill"));
    runtime.guard_available = false;
    assert!(!runtime.allows_skill("root"));
    let context: serde_json::Value =
        serde_json::from_str(runtime.context().unwrap().catalog()).unwrap();
    assert_eq!(context["commands"], serde_json::json!([]));
    assert_eq!(context["mcp_servers"], serde_json::json!([]));
    assert_eq!(context["file_tools"], "");
    assert_eq!(context["skills"], serde_json::json!([]));
    assert_eq!(context["host_commands"], true);
    assert_eq!(context["write"], false);
    let calls = catalog_call_schemas(&context);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["properties"]["operation"]["enum"],
        json!(["command_unguarded"])
    );
    let research: Value =
        serde_json::from_str(runtime.research_context(3).unwrap().catalog()).unwrap();
    assert!(catalog_call_schemas(&research).is_empty());
    assert!(runtime.worker_policy().is_none());
    std::fs::create_dir(temp.path().join("src")).unwrap();
    std::fs::write(temp.path().join("src/file.txt"), "file canary").unwrap();
    for (call, site) in [
        (
            ToolCall::Command {
                program: "sh".into(),
                args: vec!["-c".into(), "touch canary".into()],
                cwd: String::new(),
            },
            "Guard",
        ),
        (ToolCall::List { path: "src".into() }, "Read"),
        (
            ToolCall::Read {
                path: "src/file.txt".into(),
                offset: None,
                limit: None,
            },
            "Read",
        ),
        (
            ToolCall::Search {
                path: "src".into(),
                query: "canary".into(),
                regex: false,
                include: None,
                case_sensitive: true,
                offset: 0,
                limit: 100,
            },
            "Read",
        ),
        (
            ToolCall::Glob {
                pattern: "src/*".into(),
                path: ".".into(),
                limit: 100,
            },
            "Read",
        ),
        (
            ToolCall::Skill {
                name: "root".into(),
                resource: None,
            },
            "Read",
        ),
    ] {
        let result = run_in_process(&runtime, call).await;
        assert!(result.attestation.is_none());
        assert_eq!(result.disposition, ToolDisposition::Failed);
        let mut expected = format!(
            "{site} native protection unavailable before dispatch; no operation was dispatched."
        );
        if site == "Guard" {
            expected.push_str(" Cause: native protection. Check the Guard executable and native sandbox prerequisites, then resend your message.");
            if cfg!(debug_assertions) {
                expected.push_str(
                    " For details, inspect development.log in the Session State directory.",
                );
            }
        }
        assert_eq!(result.output, expected);
    }
    assert!(!temp.path().join("canary").exists());
}

#[test]
fn undispatched_guard_failures_explain_safe_causes_and_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(temp.path(), config_value());
    for (error, cause, next) in [
        (
            super::ToolError::GuardRejected(super::guard::GuardStage::NamespaceBootstrap),
            "namespace bootstrap",
            "Enable unprivileged user namespaces",
        ),
        (
            super::ToolError::GuardRejected(super::guard::GuardStage::NativeManager),
            "native manager",
            "Restore the user service manager",
        ),
        (
            super::ToolError::Operation,
            "exact cause was not reported",
            "Check the Guard executable",
        ),
    ] {
        let intent = runtime.intent(
            crate::RunId::new(),
            crate::AgentRunId::new(),
            ToolCall::Command {
                program: "script".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
        );
        let observation = super::unstarted_observation(intent, error);
        assert_eq!(observation.disposition, ToolDisposition::Failed);
        assert!(observation.attestation.is_none());
        assert!(observation.output.contains(cause));
        assert!(observation.output.contains(next));
        assert!(observation.output.contains("resend your message"));
        assert!(!observation.output.contains("/goal resume"));
        assert_eq!(
            observation.output.contains("development.log"),
            cfg!(debug_assertions)
        );
        assert!(observation.output.contains("no operation was dispatched"));
    }
}

#[test]
fn selection_boundary_failures_are_named_and_other_variants_keep_their_stage() {
    use super::{FailureStage, SelectionCause, ToolError};
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(temp.path(), config_value());
    let command = || {
        runtime.intent(
            crate::RunId::new(),
            crate::AgentRunId::new(),
            ToolCall::Command {
                program: "script".into(),
                args: Vec::new(),
                cwd: ".".into(),
            },
        )
    };
    for (error, stage, label, disposition, limit_cause, stops_run) in [
        (
            ToolError::Timeout(String::new()).in_selection(),
            FailureStage::SelectionTimedOut,
            "project selection timed out",
            ToolDisposition::Limit,
            Some(super::LimitCause::Timeout),
            false,
        ),
        (
            ToolError::NotFound.in_selection(),
            FailureStage::SelectionInputVanished,
            "project input vanished",
            ToolDisposition::Failed,
            None,
            true,
        ),
        (
            ToolError::Conflict.in_selection(),
            FailureStage::SelectionInputConflict,
            "project input conflict",
            ToolDisposition::Failed,
            None,
            true,
        ),
    ] {
        assert!(matches!(
            error,
            ToolError::ProjectSelection(
                SelectionCause::TimedOut
                    | SelectionCause::InputVanished
                    | SelectionCause::InputConflict
            )
        ));
        let observation = super::unstarted_observation(command(), error);
        assert_eq!(
            observation.guard_stage,
            Some(super::GuardStageFact { stage, check: None })
        );
        assert_eq!(
            observation.guard_stage_diagnostic().as_deref(),
            Some(format!("Guard stage: {label}").as_str())
        );
        assert!(observation.valid());
        assert_eq!(observation.disposition, disposition);
        assert_eq!(observation.limit_cause, limit_cause);
        assert_eq!(observation.stops_run(), stops_run);
        assert!(
            observation
                .output
                .starts_with(&format!("Guard {label} before dispatch;"))
        );
        assert!(
            observation.output.contains("Cause: ")
                && observation.output.contains(&format!(
                    "Cause: {label}. Resend your message; the Guard executable is not involved."
                ))
        );
        assert!(!observation.output.contains("native sandbox prerequisites"));
        let json = serde_json::to_string(&observation).unwrap();
        assert!(json.contains("\"guard_stage\":{\"stage\":\"selection_"));
        assert_eq!(
            serde_json::from_str::<super::ToolObservation>(&json).unwrap(),
            observation
        );
    }
    for (error, stage) in [
        (ToolError::Timeout(String::new()), FailureStage::Execution),
        (ToolError::NotFound, FailureStage::Execution),
        (ToolError::Conflict, FailureStage::Execution),
        (ToolError::Cancelled, FailureStage::Cancelled),
    ] {
        assert_eq!(super::failure_stage(&error), stage);
    }
    let timed_out = super::unstarted_observation(command(), ToolError::Timeout(String::new()));
    assert_eq!(timed_out.output, "Tool runtime expired");
    assert_eq!(timed_out.limit_cause, Some(super::LimitCause::Timeout));
    assert!(timed_out.guard_stage.is_none());
    for error in [ToolError::NotFound, ToolError::Conflict] {
        let observation = super::unstarted_observation(command(), error);
        assert!(
            observation
                .output
                .starts_with("Guard execution unavailable before dispatch;")
        );
        assert!(
            observation
                .output
                .contains("Check the Guard executable and native sandbox prerequisites")
        );
        assert_eq!(
            observation.guard_stage.map(|fact| fact.stage),
            Some(FailureStage::Execution)
        );
    }
    let read = super::unstarted_observation(
        runtime.intent(
            crate::RunId::new(),
            crate::AgentRunId::new(),
            ToolCall::Read {
                path: "a".into(),
                offset: Some(0),
                limit: Some(10),
            },
        ),
        ToolError::NotFound,
    );
    assert_eq!(read.guard_stage, None);
    assert!(
        !serde_json::to_string(&read)
            .unwrap()
            .contains("guard_stage")
    );
}

#[test]
fn undispatched_resource_limit_remains_a_stopping_limit() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(temp.path(), config_value());
    let intent = runtime.intent(
        crate::RunId::new(),
        crate::AgentRunId::new(),
        ToolCall::Command {
            program: "script".into(),
            args: Vec::new(),
            cwd: ".".into(),
        },
    );
    let mut observation = super::unstarted_observation(intent, super::ToolError::Limit);
    assert_eq!(observation.disposition, ToolDisposition::Limit);
    assert_eq!(observation.limit_cause, Some(super::LimitCause::Resource));
    assert!(observation.attestation.is_none());
    assert!(observation.valid());
    assert!(observation.stops_run());
    assert!(observation.output.contains("resource limit"));
    assert!(observation.output.contains("no operation was dispatched"));
    observation.limit_cause = Some(super::LimitCause::RequestedTime);
    assert!(observation.valid());
    assert!(!observation.stops_run());
}

#[test]
fn compaction_readmission_accepts_a_recorded_locale() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
    super::CommandEnvironment::for_path("/usr/bin:/bin")
        .with_name("LANG", "C.UTF-8")
        .save(&state)
        .unwrap();
    let tools = state.path().join("tools.json");
    std::fs::write(
        &tools,
        br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
    )
    .unwrap();
    std::fs::set_permissions(&tools, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
    state
        .record(crate::store::RecordKind::Permissions)
        .replace(
            br#"{"hooks":{"PreCompact":[{"matcher":"manual|auto","hooks":[{"type":"command","command":"check"}]}]}}"#,
        )
        .unwrap();
    let snapshot = super::load_permissions(&state).unwrap().unwrap();
    let mut config = Config::load(&state).unwrap();
    config.discover_skills(&workspace).unwrap();
    config.bind_environment(&super::CommandEnvironment::load(&state).unwrap());
    assert_eq!(config.locale.len(), 1);
    let mut pin = config.receipt();
    pin.permissions_digest = Some(snapshot.digest);
    let runtime = super::ToolRuntime::readmit_compaction_hooks(
        &state,
        &workspace,
        Some(std::env::current_exe().unwrap()),
        &pin,
    )
    .unwrap();
    assert!(runtime.is_some());
}

#[test]
fn a_changed_environment_record_changes_the_process_policy_digest() {
    let digest = |path: &str| {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let state = crate::store::StateRoot::admit(&temp.path().join("state")).unwrap();
        super::CommandEnvironment::for_path(path)
            .save(&state)
            .unwrap();
        let tools = state.path().join("tools.json");
        std::fs::write(
            &tools,
            br#"{"version":1,"workspace_paths":["."],"write":false,"commands":[],"skills":[],"mcp":[]}"#,
        )
        .unwrap();
        std::fs::set_permissions(&tools, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let config = Config::load(&state).unwrap();
        super::ToolRuntime::admit(
            config,
            None,
            None,
            &workspace,
            &state,
            std::env::current_exe().unwrap(),
        )
        .unwrap()
        .receipt()
        .config_digest
    };
    assert_eq!(digest("/usr/bin"), digest("/usr/bin"));
    assert_ne!(digest("/usr/bin"), digest("/usr/bin:/bin"));
}

#[test]
fn command_reservations_have_no_separate_selection_allowance() {
    let root = tempfile::tempdir().unwrap();
    let runtime = in_process_runtime(root.path(), config_value());
    let call = ToolCall::Command {
        program: "shell".into(),
        args: vec!["-c".into(), "true".into()],
        cwd: String::new(),
    };
    assert_eq!(
        runtime.reservation_overhead(&call),
        super::guard::RESERVATION_OVERHEAD
    );
}
