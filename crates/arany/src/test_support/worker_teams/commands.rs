use super::*;

pub(super) fn is_case(case: &str) -> bool {
    case.starts_with("command")
}

pub(super) fn call(outside: bool) -> ToolCall {
    ToolCall::Command {
        program: "build".into(),
        args: vec![
            "-c".into(),
            if outside {
                "printf 'edited 0' > notes/0; printf 'outside' > notes/1"
            } else {
                "printf 'edited 0' > notes/0"
            }
            .into(),
        ],
        cwd: ".".into(),
    }
}

pub(super) fn configure(state: &Path, case: &str) {
    if case == "command_auto" {
        private_file(
            &state.join("permissions.json"),
            br#"{"permissions":{"defaultMode":"auto"}}"#,
        );
    }
    let shell = std::fs::canonicalize("/bin/sh").unwrap();
    private_file(&state.join("tools.json"), &serde_json::to_vec(&serde_json::json!({
        "version":1,"workspace_paths":["notes"],"write":true,"skills":[],"mcp":[],
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
            (AgentPhase::ChildWork, 0) => tool_outcome(call(self.case == "command_outside")),
            (AgentPhase::ChildWork, 1) if self.case == "command_cancel" => {
                self.cancellation.cancel();
                finish_with("Cancelled after command publication")
            }
            (AgentPhase::ChildWork, 1) if self.case == "command_pause" => {
                assert!(self.steering.pause());
                finish_with("Paused after command publication")
            }
            (AgentPhase::ChildWork, _)
                if matches!(self.case, "command_criterion" | "command_bash_only") =>
            {
                finish_with("Command criterion passed")
            }
            (AgentPhase::ChildWork, 1) => super::waves::read("notes/0"),
            (AgentPhase::ChildWork, _) => finish_with("Command edits completed"),
            (AgentPhase::RootSynthesis, _) if request.input.child_results.is_empty() => {
                finish_with("Wait for editor command")
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
    let contract = catalog["command_contract"].as_str().unwrap();
    for required in [
        "selected inputs from the current Workspace",
        "whole changeset must fit your own lease",
        "refuses the entire batch without Workspace publication",
        "child identity with permission checks and checkpoints",
        "successful applied Workspace receipt",
        "Subsequent commands select the current Workspace, including landed edits",
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
    assert!(matches!(command.intent.call, ToolCall::Command { .. }));
    assert_eq!(command.intent.target, EffectTarget::Workspace);
    let observation = command.observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Succeeded);
    assert!(matches!(
        observation.attestation,
        Some(Attestation::Guard(_))
    ));
    assert!(command.intent.limits.cache_bytes > 0);
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
    if case == "command_outside" {
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            b"original 0"
        );
        assert_eq!(
            std::fs::read(workspace.join("notes/1")).unwrap(),
            b"original 1"
        );
        assert_eq!(editor.tools.len(), 1);
        assert!(events.iter().any(|e| matches!(e.event, Event::ToolPreflightRefused { agent_run_id, .. } if agent_run_id == editor.id)));
        assert!(run.worker_history()[0].changed_paths.is_empty());
        return;
    }
    assert_eq!(
        std::fs::read(workspace.join("notes/0")).unwrap(),
        b"edited 0"
    );
    let publication = &editor.tools[1];
    assert!(matches!(
        publication.intent.call,
        ToolCall::ApplyChangeset { .. }
    ));
    assert!(publication.checkpoint.is_some());
    assert_eq!(publication.intent.agent_run_id, editor.id);
    assert_eq!(
        publication.observation.as_ref().unwrap().disposition,
        ToolDisposition::Succeeded
    );
    if case == "command_auto" {
        for tool in [command, publication] {
            assert_eq!(
                tool.permission_verdict.as_ref().unwrap().source,
                crate::permissions::PermissionSource::AiReview
            );
            assert!(tool.review_call_index.is_some());
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
    let history = &run.worker_history()[0];
    assert!(
        history
            .changed_paths
            .contains(&crate::validate::hex_digest(b"edited 0"))
    );
    assert!(history.lease_released);
    if matches!(case, "command_cancel" | "command_pause") {
        assert!(!history.verified);
        assert_ne!(run.status, RunStatus::Finished);
        assert!(history.changed_paths.contains("notes/0"));
        if let Some(report) = &view.goals[0].report {
            assert!(report.changes.iter().any(|line| line.contains("notes/0")));
        }
        if case == "command_pause" {
            assert_eq!(view.goals[0].status, GoalStatus::Interrupted);
        }
    } else {
        assert_eq!(run.status, RunStatus::Finished);
        assert!(history.verified);
        assert_eq!(view.goals[0].status, GoalStatus::Completed);
    }
}
