use super::*;

pub(super) struct CacheFailure {
    identity: crate::store::BuildCacheIdentity,
    entered: tokio::sync::oneshot::Receiver<()>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CacheFailure {
    pub(super) fn new(_state: &Path, workspace: &Path, intent: &EffectIntent) -> Self {
        let identity = crate::store::BuildCacheIdentity::new(
            workspace,
            intent.workspace_device,
            intent.workspace_inode,
        )
        .unwrap();
        #[cfg(target_os = "linux")]
        {
            let state = StateRoot::open_existing(_state).unwrap();
            let mut lease = state.begin_build_cache(identity.clone()).unwrap();
            // Initialize the directory before execution, keeping preparation separate from the fault.
            drop(lease.prepare_directory().unwrap());
        }
        let (entered, release) = FaultPlan::gate_guard_cache_publication(intent.id);
        Self {
            identity,
            entered,
            release: Some(release),
        }
    }

    pub(super) async fn inject(&mut self) {
        (&mut self.entered)
            .await
            .expect("editor command reached cache publication");
        // Preparation also syncs the root, so arm this fault only at publication.
        FaultPlan::fail_record_sync(self.identity.token().into(), "");
        self.release
            .take()
            .unwrap()
            .send(())
            .expect("cache publication is held until its fault is armed");
    }
}

pub(super) fn is_case(case: &str) -> bool {
    case.starts_with("command")
}

pub const HOST_CASE: &str = "command_host";

// The dependency-directory output lives and dies with the editor's workspace.
pub(super) fn call(outside: bool) -> ToolCall {
    ToolCall::Command {
        program: "build".into(),
        args: vec![
            "-c".into(),
            if outside {
                "printf 'edited 0' > notes/0; printf 'outside' > notes/1"
            } else {
                "printf 'edited 0' > notes/0; mkdir -p notes/target; printf build > notes/target/out"
            }
            .into(),
        ],
        cwd: ".".into(),
        resource_class: None,
        timeout_ms: None, additional_reads: None }
}

// An oversized output must refuse integration, whatever the editor's criterion says.
pub(super) fn case_call(case: &str) -> ToolCall {
    if case == HOST_CASE {
        return ToolCall::CommandUnguarded {
            program: "build".into(),
            args: vec!["-c".into(), "printf 'edited 0' > notes/0".into()],
            cwd: String::new(),
            justification: "Edit the leased note without a sandbox".into(),
            prefix_rule: None,
        };
    }
    if case != "command_large" {
        return call(case == "command_outside");
    }
    ToolCall::Command {
        program: "build".into(),
        args: vec![
            "-c".into(),
            "i=0; while [ $i -lt 1100 ]; do printf %1024s x; i=$((i+1)); done > notes/0".into(),
        ],
        cwd: ".".into(),
        resource_class: None,
        timeout_ms: None,
        additional_reads: None,
    }
}

pub(super) fn configure(state: &Path, case: &str) {
    if case == "command_auto" {
        crate::test_support::private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"defaultMode":"auto"}}"#,
        );
    }
    // Sandboxed commands ask nothing; an ask rule gives the fault its intent before dispatch.
    if case == "command_cache_unknown" {
        crate::test_support::private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"ask":["Bash"]}}"#,
        );
    }
    let shell = std::fs::canonicalize("/bin/sh").unwrap();
    crate::test_support::private_file(&state.join("tools.json"), &serde_json::to_vec(&serde_json::json!({
        "workspace_paths":["notes"],"write":true,"skills":[],"mcp":[],
        "commands":[{"name":"build","executable":shell,"sha256":crate::validate::hex_digest(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]}]
    })).unwrap());
}

