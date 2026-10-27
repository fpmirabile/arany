mod background;
mod bounds;
mod commands;
mod waves;

use super::goals::{assert_action_bounds, finish_with, replayed};
use super::scenario::{request, tool_outcome};
use crate::worker::{EditorAssignment, EffectTarget, WorkerMode};
use crate::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TEAM_SIZE: u8 = 3;

pub(crate) const RETIRED_WORKER_KINDS: &[&str] = &[
    "WorkerDataChunk",
    "WorkerDataSealed",
    "WorkerHandoverRecorded",
    "WorkerIntegrationRecorded",
    "WorkerHandoverDiscarded",
];

pub struct TeamObservation {
    pub directory: tempfile::TempDir,
    pub session: SessionId,
    pub events: Vec<EventEnvelope>,
    pub view: SessionView,
}

pub const INVALID_EDITOR_CASES: &[(&str, ProposalRefusalCause)] = &[
    ("invalid_definition", ProposalRefusalCause::EditorDefinition),
    ("invalid_digest", ProposalRefusalCause::EditorDigest),
    ("invalid_commands", ProposalRefusalCause::EditorCommands),
    ("invalid_turns", ProposalRefusalCause::EditorTurns),
    ("invalid_lease", ProposalRefusalCause::EditorLease),
    ("invalid_criterion", ProposalRefusalCause::EditorCriterion),
    ("invalid_resume", ProposalRefusalCause::EditorResume),
];

pub fn invalidate_editor(request: &mut EditorAssignment, case: &str) {
    match case {
        "invalid_definition" => request.definition = "untrusted-definition-canary".into(),
        "invalid_digest" => request.definition_digest = [9; 32],
        "invalid_commands" => request.commands = vec!["unadmitted".into()],
        "invalid_turns" => request.max_turns = 17,
        "invalid_lease" => request.lease = vec!["outside".into()],
        "invalid_criterion" => {
            request.criterion = GoalSuccessCriterion::WorkspaceFileContent {
                path: "outside/unleased".into(),
                content: "done".into(),
            }
        }
        "invalid_resume" => request.resumes = Some(AgentRunId::new()),
        _ => panic!("unknown invalid editor scenario"),
    }
}

fn overflow_editor_catalog(assignment: &mut EditorAssignment) {
    assignment.lease = (0..64)
        .map(|index| {
            if index == 0 {
                "notes/0".into()
            } else {
                format!(
                    "notes/{index:02}-{}/{}.txt",
                    "directory".repeat(20),
                    "file".repeat(25)
                )
            }
        })
        .collect();
    assert!(
        crate::worker::ChildAssignment::Editor {
            assignment: Box::new(assignment.clone()),
        }
        .valid()
    );
}

struct LiveProvider {
    identity: ProviderIdentity,
    workspace: PathBuf,
    case: &'static str,
    source: Option<AgentRunId>,
    cancellation: RunCancellation,
    calls: Mutex<Vec<(AgentPhase, AgentRunId)>>,
    bound_gate: tokio::sync::mpsc::Sender<()>,
    background: background::Gates,
    steering: RunSteering,
}

