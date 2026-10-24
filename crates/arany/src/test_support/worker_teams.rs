use super::goals::{assert_action_bounds, finish_with, replayed};
use super::scenario::{request, tool_outcome};
use crate::worker::{EditorAssignment, WorkerIntegrationOutcome, WorkerMode};
use crate::*;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Semaphore, mpsc};

pub struct TeamObservation {
    pub directory: tempfile::TempDir,
    pub session: SessionId,
    pub events: Vec<EventEnvelope>,
    pub view: SessionView,
}

struct TeamProvider {
    identity: ProviderIdentity,
    case: &'static str,
    source: Option<AgentRunId>,
    workspace: PathBuf,
    budget: GoalBudget,
    requests: Mutex<Vec<ProviderRequest>>,
    violations: Mutex<Vec<&'static str>>,
    ready: mpsc::Sender<usize>,
    gates: [Semaphore; 8],
    active: AtomicUsize,
    maximum: AtomicUsize,
    stale: Mutex<usize>,
}

struct ActiveCall<'a>(&'a AtomicUsize);
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl TeamProvider {
    fn check(&self, condition: bool, reason: &'static str) {
        if !condition {
            self.violations.lock().unwrap().push(reason);
        }
    }

    fn read(path: &str) -> ToolCall {
        ToolCall::Read {
            path: path.into(),
            offset: None,
            limit: Some(128),
        }
    }

    fn plan(&self) -> GoalPlanContract {
        GoalPlanContract {
            mission: "Publish and verify independent edits".into(),
            paths: vec!["notes".into()],
            commands: if matches!(self.case, "command" | "worker_command") {
                vec!["verify".into()]
            } else {
                Vec::new()
            },
            resource_class: ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            budget: self.budget,
            success_criterion: GoalSuccessCriterion::WorkspaceFileContent {
                path: "notes/0".into(),
                content: "edited 0".into(),
            },
        }
    }

    fn delegation(&self, request: &ProviderRequest, wave: u8) -> ProviderOutcome {
        let catalog: serde_json::Value =
            serde_json::from_str(request.input.tools.as_ref().unwrap().catalog()).unwrap();
        let digest = serde_json::from_value(catalog["agents"][0]["digest"].clone()).unwrap();
        let count = if matches!(self.case, "cap" | "hard_cap") {
            8
        } else if matches!(self.case, "waves" | "reassign" | "resume_id") {
            1
        } else {
            2
        };
        let mut children = (0..count)
            .map(|index| {
                let number = if self.case == "waves" && wave == 2 {
                    1
                } else {
                    index
                };
                let supersedes = if self.case == "reassign" && wave == 2 {
                    self.requests
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|r| r.phase == AgentPhase::ChildWork)
                        .map(|r| r.agent_run_id)
                } else {
                    None
                };
                ChildAssignment {
                    objective: format!("editor {number} wave {wave}"),
                    profile: None,
                    editor: Some(Box::new(EditorAssignment {
                        commands: if self.case == "worker_command" {
                            vec!["verify".into()]
                        } else {
                            Vec::new()
                        },
                        definition: "editor".into(),
                        definition_digest: digest,
                        mission: format!("editor {number} wave {wave}"),
                        direction: "Make one private edit".into(),
                        lease: vec![if self.case == "waves" {
                            "notes".into()
                        } else {
                            format!("notes/{number}")
                        }],
                        criterion: GoalSuccessCriterion::WorkspaceFileContent {
                            path: format!("notes/{number}"),
                            content: format!("edited {number}"),
                        },
                        mode: if matches!(
                            self.case,
                            "background"
                                | "command"
                                | "leased_edit"
                                | "defer_finish"
                                | "budget_calls"
                                | "primary_time"
                                | "primary_denial"
                                | "cancel_background"
                        ) {
                            WorkerMode::Background
                        } else {
                            WorkerMode::Foreground
                        },
                        max_turns: if self.case == "turn_limit" { 1 } else { 17 },
                        wave,
                        resumes: self.source,
                        supersedes,
                    })),
                }
            })
            .collect::<Vec<_>>();
        if self.case.starts_with("hook_")
            || matches!(self.case, "mixed" | "c1_failure" | "cancel_mixed")
        {
            children.push(ChildAssignment {
                objective: "researcher".into(),
                profile: None,
                editor: None,
            });
        }
        ProviderOutcome::Delegate(Delegate {
            agent: (self.case == "mixed" || self.case.starts_with("hook_"))
                .then(|| "researcher".into()),
            children,
        })
    }

    async fn child(
        &self,
        request: &ProviderRequest,
        step: usize,
    ) -> Result<ProviderOutcome, ProviderError> {
        if request.input.objective == "researcher" {
            self.ready.send(7).await.unwrap();
            let permit = self.gates[7].acquire().await.unwrap();
            permit.forget();
            return if self.case == "c1_failure" {
                Err(ProviderError::InvalidOutcome)
            } else {
                Ok(finish_with("Research complete"))
            };
        }
        let index = request
            .input
            .objective
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let step = if self.case == "worker_command" {
            if step == 0 {
                return Ok(tool_outcome(ToolCall::Command {
                    program: "verify".into(),
                    args: Vec::new(),
                    cwd: ".".into(),
                }));
            }
            step - 1
        } else {
            step
        };
        if matches!(self.case, "failure" | "reassign")
            && index == 0
            && request.input.objective.ends_with("wave 1")
        {
            self.ready.send(index).await.unwrap();
            let permit = self.gates[index].acquire().await.unwrap();
            permit.forget();
            return Err(ProviderError::InvalidOutcome);
        }
        if self.case == "resume_id" && step > 0 {
            let requests = self.requests.lock().unwrap();
            let first = requests
                .iter()
                .find(|r| r.agent_run_id == request.agent_run_id)
                .unwrap();
            self.check(
                first.input.assignment_context == request.input.assignment_context,
                "historical source context drifted between calls",
            );
        }
        if self.case == "resume_id" && step == 0 {
            self.check(
                request.input.assignment_context.as_ref().is_some_and(|g| {
                    g.contains(&self.source.unwrap().to_string()) && g.contains("Historical source")
                }),
                "missing bounded source-ID context",
            );
            self.check(
                request.input.tools.as_ref().unwrap().steps.is_empty(),
                "historical private steps resurrected",
            );
        }
        if self.case == "waves" && index == 1 && step == 0 {
            self.check(
                std::fs::read(self.workspace.join("notes/0")).unwrap() == b"edited 0",
                "later wave captured before integration",
            );
            return Ok(tool_outcome(Self::read("notes/0")));
        }
        let original = if self.case == "resume_id" {
            "resume base".to_owned()
        } else {
            format!("original {index}")
        };
        let write_step = usize::from(self.case == "waves" && index == 1);
        if step == write_step {
            if write_step == 1 {
                self.check(
                    request
                        .input
                        .tools
                        .as_ref()
                        .unwrap()
                        .observations()
                        .last()
                        .unwrap()
                        .output
                        .contains("edited 0"),
                    "second wave read stale base",
                );
            }
            return Ok(tool_outcome(ToolCall::Write {
                path: format!("notes/{index}"),
                expected_digest: Some(crate::validate::hex_digest(original.as_bytes())),
                content: format!("edited {index}"),
            }));
        }
        self.check(step == write_step + 1, "unexpected child continuation");
        self.check(
            std::fs::read(self.workspace.join(format!("notes/{index}"))).unwrap()
                == original.as_bytes(),
            "private edit reached Workspace early",
        );
        self.ready.send(index).await.unwrap();
        if matches!(self.case, "primary_time" | "primary_denial") {
            self.gates[6].add_permits(1);
        }
        let permit = self.gates[index].acquire().await.unwrap();
        permit.forget();
        if self.case == "conflict" && index == 1 {
            std::fs::write(self.workspace.join("notes/1"), "host edit").unwrap();
        }
        Ok(finish_with("Private edit complete"))
    }

    fn primary(&self, request: &ProviderRequest) -> ProviderOutcome {
        let all = self.requests.lock().unwrap().clone();
        let wave = all
            .iter()
            .filter(|r| r.phase == AgentPhase::RootPlan)
            .count();
        if request.phase == AgentPhase::RootPlan {
            return if wave == 1 && self.case != "resume_id" {
                super::scenario::plan_outcome(self.plan())
            } else {
                self.delegation(request, 1)
            };
        }
        let prior = all
            .iter()
            .filter(|r| r.phase == AgentPhase::RootSynthesis)
            .count();
        let tools = request.input.tools.as_ref().unwrap();
        let boundary = tools
            .steps
            .iter()
            .rposition(|s| s.requests.is_empty())
            .unwrap_or(0);
        let mut observations: Vec<_> = tools.steps[boundary..]
            .iter()
            .flat_map(|s| &s.observations)
            .collect();
        let pending =
            serde_json::from_str::<serde_json::Value>(tools.catalog()).unwrap()["pending_workers"]
                .as_array()
                .is_some_and(|p| !p.is_empty());
        let mut stale = self.stale.lock().unwrap();
        if pending && prior != 1 {
            *stale = observations.len() + 1;
            return tool_outcome(Self::read("notes/0"));
        }
        observations.drain(..(*stale).min(observations.len()));
        drop(stale);
        if self.case == "defer_finish" && prior == 1 {
            return finish_with("Too early");
        }
        if matches!(
            self.case,
            "leased_edit"
                | "background"
                | "command"
                | "budget_calls"
                | "primary_time"
                | "primary_denial"
        ) && prior == 1
        {
            return tool_outcome(if self.case == "command" {
                ToolCall::Command {
                    program: "verify".into(),
                    args: Vec::new(),
                    cwd: ".".into(),
                }
            } else {
                ToolCall::Write {
                    path: if self.case == "leased_edit" {
                        "notes/0".into()
                    } else {
                        "notes/free".into()
                    },
                    expected_digest: None,
                    content: "primary".into(),
                }
            });
        }
        let delegations = all
            .iter()
            .filter(|r| r.phase == AgentPhase::ChildWork)
            .map(|r| r.input.objective.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if matches!(self.case, "waves" | "reassign")
            && !delegations.iter().any(|o| o.ends_with("wave 2"))
        {
            if self.case == "waves"
                && !observations.iter().any(|o| {
                    matches!(&o.intent.call, ToolCall::Read { path, .. } if path == "notes/0")
                        && o.output.contains("edited 0")
                })
            {
                return tool_outcome(Self::read("notes/0"));
            }
            return self.delegation(request, 2);
        }
        let count = if matches!(self.case, "cap" | "hard_cap") {
            8
        } else if matches!(self.case, "reassign" | "resume_id") {
            1
        } else {
            2
        };
        for index in 0..count {
            if self.case == "failure" && index == 0 || self.case == "conflict" && index == 1 {
                continue;
            }
            let path = format!("notes/{index}");
            if !observations.iter().any(|o| {
                matches!(&o.intent.call, ToolCall::Read { path: p, .. } if p == &path)
                    && o.output.contains(&format!("edited {index}"))
            }) {
                return tool_outcome(Self::read(&path));
            }
        }
        if matches!(self.case, "cap" | "hard_cap") && prior >= 9 {
            return self.delegation(request, 2);
        }
        finish_with("Primary verified team")
    }
}