impl LiveProvider {
    pub(super) fn command_outcome(
        &self,
        request: &ProviderRequest,
        step: usize,
    ) -> ProviderOutcome {
        if request.phase == AgentPhase::ChildWork {
            assert_editor_catalog(request);
        }
        match (request.phase, step) {
            (AgentPhase::RootPlan, 0) => super::super::scenario::plan_outcome(self.plan()),
            (AgentPhase::RootPlan, 1) => self.delegate(request),
            (AgentPhase::ChildWork, 0) => tool_outcome(case_call(self.case)),
            (AgentPhase::ChildWork, 1) if self.case == "command_conflict" => {
                // The command ran in the editor's workspace; the Workspace still has the base.
                assert_eq!(
                    std::fs::read(self.workspace.join("notes/0")).unwrap(),
                    b"original 0"
                );
                crate::test_support::private_file(&self.workspace.join("notes/0"), b"host");
                super::waves::read("notes/0")
            }
            (AgentPhase::ChildWork, 1) if self.case == "command_cancel" => {
                self.cancellation.cancel();
                finish_with("Cancelled after command publication")
            }
            (AgentPhase::ChildWork, 1) if self.case == "command_pause" => {
                assert!(self.steering.pause());
                finish_with("Paused after command publication")
            }
            (AgentPhase::ChildWork, _)
                if matches!(
                    self.case,
                    "command_criterion" | "command_bash_only" | "command_large"
                ) =>
            {
                finish_with("Command criterion passed")
            }
            (AgentPhase::ChildWork, 1) => super::waves::read("notes/0"),
            (AgentPhase::ChildWork, _) => finish_with("Command edits completed"),
            (AgentPhase::RootSynthesis, _) if request.input.child_results.is_empty() => {
                finish_with("Wait for editor command")
            }
            (AgentPhase::RootSynthesis, _)
                if matches!(
                    self.case,
                    "command_outside" | "command_conflict" | "command_large"
                ) =>
            {
                finish_with("The editor's integration was refused")
            }
            (AgentPhase::RootSynthesis, _) => {
                let calls: Vec<_> = request
                    .input
                    .tools
                    .as_ref()
                    .unwrap()
                    .steps
                    .iter()
                    .flat_map(|step| &step.observations)
                    .map(|o| &o.intent.call)
                    .collect();
                if matches!(self.case, "command_criterion" | "command_bash_only")
                    && !calls.iter().any(|c| matches!(c, ToolCall::Command { .. }))
                {
                    return tool_outcome(call(false));
                }
                if !calls
                    .iter()
                    .any(|c| matches!(c, ToolCall::Read { path, .. } if path == "notes/0"))
                {
                    return super::waves::read("notes/0");
                }
                let report = &request.input.child_results[0].result;
                assert!(report.contains("Changed paths (untrusted"));
                assert!(report.contains(&crate::validate::hex_digest(b"edited 0")));
                finish_with("Primary verified command edits")
            }
            (AgentPhase::ToolReview, _) => ProviderOutcome::Review(ReviewVerdict::Approve),
            (AgentPhase::RootPlan, _) => panic!("unexpected command planning call"),
        }
    }
}

fn assert_editor_catalog(request: &ProviderRequest) {
    let catalog: serde_json::Value =
        serde_json::from_str(request.input.tools.as_ref().unwrap().catalog()).unwrap();
    if catalog["host_commands"] == true {
        let operations = catalog["operations"].as_array().unwrap();
        assert!(operations.contains(&serde_json::json!("command_unguarded")));
        assert!(!operations.contains(&serde_json::json!("command")));
        assert!(catalog["sandbox"].is_string() && catalog.get("command_contract").is_none());
        return;
    }
    let contract = catalog["command_contract"].as_str().unwrap();
    for required in [
        "run directly in your private workspace",
        "integrates your project changes once",
        "must fit your own lease",
        "permission checks and checkpoints",
        "never integrate",
        "exclusive cache-identity lease",
        "busy refuses without waiting or retry",
        "cache_update reports cache publication separately from command success",
    ] {
        assert!(
            contract.contains(required),
            "missing editor command rule: {required}"
        );
    }
    for obsolete in [
        "private assignment image",
        "update that view",
        "handover",
        "Cache changes are discarded",
    ] {
        assert!(
            !contract.contains(obsolete),
            "obsolete editor command rule: {obsolete}"
        );
    }
}