#[derive(Clone, Copy)]
enum LiveStart {
    New {
        padding: Option<usize>,
    },
    Resume {
        session: SessionId,
        goal: GoalId,
        source: AgentRunId,
    },
}
impl LiveProvider {
    fn plan(&self) -> GoalPlanContract {
        GoalPlanContract {
            mission: "Edit and verify leased notes".into(),
            paths: vec!["notes".into()],
            commands: if commands::is_case(self.case) {
                vec!["build".into()]
            } else {
                Vec::new()
            },
            resource_class: ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: if matches!(self.case, "stale_goal" | "waves_stale" | "waves_fresh") {
                    "notes/1"
                } else {
                    "notes/0"
                }
                .into(),
                content: if self.case == "stale_goal" {
                    "original 1"
                } else if matches!(self.case, "waves_stale" | "waves_fresh") {
                    "edited 1"
                } else if self.case == "no_change" {
                    "original 0"
                } else {
                    "edited 0"
                }
                .into(),
            },
            budget: bounds::budget(self.case),
        }
    }
    fn delegate(&self, request: &ProviderRequest) -> ProviderOutcome {
        let catalog: serde_json::Value =
            serde_json::from_str(request.input.tools.as_ref().unwrap().catalog()).unwrap();
        let digest = serde_json::from_value(catalog["agents"][0]["digest"].clone()).unwrap();
        let mut assignment = EditorAssignment {
            definition: "editor".into(),
            definition_digest: digest,
            mission: "Edit two leased files".into(),
            direction: "Report after editing".into(),
            lease: vec!["notes".into()],
            commands: Vec::new(),
            criterion: if self.case == "stale_goal" {
                GoalSuccessCriterion::WorkspaceFileContent {
                    path: "notes/0".into(),
                    content: "edited 0".into(),
                }
            } else {
                self.plan().success_criterion
            },
            mode: WorkerMode::Foreground,
            max_turns: 17,
            wave: 1,
            resumes: self.source,
            supersedes: None,
        };
        if self.case.starts_with("invalid_") {
            invalidate_editor(&mut assignment, self.case);
        }
        if self.case == "catalog_overflow" {
            overflow_editor_catalog(&mut assignment);
        }
        if self.case == "background" {
            assignment.mode = WorkerMode::Background;
        }
        if commands::is_case(self.case) {
            assignment.mode = WorkerMode::Background;
            assignment.lease = vec!["notes/0".into()];
            assignment.commands = vec!["build".into()];
            if matches!(self.case, "command_criterion" | "command_bash_only") {
                assignment.criterion = GoalSuccessCriterion::ToolCallSucceeded {
                    call: Box::new(commands::call(false)),
                };
            }
        }
        ProviderOutcome::Delegate(Delegate {
            agent: None,
            children: vec![ChildAssignment {
                objective: assignment.mission.clone(),
                profile: None,
                editor: Some(Box::new(assignment)),
            }],
        })
    }
    fn child_outcome(&self, step: usize) -> ProviderOutcome {
        let step = if self.case == "discovery" {
            if step == 0 {
                return tool_outcome(ToolCall::List {
                    path: "notes".into(),
                });
            }
            step - 1
        } else {
            step
        };
        match step {
            0 if self.case == "cancel" => {
                self.cancellation.cancel();
                finish_with("Cancelled before editing")
            }
            0 if self.case == "no_change" => tool_outcome(ToolCall::Read {
                path: "notes/0".into(),
                offset: None,
                limit: Some(128),
            }),
            _ if self.case == "no_change" => finish_with("No change required"),
            0 | 1 => {
                if step == 1 && self.case != "no_channel" {
                    assert_eq!(
                        std::fs::read(self.workspace.join("notes/0")).unwrap(),
                        b"edited 0"
                    );
                    if self.case == "cancel_landed" {
                        self.cancellation.cancel();
                        return finish_with("Cancelled after editing");
                    }
                }
                let path = format!("notes/{step}");
                tool_outcome(ToolCall::Write {
                    expected_digest: Some(crate::validate::hex_digest(
                        &std::fs::read(self.workspace.join(&path)).unwrap(),
                    )),
                    path,
                    content: format!("edited {step}"),
                })
            }
            2 => tool_outcome(ToolCall::Read {
                path: "notes/0".into(),
                offset: None,
                limit: Some(128),
            }),
            _ => finish_with("Leased edits completed"),
        }
    }

    fn synthesis_outcome(&self, request: &ProviderRequest, step: usize) -> ProviderOutcome {
        let step = if self.case == "primary_after" {
            if step == 0 {
                return tool_outcome(ToolCall::Write {
                    path: "notes/1".into(),
                    content: "primary 1".into(),
                    expected_digest: Some(crate::validate::hex_digest(b"edited 1")),
                });
            }
            step - 1
        } else {
            step
        };
        match step {
            _ if self.case == "no_channel" => {
                assert_eq!(
                    std::fs::read(self.workspace.join("notes/0")).unwrap(),
                    b"original 0"
                );
                assert!(
                    request.input.child_results[0]
                        .result
                        .contains("channel_unavailable")
                );
                finish_with("Editor could not obtain approval")
            }
            0 => {
                assert_eq!(
                    std::fs::read(self.workspace.join("notes/0")).unwrap(),
                    if self.case == "no_change" {
                        b"original 0".as_slice()
                    } else {
                        b"edited 0".as_slice()
                    }
                );
                assert_eq!(request.input.child_results.len(), 1);
                tool_outcome(ToolCall::Read {
                    path: "notes/0".into(),
                    offset: None,
                    limit: Some(128),
                })
            }
            _ => finish_with("Primary verified the live edits"),
        }
    }
}
#[async_trait::async_trait]
impl Provider for LiveProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }
    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let step = {
            let mut calls = self.calls.lock().unwrap();
            assert!(calls.len() < 24, "bounded live-editor scenario");
            let step = calls
                .iter()
                .filter(|(phase, actor)| *phase == request.phase && *actor == request.agent_run_id)
                .count();
            calls.push((request.phase, request.agent_run_id));
            step
        };
        if request.phase == AgentPhase::ChildWork
            && step == 0
            && let Some(source) = self.source
        {
            assert!(
                request
                    .input
                    .assignment_context
                    .as_ref()
                    .unwrap()
                    .contains(&source.to_string())
            );
        }
        if bounds::is_timed(self.case) && request.phase == AgentPhase::ChildWork && step == 1 {
            self.bound_gate.send(()).await.unwrap();
            return std::future::pending().await;
        }
        let outcome = if self.case == "background" {
            self.background_outcome(&request, step).await
        } else if commands::is_case(self.case) {
            self.command_outcome(&request, step)
        } else if matches!(self.case, "waves_stale" | "waves_fresh") {
            self.wave_outcome(&request, step)
        } else {
            match (request.phase, step) {
                (AgentPhase::RootPlan, 0) if self.source.is_some() => self.delegate(&request),
                (AgentPhase::RootPlan, 0) => super::scenario::plan_outcome(self.plan()),
                (AgentPhase::RootPlan, 1) if self.case == "stale_goal" => {
                    tool_outcome(ToolCall::Read {
                        path: "notes/1".into(),
                        offset: None,
                        limit: Some(128),
                    })
                }
                (AgentPhase::RootPlan, 2) if self.case == "stale_goal" => self.delegate(&request),
                (AgentPhase::RootPlan, 1) => self.delegate(&request),
                (AgentPhase::RootPlan, _) => finish_with("Editor assignment refused"),
                (AgentPhase::ChildWork, _) => self.child_outcome(step),
                (AgentPhase::RootSynthesis, _) => self.synthesis_outcome(&request, step),
                (AgentPhase::ToolReview, _) => ProviderOutcome::Review(ReviewVerdict::Approve),
            }
        };
        Ok(ProviderResponse {
            outcome,
            turn: ProviderTurn::scripted(),
            response_id: None,
            usage: Usage {
                input_tokens: Some(2),
                output_tokens: Some(2),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

fn private_file(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

pub async fn run_case(guard: &Path, case: &'static str) -> TeamObservation {
    if case == "capacity" {
        bounds::capacity_case(guard).await
    } else {
        run_case_inner(guard, case, None).await
    }
}

async fn run_case_inner(
    guard: &Path,
    case: &'static str,
    padding: Option<usize>,
) -> TeamObservation {
    let _deadline = super::engine::deadline(case, Duration::from_secs(180));
    super::isolate_account_root();
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    for index in 0..2 {
        std::fs::write(
            workspace.join(format!("notes/{index}")),
            format!("original {index}"),
        )
        .unwrap();
    }
    let state = directory.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    private_file(&state.join("tools.json"), br#"{"version":1,"workspace_paths":["notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#);
    std::fs::create_dir(state.join("agents")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(state.join("agents"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let definition = if case == "invalid_turns" {
        "maxTurns: 1\n"
    } else {
        ""
    };
    let tools = if case == "command_bash_only" {
        "Bash"
    } else if commands::is_case(case) {
        "Read, Write, Bash"
    } else if case == "discovery" {
        "Read, Write, Glob"
    } else {
        "Read, Write"
    };
    private_file(&state.join("agents/editor.md"), format!("---\nname: editor\ndescription: Edit leased notes\ntools: {tools}\n{definition}isolation: worktree\n---\nEdit the live Workspace within the lease.\n").as_bytes());
    private_file(
        &state.join("permissions.json"),
        br#"{"permissions":{"defaultMode":"default"}}"#,
    );
    if case == "hooks" {
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        private_file(&state.join("tools.json"), &serde_json::to_vec(&serde_json::json!({
            "version":1,"workspace_paths":["notes"],"write":true,"skills":[],"mcp":[],
            "commands":[{"name":"sh","executable":shell,"sha256":crate::validate::hex_digest(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]}]
        })).unwrap());
        let hook = serde_json::json!({"type":"command","command":"exit 0","timeout":5});
        private_file(
            &state.join("permissions.json"),
            &serde_json::to_vec(&serde_json::json!({
                "permissions":{"defaultMode":"default"},"hooks":{
                    "PreToolUse":[{"matcher":"Read|Write","hooks":[hook.clone()]}],
                    "PostToolUse":[{"matcher":"Read|Write","hooks":[hook.clone()]}],
                    "SubagentStop":[{"matcher":"editor","hooks":[hook]}]
                }
            }))
            .unwrap(),
        );
    }
    if commands::is_case(case) {
        commands::configure(&state, case);
    }
    run_live(directory, root, guard, case, LiveStart::New { padding }).await
}

async fn run_live(
    directory: tempfile::TempDir,
    root: StateRoot,
    guard: &Path,
    case: &'static str,
    start: LiveStart,
) -> TeamObservation {
    let workspace = directory.path().join("workspace");
    let state = directory.path().join("state");
    let engine = super::engine::open(
        root,
        EngineConfig {
            guard_executable: Some(guard.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let cancellation = RunCancellation::new();
    let (bound_gate, mut clock_gate) = tokio::sync::mpsc::channel(bounds::GATE_CAPACITY);
    let mut identity = ProviderIdentity::new("openai", "test-model", Provenance::native());
    if case == "background" {
        identity.limits.concurrency = TEAM_SIZE;
    }
    if bounds::is_timed(case) {
        identity.limits.deadline = crate::engine::MAX_DELEGATION_DEADLINE;
    }
    let steering = RunSteering::new();
    let provider = Arc::new(LiveProvider {
        identity,
        workspace: workspace.clone(),
        case,
        source: match start {
            LiveStart::Resume { source, .. } => Some(source),
            LiveStart::New { .. } => None,
        },
        cancellation: cancellation.clone(),
        calls: Default::default(),
        bound_gate,
        background: background::Gates::default(),
        steering: steering.clone(),
    });
    let mut input = request(&workspace, "Edit leased notes");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: TEAM_SIZE,
    };
    input.goal = GoalRequest::new(bounds::budget(case), GoalPhase::Implementation);
    if let LiveStart::Resume { session, goal, .. } = start {
        input.session_id = Some(session);
        input.title = None;
        input.objective.clear();
        input.goal = GoalRequest::resume(goal, true);
    }
    let session = match input.session_id {
        Some(id) => id,
        None => engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap(),
    };
    input.session_id = Some(session);
    input.title = None;
    let engine = if let LiveStart::New {
        padding: Some(through),
    } = start
    {
        bounds::pad_session(engine, &state, guard, session, through).await
    } else {
        engine
    };
    let (approvals, mut inbox) = ToolApprovals::new(if case == "command_auto" {
        ApprovalMode::Auto
    } else {
        ApprovalMode::Request
    });
    let mut running = engine
        .submit(
            AdmittedProviders::single(provider.clone()),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: (case != "no_channel").then_some(approvals),
                cancellation,
                steering: Some(steering),
                ..Default::default()
            },
        )
        .unwrap()
        .result;
    let result = loop {
        tokio::select! {
            result = &mut running => break result,
            Some(()) = clock_gate.recv() => bounds::exhaust(case).await,
            Some(approval) = inbox.next() => {
                if case == "command_cache_unknown" && matches!(approval.intent.call, ToolCall::Command { .. }) {
                    let identity = crate::store::BuildCacheIdentity::new(&workspace, approval.intent.workspace_device, approval.intent.workspace_inode).unwrap();
                    FaultPlan::fail_commit_marker(&identity);
                }
                if case == "uncertain" && matches!(approval.intent.call, ToolCall::Write { .. }) {
                    FaultPlan::fail_file_parent_sync(approval.intent.id);
                }
                approval.decide(true);
            }
        }
    };
    drop(running);
    if case == "capacity" {
        assert!(
            matches!(&result, Err(EngineError::Store(StoreError::ReplayLimit))),
            "{result:?}"
        );
    }
    if case == "command_cache_unknown" {
        assert!(
            matches!(&result, Err(EngineError::Tool(ToolError::Uncertain))),
            "uncertain command cache must stop the Run with Tool(Uncertain)"
        );
    }
    let result = if matches!(case, "uncertain" | "capacity" | "command_cache_unknown") {
        result.ok()
    } else {
        Some(result.unwrap())
    };
    engine.close().await.unwrap();
    let (events, view) = replayed(&state, session).await;
    let run = view.runs.last().unwrap();
    if let Some(result) = result {
        assert_eq!(result.run.run.status, run.status);
    }
    if case == "uncertain" {
        assert_ne!(run.status, RunStatus::Finished);
        assert_eq!(run.agents[1].tools.len(), 1);
        assert_eq!(
            run.agents[1].tools[0]
                .observation
                .as_ref()
                .unwrap()
                .disposition,
            ToolDisposition::Uncertain
        );
        assert!(
            run.agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .released
                .is_none()
        );
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            b"edited 0"
        );
        assert_eq!(
            std::fs::read(workspace.join("notes/1")).unwrap(),
            b"original 1"
        );
    }
    if case == "cancel_landed" {
        assert_eq!(
            std::fs::read(workspace.join("notes/0")).unwrap(),
            b"edited 0"
        );
        assert!(run.agents[1].tools[0].checkpoint.is_some());
        assert!(run.worker_history()[0].changed_paths.contains("notes/0"));
        assert!(
            run.worker_history()[0]
                .changed_paths
                .contains(&crate::validate::hex_digest(b"edited 0"))
        );
        assert_eq!(run.status, RunStatus::Cancelled);
        let goal = view.goals.last().unwrap();
        assert_eq!(goal.status, GoalStatus::Interrupted);
        assert!(goal.report.is_none());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.event, Event::GoalFinished { .. }))
        );
        let tool = &run.agents[1].tools[0];
        let observation = tool.observation.as_ref().unwrap();
        assert_eq!(observation.disposition, ToolDisposition::Succeeded);
        assert!(observation.attested_for(&tool.intent));
        assert!(
            run.agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .released
                .is_some()
        );
    }
    assert_live_editor_evidence(case, session, &events, &view);
    bounds::assert_stopped(case, &events, &view, &provider);
    waves::assert_waves(case, &events, &view);
    background::assert_background(case, &events, &view);
    commands::assert_commands(case, &workspace, &events, &view);
    if case == "no_change" {
        let worker = run.agents[1].assignment.as_ref().unwrap();
        assert_eq!(
            worker.outcome.as_ref().unwrap().1.disposition,
            crate::worker::WorkerDisposition::NoChange
        );
        assert!(worker.verification.as_ref().unwrap().1.passed);
        assert_eq!(view.goals[0].status, GoalStatus::Completed);
    }
    if case == "no_channel" {
        assert!(
            run.agents[1]
                .tools
                .iter()
                .filter(|t| matches!(t.intent.call, ToolCall::Write { .. }))
                .all(|t| t.observation.as_ref().unwrap().attestation.is_none())
        );
        assert!(
            run.agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .released
                .is_some()
        );
        assert_ne!(view.goals[0].status, GoalStatus::Completed);
    }
    if case == "hooks" {
        let child = &run.agents[1];
        for tool in &child.tools {
            assert_eq!(tool.hooks.len(), 2);
            assert!(
                tool.hooks
                    .iter()
                    .all(|hook| hook.invocation.intent.agent_run_id == child.id
                        && hook.outcome.is_some())
            );
        }
        assert!(
            run.lifecycle_hooks
                .iter()
                .any(|hook| hook.invocation.intent.agent_run_id == child.id
                    && hook.outcome.is_some())
        );
    }
    if case == "catalog_overflow" || case.starts_with("invalid_") {
        assert_eq!(run.agents.len(), 1, "{case}: no child spawn");
        let expected = if case == "catalog_overflow" {
            ProposalRefusalCause::DelegateInvalid
        } else {
            INVALID_EDITOR_CASES
                .iter()
                .find_map(|(name, cause)| (*name == case).then_some(*cause))
                .expect("invalid editor scenario has a refusal cause")
        };
        assert!(
            events.iter().any(|e| matches!(&e.event,
                Event::ProviderCallRecorded { record, .. }
                    if record.refusal.as_ref().is_some_and(|refusal| refusal.cause == expected)
            )),
            "{case}: expected {expected:?}"
        );
    }
    if case == "catalog_overflow" {
        assert!(!events.iter().any(|event| matches!(
            &event.event,
            Event::ProviderCallRecorded { record, .. }
                if record.disposition == ProviderCallDisposition::Delegated
        )));
        assert!(
            provider
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(phase, _)| *phase != AgentPhase::ChildWork)
        );
    }
    assert!(
        !events
            .iter()
            .any(|e| RETIRED_WORKER_KINDS.contains(&e.event.kind()))
    );
    assert_action_bounds(&events, false, false);
    TeamObservation {
        directory,
        session,
        events,
        view,
    }
}

fn assert_live_editor_evidence(
    case: &str,
    session: SessionId,
    events: &[EventEnvelope],
    view: &SessionView,
) {
    let run = view.runs.last().unwrap();
    if matches!(case, "success" | "hooks" | "discovery" | "primary_after") {
        assert_eq!(run.status, RunStatus::Finished);
        let worker = run.agents[1].assignment.as_ref().unwrap();
        assert!(worker.released.is_some() && worker.verification.as_ref().unwrap().1.passed);
        assert_eq!(
            run.agents[1].tools.len(),
            if case == "discovery" { 4 } else { 3 }
        );
        assert!(run.agents[1].tools.iter().all(
            |tool| tool.observation.as_ref().unwrap().disposition == ToolDisposition::Succeeded
        ));
        if case == "discovery" {
            assert!(matches!(
                run.agents[1].tools[0]
                    .observation
                    .as_ref()
                    .unwrap()
                    .attestation,
                Some(Attestation::Guard(_))
            ));
        }
        assert!(
            run.agents[1]
                .tools
                .iter()
                .all(|t| t.intent.target == EffectTarget::Workspace)
        );
        let goal = view.goals.last().unwrap();
        assert_eq!(goal.status, GoalStatus::Completed);
        assert!(
            goal.report
                .as_ref()
                .unwrap()
                .changes
                .iter()
                .any(|line| line.contains("notes/0"))
        );
    }
    if case == "stale_goal" {
        assert!(
            run.agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .verification
                .as_ref()
                .unwrap()
                .1
                .passed
        );
        assert!(!view.goals[0].success_criterion_satisfied);
        assert_eq!(run.failure, Some(RunFailure::CriterionUnmet));
        assert_eq!(view.goals[0].status, GoalStatus::Failed);
        let before_edit = events.iter().position(|event| matches!(&event.event,
            Event::ToolProposed { agent_run_id, intent, .. }
                if *agent_run_id == run.agents[1].id && matches!(intent.call, ToolCall::Write { .. })
        )).unwrap();
        let prior = SessionView::replay_open(session, &events[..before_edit])
            .unwrap()
            .unwrap();
        assert!(prior.goals[0].success_criterion_satisfied);
    }
}

pub async fn resume_by_id(guard: &Path) -> TeamObservation {
    let original = run_case(guard, "cancel").await;
    let source = original.view.runs[0].agents[1].id;
    let state = original.directory.path().join("state");
    let start = LiveStart::Resume {
        session: original.session,
        goal: original.view.goals[0].id,
        source,
    };
    let observed = run_live(
        original.directory,
        StateRoot::open_existing(&state).unwrap(),
        guard,
        "success",
        start,
    )
    .await;
    assert_eq!(
        &observed.events[..original.events.len()],
        original.events.as_slice()
    );
    assert_eq!(observed.view.runs.len(), 2);
    let child = &observed.view.runs[1].agents[1];
    assert_ne!(child.id, source);
    assert_eq!(
        child.assignment.as_ref().unwrap().pins.request.resumes,
        Some(source)
    );
    observed
}