#[async_trait::async_trait]
impl Provider for TeamProvider {
    fn identity(&self) -> &ProviderIdentity {
        &self.identity
    }
    async fn invoke(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        let _active = ActiveCall(&self.active);
        let step = {
            let mut requests = self.requests.lock().unwrap();
            assert!(requests.len() < 120, "bounded scripted call history");
            let step = requests
                .iter()
                .filter(|r| r.agent_run_id == request.agent_run_id)
                .count();
            requests.push(request.clone());
            step
        };
        if self.case == "primary_time" && request.phase == AgentPhase::RootSynthesis && step == 2 {
            self.gates[6].acquire_many(2).await.unwrap().forget();
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(3600)).await;
            // Resume before native Guard work: blocking tasks inhibit paused-clock auto-advance.
            tokio::time::resume();
        }
        if self.case == "primary_denial" && request.phase == AgentPhase::RootSynthesis && step == 2
        {
            self.gates[6].acquire_many(2).await.unwrap().forget();
        }
        let outcome = if request.phase == AgentPhase::ChildWork {
            self.child(&request, step).await?
        } else {
            self.primary(&request)
        };
        Ok(ProviderResponse {
            turn: ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: Usage {
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}

pub async fn run_case(guard: &Path, case: &'static str) -> TeamObservation {
    let _deadline = super::engine::deadline(case, std::time::Duration::from_secs(180));
    use std::os::unix::fs::PermissionsExt;
    super::isolate_account_root();
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    for index in 0..8 {
        std::fs::write(
            workspace.join(format!("notes/{index}")),
            format!("original {index}"),
        )
        .unwrap();
    }
    let state = directory.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    let commands = if case.starts_with("hook_") || case == "order_hooks" {
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        serde_json::json!([{"name":"sh", "executable":shell, "sha256":crate::validate::hex_digest(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]}])
    } else if matches!(case, "command" | "worker_command") {
        let executable = std::fs::canonicalize("/usr/bin/true").unwrap();
        serde_json::json!([{"name":"verify", "executable":executable, "sha256":crate::validate::hex_digest(&std::fs::read(&executable).unwrap()),"interpreter":false,"inputs":[]}])
    } else {
        serde_json::json!([])
    };
    std::fs::write(state.join("tools.json"), serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["notes"],"write":true,"commands":commands,"skills":[],"mcp":[]})).unwrap()).unwrap();
    std::fs::set_permissions(
        state.join("tools.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::create_dir(state.join("agents")).unwrap();
    std::fs::set_permissions(state.join("agents"), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(state.join("agents/editor.md"), "---\nname: editor\ndescription: Edit one leased file\ntools: Read, Write\npermissionMode: acceptEdits\nisolation: worktree\n---\nKeep edits private.\n").unwrap();
    if case == "worker_command" {
        std::fs::write(state.join("agents/editor.md"), "---\nname: editor\ndescription: Edit one leased file\ntools: Read, Write, Bash\npermissionMode: acceptEdits\nisolation: worktree\n---\nKeep edits private.\n").unwrap();
    }
    if case == "background" {
        std::fs::write(state.join("agents/editor.md"), "---\nname: editor\ndescription: Edit in the background\ntools: Read, Write\npermissionMode: acceptEdits\nisolation: worktree\nbackground: true\n---\nKeep edits private.\n").unwrap();
    }
    std::fs::set_permissions(
        state.join("agents/editor.md"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    if case.starts_with("hook_") || case == "order_hooks" {
        let command =
            |script: &str| serde_json::json!({"type":"command","command":script,"timeout":5});
        let hooks = if case == "order_hooks" {
            serde_json::json!({"PreToolUse":[{"matcher":"Write","hooks":[command("exit 0")]}],"PostToolUse":[{"matcher":"Write","hooks":[command("exit 0")]}]})
        } else if case == "hook_sibling_stop" {
            serde_json::json!({"SubagentStop":[{"matcher":"researcher","hooks":[command(r#"printf '%s' '{"continue":false,"stopReason":"halt"}'"#)]}]})
        } else {
            serde_json::json!({"PreToolUse":[{"matcher":"Write","hooks":[command("kill -9 $PPID")]}]})
        };
        std::fs::write(
            state.join("permissions.json"),
            serde_json::to_vec(&serde_json::json!({"hooks":hooks})).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(
            state.join("permissions.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    if case == "mixed" || case.starts_with("hook_") {
        std::fs::write(state.join("agents/researcher.md"), "---\nname: researcher\ndescription: Inspect only\ntools: Read\n---\nReturn a bounded report.\n").unwrap();
        std::fs::set_permissions(
            state.join("agents/researcher.md"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let engine = super::engine::open(
        root,
        EngineConfig {
            guard_executable: Some(guard.into()),
            worker_concurrency: (case == "hard_cap").then_some(8),
            ..Default::default()
        },
    )
    .unwrap();
    let budget = GoalBudget {
        max_provider_calls: Some(if case == "budget_calls" { 9 } else { 64 }),
        max_tokens: (case == "tokens").then_some(1024),
        ..Default::default()
    };
    let (ready, mut arrivals) = mpsc::channel(16);
    let mut identity = ProviderIdentity::new("scripted", "test-model", Provenance::native());
    identity.limits.concurrency = 8;
    let provider = Arc::new(TeamProvider {
        identity,
        case,
        source: (case == "resume_unknown").then(AgentRunId::new),
        workspace: workspace.clone(),
        budget,
        requests: Default::default(),
        violations: Default::default(),
        ready,
        gates: std::array::from_fn(|_| Semaphore::new(0)),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
        stale: Mutex::new(0),
    });
    let mut input = request(&workspace, "Edit independent notes");
    input.include_paths.clear();
    input.policy = if case == "primary_denial" {
        CollaborationPolicy::Auto {
            max_active_children: 3,
        }
    } else {
        CollaborationPolicy::Team {
            max_active_children: 8,
        }
    };
    input.goal = GoalRequest::new(budget, GoalPhase::Implementation);
    let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
    let cancellation = RunCancellation::new();
    if case == "fallback" {
        // The journaled retry delays stay; only the waits before each editor's switch end at once.
        let session = engine
            .create_session(workspace.clone(), None)
            .await
            .unwrap();
        crate::fault::FaultPlan::skip_retry_backoff(session);
        input.session_id = Some(session);
        input.title = None;
    }
    let mut bindings = AdmittedProviders::single(provider.clone());
    let fallback = if case == "fallback" {
        let mut first = super::scenario::ScriptedProvider::new(Vec::new())
            .with_profile("scripted", "first-child");
        first.identity.limits.concurrency = 8;
        // Each editor's first call and its three same-route retries.
        *first.responses.lock().unwrap() = (0..2 * (usize::from(crate::MAX_PROVIDER_RETRIES) + 1))
            .map(|_| Err(ProviderError::HttpServerUnavailable(None)))
            .collect();
        let first = Arc::new(first);
        bindings.roles.child = vec![first.clone(), provider.clone()];
        Some(first)
    } else {
        None
    };
    let mut handle = engine
        .submit(
            bindings,
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals),
                cancellation: cancellation.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    let mut ready = Vec::new();
    let mut integrated = Vec::new();
    let mut primary_seen = false;
    let result = loop {
        tokio::select! {
            result = &mut handle.result => break result,
            Some(approval) = inbox.next() => {
                let allow = !(case == "primary_denial" && matches!(&approval.intent.call, ToolCall::Write { path, .. } if path == "notes/free"));
                approval.decide(allow);
            },
            Some(index) = arrivals.recv() => {
                ready.push(index);
                if case.starts_with("hook_") { if index == 7 { provider.gates[7].add_permits(1); } }
                else if case == "tokens" || case == "waves" || case == "reassign" { provider.gates[index].add_permits(1); }
                else if matches!(case, "cap" | "hard_cap") {
                    let cap = if case == "hard_cap" { 8 } else { 4 };
                    if ready.len() == cap {
                        let requests = provider.requests.lock().unwrap();
                        assert_eq!(requests.iter().filter(|r| r.phase == AgentPhase::ChildWork).map(|r| r.agent_run_id).collect::<std::collections::HashSet<_>>().len(), cap);
                        for gate in &provider.gates { gate.add_permits(1); }
                    }
                } else if ready.len() == if matches!(case, "mixed" | "c1_failure" | "cancel_mixed") { 3 } else { 2 } {
                    if case.starts_with("cancel_") { cancellation.cancel(); }
                    else if case == "c1_failure" { provider.gates[7].add_permits(1); }
                    else if case == "failure" { provider.gates[0].add_permits(1); }
                    else if case == "primary_denial" {}
                    else if matches!(case, "background" | "command" | "leased_edit" | "defer_finish" | "budget_calls" | "primary_time") { if primary_seen { for gate in &provider.gates { gate.add_permits(1); } } }
                    else { provider.gates[1].add_permits(1); if case == "mixed" { provider.gates[7].add_permits(1); } }
                }
            },
            Some(event) = handle.events.next() => if let Tapped::Committed(envelope) = event {
                match &envelope.event {
                    Event::WorkerIntegrationRecorded { record, .. } => {
                        integrated.push(record.assignment_id);
                        if integrated.len() == 1 && matches!(case, "order" | "order_hooks" | "conflict" | "mixed" | "fallback" | "worker_command") { provider.gates[0].add_permits(1); }
                    }
                    Event::AgentFinished { worker: Some(outcome), .. } if outcome.disposition == crate::worker::WorkerDisposition::Failed && case == "failure" => provider.gates[1].add_permits(1),
                    Event::ProviderCallRecorded { record, .. } if matches!(case, "leased_edit" | "defer_finish") && record.phase == AgentPhase::RootSynthesis && record.disposition == ProviderCallDisposition::InvalidResponse => { primary_seen = true; for gate in &provider.gates { gate.add_permits(1); } }
                    Event::ToolFinished { observation, .. } if matches!(&observation.intent.call, ToolCall::Write { path, .. } if path == "notes/free") => { primary_seen = true; for gate in &provider.gates { gate.add_permits(1); } }
                    Event::ProviderCallRecorded { record, agent_run_id, .. } if case == "command" && record.phase == AgentPhase::RootSynthesis && record.disposition == ProviderCallDisposition::ToolRequested => {
                        assert_ne!(*agent_run_id, provider.requests.lock().unwrap().iter().find(|r| r.phase == AgentPhase::ChildWork).unwrap().agent_run_id);
                        primary_seen = true; for gate in &provider.gates { gate.add_permits(1); }
                    }
                    _ => {}
                }
            }
        }
    };
    if case == "hook_uncertain_sibling" {
        assert!(
            matches!(result, Err(EngineError::Tool(ToolError::Uncertain))),
            "{case}: {result:?}"
        );
        drop(handle);
        let session = provider.requests.lock().unwrap()[0].session_id;
        engine.close().await.unwrap();
        let (events, view) = replayed(&state, session).await;
        let run = &view.runs[0];
        assert!(run.agents.iter().all(|a| {
            a.assignment
                .as_ref()
                .is_none_or(|w| w.integration.is_none())
        }));
        return TeamObservation {
            directory,
            session,
            events,
            view,
        };
    }
    let result = result.unwrap_or_else(|e| panic!("{case}: team Run: {e:?}"));
    drop(handle);
    assert!(
        provider.violations.lock().unwrap().is_empty(),
        "{case}: {:?}",
        provider.violations.lock().unwrap()
    );
    assert_eq!(
        provider.active.load(Ordering::SeqCst),
        0,
        "all Provider futures drained"
    );
    if case == "tokens" {
        assert_eq!(provider.maximum.load(Ordering::SeqCst), 1);
    }
    if let Some(first) = fallback {
        assert_eq!(
            first.requests.lock().unwrap().len(),
            2 * (usize::from(crate::MAX_PROVIDER_RETRIES) + 1)
        );
        assert!(first.responses.lock().unwrap().is_empty());
    }
    let session = result.run.session_id;
    engine.close().await.unwrap();
    let (events, view) = replayed(&state, session).await;
    assert_action_bounds(&events, false, false);
    let run = &view.runs[0];
    assert_eq!(
        result.stopped,
        view.run_stop(run),
        "{case}: the live stop is the replayed projection"
    );
    let failed = matches!(
        case,
        "failure"
            | "c1_failure"
            | "cap"
            | "hard_cap"
            | "budget_calls"
            | "primary_time"
            | "turn_limit"
            | "resume_unknown"
    );
    assert_eq!(
        run.status,
        if case.starts_with("cancel_") {
            RunStatus::Cancelled
        } else if case == "hook_sibling_stop" {
            RunStatus::StoppedByHook
        } else if failed {
            RunStatus::Failed
        } else {
            RunStatus::Finished
        },
        "{case}"
    );
    assert!(
        run.agents
            .iter()
            .skip(1)
            .all(|a| a.status != AgentStatus::Active
                && a.assignment.as_ref().is_none_or(|w| w.released.is_some()))
    );
    if case == "primary_denial" {
        assert_primary_denial_preserves_workers(run, &workspace);
    }
    if case == "hook_sibling_stop" {
        let researcher = run.agents.last().unwrap();
        assert_eq!(researcher.status, AgentStatus::StoppedByHook);
        for editor in &run.agents[1..run.agents.len() - 1] {
            assert_eq!(editor.status, AgentStatus::Cancelled);
            assert!(editor.assignment.as_ref().unwrap().integration.is_none());
        }
    }
    if matches!(case, "failure" | "reassign" | "turn_limit") {
        let cause = if case == "turn_limit" {
            crate::worker::WorkerCause::TurnLimit
        } else {
            crate::worker::WorkerCause::InvalidOutcome
        };
        assert_eq!(
            run.agents[1]
                .assignment
                .as_ref()
                .unwrap()
                .outcome
                .as_ref()
                .unwrap()
                .1
                .cause,
            cause
        );
    }
    if matches!(case, "cap" | "hard_cap") {
        assert_eq!(run.agents.len(), 9, "total attempts differ from active cap");
    }
    if matches!(case, "leased_edit" | "defer_finish") {
        let reason = if case == "defer_finish" {
            ProviderFailureReason::WorkerAnswerDeferred
        } else {
            ProviderFailureReason::WorkerToolRefused
        };
        assert!(
            run.agents[0]
                .provider_calls
                .iter()
                .any(|c| c.failure_reason == Some(reason))
        );
        assert!(
            run.agents[0]
                .provider_calls
                .iter()
                .any(|c| c.disposition == ProviderCallDisposition::InvalidResponse)
        );
        assert_eq!(
            run.agents
                .iter()
                .skip(1)
                .filter(|a| a
                    .assignment
                    .as_ref()
                    .is_some_and(|w| w.integration.is_some()))
                .count(),
            2
        );
    }
    if case == "leased_edit" {
        assert!(
            !run.tools
                .iter()
                .any(|t| matches!(t.intent.call, ToolCall::Write { .. }))
        );
    }
    if case == "command" {
        let command = events.iter().find(|e| matches!(&e.event, Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::Command { .. }))).unwrap().sequence;
        assert!(
            events
                .iter()
                .filter(|e| matches!(e.event, Event::WorkerLeaseReleased { .. }))
                .all(|e| e.sequence < command)
        );
    }
    if case == "command" {
        assert!(events.iter().any(|e| matches!(&e.event, Event::WorkerVerificationRecorded { record, .. } if record.passed)));
    }
    if case == "worker_command" {
        let commands = events
            .iter()
            .filter_map(|e| match &e.event {
                Event::ToolFinished { observation, .. }
                    if matches!(observation.intent.call, ToolCall::Command { .. })
                        && observation.intent.agent_run_id != run.agents[0].id =>
                {
                    Some(observation)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), 2);
        for observation in commands {
            assert_eq!(observation.disposition, ToolDisposition::Succeeded);
            assert!(matches!(
                observation.attestation,
                Some(Attestation::WorkerCommand { .. })
            ));
            assert_eq!(observation.cache_update, CacheUpdate::Discarded);
            assert!(events.iter().any(|e| matches!(&e.event, Event::ToolPermissionDecided { verdict, .. } if verdict.intent_digest == observation.intent.digest())));
        }
    }
    if case == "conflict" {
        let outcomes = events
            .iter()
            .filter_map(|e| {
                if let Event::WorkerIntegrationRecorded { record, .. } = &e.event {
                    Some(record.outcome)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes,
            [
                WorkerIntegrationOutcome::Conflict,
                WorkerIntegrationOutcome::Integrated
            ]
        );
        assert_eq!(
            std::fs::read(workspace.join("notes/1")).unwrap(),
            b"host edit"
        );
    }
    if case == "order_hooks" {
        for worker in run.agents.iter().skip(1) {
            let writes: Vec<_> = worker
                .tools
                .iter()
                .filter(|tool| matches!(tool.intent.call, ToolCall::Write { .. }))
                .collect();
            assert_eq!(writes.len(), 1, "each worker performs one Write");
            let tool = writes[0];
            assert_eq!(tool.hooks.len(), 2, "worker Write runs both hooks");
            assert_eq!(
                tool.hooks[0].invocation.event,
                crate::permissions::HookEvent::PreToolUse
            );
            assert_eq!(
                tool.hooks[1].invocation.event,
                crate::permissions::HookEvent::PostToolUse
            );
            assert!(tool.hooks.iter().all(|hook| {
                matches!(
                    hook.outcome.as_ref().map(|outcome| &outcome.result),
                    Some(crate::tools::permissions::HookResult::Passed)
                )
            }));
        }
    }
    if matches!(
        case,
        "order" | "order_hooks" | "conflict" | "mixed" | "fallback" | "worker_command"
    ) {
        let ordinals = integrated
            .iter()
            .map(|id| run.agents.iter().find(|a| a.id == *id).unwrap().ordinal)
            .collect::<Vec<_>>();
        assert_eq!(ordinals, [2, 1], "first completion integrates first");
    }
    TeamObservation {
        directory,
        session,
        events,
        view,
    }
}

fn assert_primary_denial_preserves_workers(run: &RunView, workspace: &Path) {
    assert_eq!(run.agents.len(), 3, "primary and two in-flight editors");
    assert_eq!(run.failure, None);
    let denied: Vec<_> = run.tools.iter().filter(|tool| {
        matches!(&tool.intent.call, ToolCall::Write { path, .. } if path == "notes/free")
    }).collect();
    assert_eq!(denied.len(), 1, "the declined write is never retried");
    let observation = denied[0].observation.as_ref().unwrap();
    assert_eq!(observation.disposition, ToolDisposition::Denied);
    assert_eq!(observation.approval_cause, Some(ApprovalCause::Declined));
    assert!(observation.attestation.is_none());
    assert_eq!(
        denied[0].model_denial_output(),
        Some("user_declined: no operation was dispatched")
    );
    assert!(!workspace.join("notes/free").exists());
    for (index, editor) in run.agents.iter().skip(1).enumerate() {
        let worker = editor.assignment.as_ref().unwrap();
        let outcome = &worker.outcome.as_ref().unwrap().1;
        assert_eq!(outcome.disposition, crate::worker::WorkerDisposition::Ready);
        assert_eq!(outcome.cause, crate::worker::WorkerCause::Finished);
        assert_eq!(
            worker.integration.as_ref().unwrap().1.outcome,
            WorkerIntegrationOutcome::Integrated
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join(format!("notes/{index}"))).unwrap(),
            format!("edited {index}")
        );
    }
}

pub async fn resume_by_id(guard: &Path) -> TeamObservation {
    tokio::time::timeout(
        std::time::Duration::from_secs(360),
        Box::pin(resume_case(guard, None)),
    )
    .await
    .expect("bounded worker ID resume")
}

pub async fn recover_by_id(guard: &Path, checkpoint: &'static str) -> TeamObservation {
    tokio::time::timeout(
        std::time::Duration::from_secs(360),
        Box::pin(resume_case(guard, Some(checkpoint))),
    )
    .await
    .expect("bounded worker recovery")
}

async fn resume_case(guard: &Path, checkpoint: Option<&'static str>) -> TeamObservation {
    let mut original = Box::pin(run_case(
        guard,
        match checkpoint {
            Some("child_hook") => "hook_sibling_stop",
            Some("worker_hook") => "order_hooks",
            Some(_) => "order",
            None => "cancel_foreground",
        },
    ))
    .await;
    let state = original.directory.path().join(if checkpoint.is_some() {
        "recovery-state"
    } else {
        "state"
    });
    if let Some(checkpoint) = checkpoint {
        let root = StateRoot::admit(&state).unwrap();
        let prior = original.directory.path().join("state");
        std::fs::copy(prior.join("tools.json"), state.join("tools.json")).unwrap();
        std::fs::create_dir(state.join("agents")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(state.join("agents"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        std::fs::copy(
            prior.join("agents/editor.md"),
            state.join("agents/editor.md"),
        )
        .unwrap();
        let last = original
            .events
            .iter()
            .position(|e| match (checkpoint, &e.event) {
                ("child_hook", Event::HookStarted { hook, .. }) => {
                    hook.event == crate::permissions::HookEvent::SubagentStop
                }
                ("worker_hook", Event::HookStarted { hook, .. }) => {
                    hook.event == crate::permissions::HookEvent::PreToolUse
                }
                ("private_intent" | "private_unknown", Event::ToolStarted { intent, .. }) => {
                    matches!(
                        intent.target,
                        crate::worker::EffectTarget::WorkerView { .. }
                    )
                }
                ("private_result", Event::ToolFinished { observation, .. }) => matches!(
                    observation.intent.target,
                    crate::worker::EffectTarget::WorkerView { .. }
                ),
                ("handover", Event::WorkerHandoverRecorded { .. }) => true,
                ("workspace_unknown", Event::ToolStarted { intent, .. }) => {
                    matches!(intent.call, ToolCall::ApplyChangeset { .. })
                }
                ("published", Event::ToolFinished { observation, .. }) => {
                    matches!(observation.intent.call, ToolCall::ApplyChangeset { .. })
                }
                _ => false,
            })
            .expect("named recovery checkpoint");
        let last = if checkpoint == "private_unknown"
            && matches!(
                original.events[last + 1].event,
                Event::ToolPermissionDecided { .. }
            ) {
            last + 1
        } else {
            last
        };
        let mut prefix: Vec<_> = original.events[..=last]
            .iter()
            .map(|e| e.event.clone())
            .collect();
        if checkpoint == "private_unknown" {
            let intent = prefix
                .iter()
                .rev()
                .find_map(|event| match event {
                    Event::ToolStarted { intent, .. } => Some(intent),
                    _ => None,
                })
                .unwrap();
            prefix.push(Event::ToolFinished {
                checkpoint: None,
                run_id: intent.run_id,
                agent_run_id: intent.agent_run_id,
                observation: ToolObservation {
                    intent: intent.clone(),
                    disposition: ToolDisposition::Uncertain,
                    output: "Private outcome unknown".into(),
                    attestation: None,
                    enforcement_cause: None,
                    limit_cause: None,
                    approval_cause: None,
                    cache_update: CacheUpdate::NotApplicable,
                },
            });
        }
        let store = Store::open(root).unwrap();
        for event in prefix {
            store.append(original.session, event).await.unwrap();
        }
        store.close().await.unwrap();
        let (events, view) = replayed(&state, original.session).await;
        original.events = events;
        original.view = view;
        for index in 0..2 {
            std::fs::write(
                original
                    .directory
                    .path()
                    .join(format!("workspace/notes/{index}")),
                format!("original {index}"),
            )
            .unwrap();
        }
    }
    let workspace = original.directory.path().join("workspace");
    let old = &original.view.runs[0].agents[1];
    let old_assignment = old.assignment.as_ref().unwrap();
    let source = old.id;
    let old_base = old_assignment.pins.base.sha256;
    let old_grant = old_assignment.pins.policy_digest;
    std::fs::write(workspace.join("notes/0"), b"resume base").unwrap();
    std::fs::write(state.join("tools.json"), br#"{"version":1,"workspace_paths":["docs","notes"],"write":true,"commands":[],"skills":[],"mcp":[]}"#).unwrap();
    if checkpoint.is_none() {
        std::fs::create_dir_all(workspace.join("docs")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let shell = std::fs::canonicalize("/bin/sh").unwrap();
        let command = serde_json::json!({"name":"sh","executable":shell,"sha256":crate::validate::hex_digest(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]});
        std::fs::write(state.join("tools.json"), serde_json::to_vec(&serde_json::json!({"version":1,"workspace_paths":["docs","notes"],"write":true,"commands":[command],"skills":[],"mcp":[]})).unwrap()).unwrap();
        std::fs::write(state.join("permissions.json"), br#"{"hooks":{"PreToolUse":[{"matcher":"Read|Write","hooks":[{"type":"command","command":"exit 0","timeout":5}]}],"PostToolUse":[{"matcher":"Read|Write","hooks":[{"type":"command","command":"exit 0","timeout":5}]}]}}"#).unwrap();
        std::fs::set_permissions(
            state.join("permissions.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    let engine = super::engine::open(
        StateRoot::open_existing(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(guard.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let (ready, _arrivals) = mpsc::channel(16);
    let provider = Arc::new(TeamProvider {
        identity: ProviderIdentity::new("scripted", "test-model", Provenance::native()),
        case: "resume_id",
        source: Some(source),
        workspace: workspace.clone(),
        budget: original.view.goals[0].budget,
        requests: Default::default(),
        violations: Default::default(),
        ready,
        gates: std::array::from_fn(|_| Semaphore::new(1)),
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
        stale: Mutex::new(0),
    });
    let mut input = request(&workspace, "");
    input.include_paths.clear();
    input.title = None;
    input.session_id = Some(original.session);
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    input.goal = GoalRequest::resume(original.view.goals[0].id, false);
    if matches!(
        checkpoint,
        Some("workspace_unknown" | "child_hook" | "worker_hook")
    ) {
        let result = engine
            .submit(
                AdmittedProviders::single(provider.clone()),
                input,
                RunControls {
                    tools: ToolGrant::Configured,
                    ..Default::default()
                },
            )
            .unwrap()
            .result
            .await;
        assert!(matches!(result, Err(EngineError::GoalUnavailable)));
        assert!(provider.requests.lock().unwrap().is_empty());
        engine.close().await.unwrap();
        let (events, view) = replayed(&state, original.session).await;
        assert_eq!(
            events, original.events,
            "refused resume never reruns a historical hook or effect"
        );
        if checkpoint != Some("workspace_unknown") {
            assert!(!view.runs[0].hooks_settled());
            assert_eq!(view.runs[0].status, RunStatus::Interrupted);
        }
        return TeamObservation {
            directory: original.directory,
            session: original.session,
            events,
            view,
        };
    }
    let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
    let mut running = engine
        .submit(
            AdmittedProviders::single(provider.clone()),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals),
                ..Default::default()
            },
        )
        .unwrap()
        .result;
    let result = loop {
        tokio::select! {
            result = &mut running => break result.unwrap(),
            Some(approval) = inbox.next() => { approval.decide(true); },
        }
    };
    drop(running);
    assert!(
        provider.violations.lock().unwrap().is_empty(),
        "resume violated source context or isolation"
    );
    engine.close().await.unwrap();
    let (events, view) = replayed(&state, original.session).await;
    assert_eq!(
        &events[..original.events.len()],
        original.events.as_slice(),
        "resume rewrote source history"
    );
    assert_eq!(view.runs.len(), 2);
    let resumed = &view.runs[1];
    assert_eq!(resumed.status, RunStatus::Finished);
    assert_eq!(result.stopped, view.run_stop(resumed));
    let worker = &resumed.agents[1];
    let assignment = worker.assignment.as_ref().unwrap();
    if checkpoint.is_none() {
        let tools: Vec<_> = worker
            .tools
            .iter()
            .filter(|tool| !tool.intent.call.is_internal())
            .collect();
        assert!(!tools.is_empty(), "resumed worker uses Tools");
        for tool in tools {
            assert_eq!(tool.hooks.len(), 2, "resumed worker runs both Tool hooks");
            assert_eq!(
                tool.hooks[0].invocation.event,
                crate::permissions::HookEvent::PreToolUse
            );
            assert_eq!(
                tool.hooks[1].invocation.event,
                crate::permissions::HookEvent::PostToolUse
            );
            assert!(
                tool.hooks
                    .iter()
                    .all(|hook| hook.invocation.intent.agent_run_id == worker.id
                        && hook.outcome.is_some())
            );
        }
    }
    assert_ne!(worker.id, source);
    assert_ne!(resumed.id, original.view.runs[0].id);
    assert_eq!(assignment.pins.request.resumes, Some(source));
    assert_ne!(assignment.pins.base.sha256, old_base);
    assert_ne!(assignment.pins.policy_digest, old_grant);
    assert_eq!(assignment.pins.grant.write_paths, ["notes/0"]);
    assert_eq!(events[original.events.len()..].iter().filter(|e| matches!(&e.event, Event::ToolStarted { intent, .. } if matches!(intent.call, ToolCall::ApplyChangeset { .. }))).count(), 1, "historical effect re-executed");
    assert_action_bounds(&events, false, false);
    TeamObservation {
        directory: original.directory,
        session: original.session,
        events,
        view,
    }
}

pub async fn runtime_command_case(guard: &Path, case: &'static str) -> TeamObservation {
    let _deadline = super::engine::deadline(case, std::time::Duration::from_secs(180));
    use super::goals::ForegroundWorkerProvider;
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).unwrap();
    std::fs::write(workspace.join("notes/a"), "original a").unwrap();
    std::fs::write(workspace.join("notes/b"), "original b").unwrap();
    std::fs::create_dir(workspace.join(".git")).unwrap();
    std::fs::write(workspace.join(".git/config"), "hostile git canary").unwrap();
    let state = directory.path().join("state");
    let root = StateRoot::admit(&state).unwrap();
    let private = |path: &Path, bytes: &[u8]| {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    let shell = std::fs::canonicalize("/bin/sh").unwrap();
    private(&state.join("tools.json"), &serde_json::to_vec(&serde_json::json!({
        "version":1,"workspace_paths":["notes"],"write":true,
        "commands":[{"name":"sh","executable":shell,"sha256":crate::validate::hex_digest(&std::fs::read(&shell).unwrap()),"interpreter":true,"inputs":[]}],
        "skills":[],"mcp":[],"child_unguarded":"denied"
    })).unwrap());
    std::fs::create_dir(state.join("agents")).unwrap();
    std::fs::set_permissions(state.join("agents"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mode = if case == "command_review_denied" {
        ""
    } else {
        "permissionMode: acceptEdits\n"
    };
    private(
        &state.join("agents/editor.md"),
        format!("---\nname: editor\ndescription: Build and test leased notes\ntools: Read, Write, Bash\n{mode}isolation: worktree\n---\nKeep commands private.\n").as_bytes(),
    );
    let hooks = serde_json::json!({
        "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"exit 0","timeout":5}]}],
        "PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo checked >&2; exit 2","timeout":5}]}]
    });
    let permissions = if case == "command_denied" {
        serde_json::json!({"permissions":{"deny":["Bash"]}})
    } else if case == "command_review_denied" {
        serde_json::json!({"permissions":{"defaultMode":"auto"},"hooks":hooks})
    } else if case == "command_hook_denied" {
        serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo blocked >&2; exit 2","timeout":5}]}]}})
    } else {
        serde_json::json!({"hooks":hooks})
    };
    private(
        &state.join("permissions.json"),
        &serde_json::to_vec(&permissions).unwrap(),
    );
    let engine = super::engine::open(
        root,
        EngineConfig {
            guard_executable: Some(guard.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let budget = GoalBudget {
        max_provider_calls: Some(48),
        max_active_ms: Some(30 * 60 * 1000),
        ..Default::default()
    };
    let provider = Arc::new(ForegroundWorkerProvider::new(&workspace, case, budget));
    let mut input = request(&workspace, "Build and verify in the worker view");
    input.include_paths.clear();
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    input.goal = GoalRequest::new(budget, GoalPhase::Implementation);
    let (approvals, mut inbox) = ToolApprovals::new(if case == "command_review_denied" {
        ApprovalMode::Auto
    } else {
        ApprovalMode::Request
    });
    let mut handle = engine
        .submit(
            AdmittedProviders::single(provider.clone()),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals),
                ..Default::default()
            },
        )
        .unwrap();
    let mut lost = false;
    let mut tail = std::collections::VecDeque::new();
    let result = loop {
        tokio::select! {
            result = &mut handle.result => match result {
                Ok(result) => break result,
                Err(error) => panic!("{case}: {error:?}; last committed Events {tail:?}"),
            },
            Some(crate::journal::Tapped::Committed(envelope)) = handle.events.next() => {
                if tail.len() == 8 {
                    tail.pop_front();
                }
                tail.push_back((envelope.sequence, envelope.event.kind()));
            }
            Some(approval) = inbox.next() => {
                assert!(!approval.intent.call.is_unguarded());
                if case == "command_loss" && matches!(approval.intent.target, crate::worker::EffectTarget::WorkerView { .. }) {
                    assert!(!lost);
                    crate::FaultPlan::lose_guard_receipt(approval.intent.id);
                    lost = true;
                }
                approval.decide(case != "command_review_denied");
            }
        }
    };
    drop(handle);
    assert!(
        provider.violations.lock().unwrap().is_empty(),
        "{case}: {:?}",
        provider.violations.lock().unwrap()
    );
    let session = result.run.session_id;
    engine.close().await.unwrap();
    let (events, view) = replayed(&state, session).await;
    let first_action = events
        .iter()
        .position(|e| {
            matches!(
                e.event,
                Event::ToolStarted { .. } | Event::GoalProviderCallReserved { .. }
            )
        })
        .expect("admitted action");
    for end in 1..=events.len() {
        SessionView::replay_open(session, &events[..end]).expect("valid interrupted prefix");
        if end > first_action {
            assert_action_bounds(&events[..end], false, false);
        }
    }
    let run = &view.runs[0];
    let child = &run.agents[1];
    let worker = child.assignment.as_ref().unwrap();
    assert_eq!(
        child.grant,
        Some(ChildGrant::EditOnly {
            unguarded: UnguardedGrant::Denied
        })
    );
    assert!(worker.released.is_some());
    let commands: Vec<_> = child
        .tools
        .iter()
        .filter(|t| matches!(t.intent.call, ToolCall::Command { .. }))
        .collect();
    assert!(commands.len() <= 16);
    assert!(child.provider_calls.len() <= 17);
    assert!(run.tools.iter().all(|t| !t.intent.call.is_unguarded()));
    assert!(child.tools.iter().all(|t| matches!(
        t.intent.target,
        crate::worker::EffectTarget::WorkerView { .. }
    )));
    assert_eq!(
        std::fs::read(workspace.join(".git/config")).unwrap(),
        b"hostile git canary"
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(view.goals[0].provider_calls as usize, requests.len());
    assert_eq!(view.goals[0].known_tokens, requests.len() as u64 * 4);
    assert!(view.goals[0].active_ms > 0);
    let rejected: Vec<_> = child
        .provider_calls
        .iter()
        .filter(|c| c.failure_reason == Some(ProviderFailureReason::CriterionNotMetInView))
        .collect();
    assert!(worker.gate_continuations <= 3);
    if case == "command_success" {
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(commands.len(), 3);
        assert_eq!(rejected.len(), 1);
        assert_eq!(worker.gate_continuations, 1);
        assert_eq!(worker.revision, 2);
        assert!(worker.verification.as_ref().unwrap().1.passed);
        let handover = &worker.handover.as_ref().unwrap().1;
        assert_eq!(
            handover.criterion_evidence,
            vec![commands.last().unwrap().finished_sequence.unwrap()]
        );
        for tool in commands {
            assert_eq!(tool.hooks.len(), 2);
            assert!(
                tool.hooks
                    .iter()
                    .all(|h| h.invocation.intent.agent_run_id == child.id)
            );
            assert_eq!(
                tool.observation.as_ref().unwrap().cache_update,
                CacheUpdate::Discarded
            );
        }
        assert!(requests.iter().any(|r| {
            r.phase == AgentPhase::ChildWork
                && r.input
                    .tools
                    .as_ref()
                    .unwrap()
                    .steps
                    .iter()
                    .any(|s| !s.hook_feedback.is_empty())
        }));
        assert_eq!(
            std::fs::read(workspace.join("notes/a")).unwrap(),
            b"edited a"
        );
    } else {
        assert!(worker.handover.is_none());
        assert_eq!(
            std::fs::read(workspace.join("notes/a")).unwrap(),
            b"original a"
        );
        let outcome = &worker.outcome.as_ref().unwrap().1;
        if case == "command_loss" {
            assert!(lost);
            assert_eq!(commands.len(), 1);
            assert_eq!(child.provider_calls.len(), 1);
            assert_eq!(
                outcome.disposition,
                crate::worker::WorkerDisposition::PrivateUncertain
            );
            assert_eq!(
                commands[0].observation.as_ref().unwrap().disposition,
                ToolDisposition::Uncertain
            );
        } else if case == "command_unguarded" {
            assert_eq!(commands.len(), 1);
            assert_eq!(outcome.cause, crate::worker::WorkerCause::InvalidOutcome);
        } else if case == "command_time" {
            assert!(run.workers.stop.is_some());
            assert_eq!(view.goals[0].status, GoalStatus::Failed);
        } else {
            assert_eq!(
                outcome.cause,
                if case == "command_review_denied" {
                    crate::worker::WorkerCause::ToolFailed
                } else {
                    crate::worker::WorkerCause::CriterionUnverified
                },
                "{case}: {} child calls, {} commands, {} gate rejections",
                child.provider_calls.len(),
                commands.len(),
                rejected.len()
            );
            if matches!(
                case,
                "command_nonzero"
                    | "command_rejected"
                    | "command_review_denied"
                    | "command_hook_denied"
            ) {
                assert_eq!(commands.len(), 1);
            }
            if case == "command_cap" {
                assert_eq!(rejected.len(), 4);
                assert_eq!(worker.gate_continuations, 3);
                let heads: Vec<_> = requests
                    .iter()
                    .filter(|r| r.phase == AgentPhase::ChildWork)
                    .map(|r| r.input.assignment_context.clone())
                    .collect();
                assert_eq!(heads.len(), 4);
                assert!(heads.windows(2).all(|w| w[0] == w[1]));
                assert!(
                    !heads[0]
                        .as_deref()
                        .unwrap_or_default()
                        .contains("Answer refused")
                );
                let feedback: Vec<_> = requests
                    .iter()
                    .filter(|r| r.phase == AgentPhase::ChildWork)
                    .map(|r| r.input.stop_hook_feedback.len())
                    .collect();
                assert_eq!(feedback, [0, 1, 2, 3]);
            }
            if matches!(case, "command_review_denied" | "command_hook_denied") {
                let observation = commands[0].observation.as_ref().unwrap();
                assert_eq!(observation.disposition, ToolDisposition::Denied);
                assert!(commands[0].permission_verdict.is_some());
                assert!(worker.verification.is_none());
            }
            if case == "command_review_denied" {
                assert!(requests.iter().any(|r| r.phase == AgentPhase::ToolReview));
                let verdict = commands[0].permission_verdict.as_ref().unwrap();
                assert!(!verdict.approved);
                assert_eq!(verdict.source, crate::permissions::PermissionSource::Human);
            }
            if case == "command_denied" {
                assert_eq!(commands.len(), 16);
                assert!(commands.iter().all(|t| t.permission_verdict.is_some()
                    && t.observation.as_ref().unwrap().disposition == ToolDisposition::Denied));
                assert_eq!(rejected.len(), 1);
                assert_eq!(worker.gate_continuations, 0);
            }
            if !matches!(case, "command_review_denied" | "command_hook_denied") {
                assert!(
                    run.supervisor
                        .verdicts()
                        .iter()
                        .any(|(actor, _)| *actor == child.id)
                );
            }
        }
    }
    TeamObservation {
        directory,
        session,
        events,
        view,
    }
}

pub async fn recover_open_worker_command(guard: &Path) -> TeamObservation {
    let _deadline = super::engine::deadline(
        "open worker command recovery",
        std::time::Duration::from_secs(360),
    );
    use super::goals::ForegroundWorkerProvider;
    use std::os::unix::fs::PermissionsExt;
    let mut original = Box::pin(runtime_command_case(guard, "command_success")).await;
    let prior = original.directory.path().join("state");
    let state = original.directory.path().join("recovery-state");
    let root = StateRoot::admit(&state).unwrap();
    for file in ["tools.json", "permissions.json", "agents/editor.md"] {
        if file.contains('/') {
            std::fs::create_dir_all(state.join("agents")).unwrap();
            std::fs::set_permissions(state.join("agents"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        std::fs::copy(prior.join(file), state.join(file)).unwrap();
    }
    let open = original
        .events
        .iter()
        .position(|e| {
            matches!(&e.event, Event::ToolStarted { intent, .. }
                if matches!(intent.call, ToolCall::Command { .. })
                    && matches!(intent.target, crate::worker::EffectTarget::WorkerView { .. }))
        })
        .expect("worker command intent");
    let intent = match &original.events[open].event {
        Event::ToolStarted { intent, .. } => intent.id,
        _ => unreachable!(),
    };
    let store = Store::open(root).unwrap();
    for event in &original.events[..=open] {
        store
            .append(original.session, event.event.clone())
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    let (prefix, _) = replayed(&state, original.session).await;
    let workspace = original.directory.path().join("workspace");
    std::fs::write(workspace.join("notes/a"), b"original a").unwrap();
    let engine = super::engine::open(
        StateRoot::open_existing(&state).unwrap(),
        EngineConfig {
            guard_executable: Some(guard.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let budget = original.view.goals[0].budget;
    let provider = Arc::new(ForegroundWorkerProvider::new(
        &workspace,
        "command_success",
        budget,
    ));
    let mut input = request(&workspace, "");
    input.include_paths.clear();
    input.title = None;
    input.session_id = Some(original.session);
    input.policy = CollaborationPolicy::Auto {
        max_active_children: 3,
    };
    input.goal = GoalRequest::resume(original.view.goals[0].id, false);
    let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
    let mut running = engine
        .submit(
            AdmittedProviders::single(provider.clone()),
            input,
            RunControls {
                tools: ToolGrant::Configured,
                approvals: Some(approvals),
                ..Default::default()
            },
        )
        .unwrap()
        .result;
    let result = loop {
        tokio::select! {
            result = &mut running => break result,
            Some(approval) = inbox.next() => { approval.decide(true); },
        }
    };
    drop(running);
    engine.close().await.unwrap();
    let (events, view) = replayed(&state, original.session).await;
    assert_eq!(
        &events[..prefix.len()],
        prefix.as_slice(),
        "recovery rewrote history"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.event, Event::ToolStarted { intent: i, .. } if i.id == intent))
            .count(),
        1,
        "the open worker command was re-executed"
    );
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(view.runs[0].status, RunStatus::Interrupted);
    assert!(
        view.runs[0]
            .agents
            .iter()
            .flat_map(|a| &a.tools)
            .find(|t| t.intent.id == intent)
            .is_some_and(|t| t.observation.is_none()),
        "recovery neither re-executes nor invents an outcome for the open command"
    );
    assert_eq!(
        std::fs::read(workspace.join(".git/config")).unwrap(),
        b"hostile git canary"
    );
    original.events = events.clone();
    original.view = view.clone();
    TeamObservation {
        directory: original.directory,
        session: original.session,
        events,
        view,
    }
}