pub(super) fn assert_commands(
    case: &str,
    workspace: &Path,
    events: &[EventEnvelope],
    view: &SessionView,
) {
    if !is_case(case) {
        return;
    }
    let run = &view.runs[0];
    let editor = &run.agents[1];
    let command = &editor.tools[0];
    if case == HOST_CASE {
        assert_host_command(workspace, view);
        return;
    }
    assert!(matches!(command.intent.call, ToolCall::Command { .. }));
    assert!(command.intent.target.editor());
    let observation = command.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(matches!(
        observation.attestation,
        Some(Attestation::Guard(_))
    ));
    let result: crate::tools::types::CommandResult =
        serde_json::from_str(&observation.output).unwrap();
    assert!(
        result.changeset.files.is_empty(),
        "nothing publishes per command"
    );
    assert!(command.checkpoint.is_none());
    assert!(command.intent.limits.cache_bytes > 0);
    let integration = editor
        .tools
        .iter()
        .find(|tool| crate::session::editor_integration(&tool.intent.call));
    let outcome = editor.assignment.as_ref().unwrap().outcome.as_ref();
    if case == "command_cache_unknown" {
        assert_eq!(observation.cache_update, CacheUpdate::Uncertain);
        assert_eq!(editor.tools.len(), 1);
        assert!(editor.assignment.as_ref().unwrap().released.is_none());
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            b"original 0"
        );
        assert_ne!(run.status, RunStatus::Finished);
        assert!(!view.goals[0].can_resume(view));
        return;
    }
    assert_eq!(observation.cache_update, CacheUpdate::Published);
    assert!(!workspace.join("notes/target").exists());
    if case == "command_large" {
        assert!(integration.is_none());
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            b"original 0"
        );
        assert_eq!(
            outcome.unwrap().1.cause,
            crate::worker::WorkerCause::ChangesTooLarge
        );
        // The primary reads which file blocked integration.
        let report = &run.worker_history()[0].report;
        assert!(report.contains("notes/0") && report.contains("Nothing was integrated"));
        assert_eq!(run.status, RunStatus::Failed);
        return;
    }
    if matches!(case, "command_outside" | "command_conflict") {
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            if case == "command_conflict" {
                b"host".as_slice()
            } else {
                b"original 0"
            }
        );
        assert_eq!(
            std::fs::read(workspace.join("notes/1")).unwrap(),
            b"original 1"
        );
        assert_eq!(
            outcome.unwrap().1.cause,
            crate::worker::WorkerCause::IntegrationConflict
        );
        let report = &run.worker_history()[0].report;
        assert!(report.contains("notes/") && report.contains("Nothing was integrated"));
        if case == "command_conflict" {
            let refused = integration.unwrap().observation.as_ref().unwrap();
            assert_eq!(refused.disposition, ToolDisposition::Conflict);
            assert!(refused.output.contains("notes/0"));
        } else {
            assert!(integration.is_none());
        }
        assert!(run.worker_history()[0].changed_paths.is_empty());
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, Event::ToolPreflightRefused { .. }))
        );
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.failure, Some(RunFailure::CriterionUnmet));
        assert_eq!(view.goals[0].status, GoalStatus::Failed);
        return;
    }
    let history = &run.worker_history()[0];
    assert!(history.lease_released);
    if matches!(case, "command_cancel" | "command_pause") {
        assert!(integration.is_none());
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            b"original 0"
        );
        assert!(!history.verified);
        assert!(history.changed_paths.is_empty());
        assert_ne!(run.status, RunStatus::Finished);
        if case == "command_pause" {
            assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
        }
        return;
    }
    assert_eq!(
        std::fs::read(workspace.join("notes/0")).unwrap(),
        b"edited 0"
    );
    let publication = integration.unwrap();
    assert!(publication.checkpoint.is_some());
    assert_eq!(publication.intent.agent_run_id, editor.id);
    assert_eq!(publication.intent.target, EffectTarget::Workspace);
    assert_eq!(
        publication.observation.as_ref().unwrap().disposition,
        ToolDisposition::Succeeded
    );
    if case == "command_auto" {
        for tool in [command, publication] {
            assert!(
                tool.permission_verdict.as_ref().is_none_or(
                    |verdict| verdict.source == crate::permissions::PermissionSource::Mode
                )
            );
            assert!(tool.review_call_index.is_none());
        }
    }
    if case == "command_bash_only" {
        let grant = &editor.assignment.as_ref().unwrap().pins.grant;
        assert_eq!(grant.tools, [crate::worker::WorkerTool::Command]);
        assert!(editor.tools.iter().all(|tool| matches!(
            tool.intent.call,
            ToolCall::Command { .. } | ToolCall::ApplyChangeset { .. }
        )));
    }
    assert!(
        history
            .changed_paths
            .contains(&crate::validate::hex_digest(b"edited 0"))
    );
    assert_eq!(run.status, RunStatus::Finished);
    assert!(history.verified);
    assert_eq!(view.goals[0].status, GoalStatus::Completed);
}

fn assert_host_command(workspace: &Path, view: &SessionView) {
    let run = &view.runs[0];
    assert_eq!(run.status, RunStatus::Finished);
    let editor = &run.agents[1];
    assert_eq!(
        editor.grant,
        Some(crate::session::ChildGrant::EditOnly {
            unguarded: crate::tools::UnguardedGrant::HumanOnly
        })
    );
    let command = &editor.tools[0];
    assert!(matches!(
        command.intent.call,
        ToolCall::CommandUnguarded { .. }
    ));
    assert!(command.intent.target.editor());
    let pin = command.intent.host_command.as_ref().unwrap();
    assert!(!std::path::Path::new(&pin.workspace).starts_with(workspace));
    let verdict = command.permission_verdict.as_ref().unwrap();
    assert_eq!(verdict.source, crate::permissions::PermissionSource::Human);
    let host = verdict.host.as_ref().unwrap();
    assert!(host.rule.is_none() && !host.session && host.source_intent.is_none());
    let observation = command.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(matches!(
        observation.attestation,
        Some(Attestation::Unguarded(_))
    ));
    assert!(
        editor
            .tools
            .iter()
            .any(|tool| crate::session::editor_integration(&tool.intent.call))
    );
    assert_eq!(
        std::fs::read(workspace.join("notes/0")).unwrap(),
        b"edited 0"
    );
}
