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
            offset: 0,
            limit: 128,
        }
    }

    fn plan(&self) -> GoalPlanContract {
        GoalPlanContract {
            mission: "Publish and verify independent edits".into(),
            paths: vec!["notes".into()],
            commands: if self.case == "command" {
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
            serde_json::from_str(&request.input.tools.as_ref().unwrap().catalog).unwrap();
        let digest = serde_json::from_value(catalog["agents"][0]["digest"].clone()).unwrap();
        let count = if matches!(self.case, "cap" | "hard_cap") {
            8
        } else if matches!(self.case, "waves" | "reassign") {
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
                                | "cancel_background"
                        ) {
                            WorkerMode::Background
                        } else {
                            WorkerMode::Foreground
                        },
                        max_turns: 17,
                        wave,
                        resumes: None,
                        supersedes,
                    })),
                }
            })
            .collect::<Vec<_>>();
        if matches!(self.case, "mixed" | "c1_failure" | "cancel_mixed") {
            children.push(ChildAssignment {
                objective: "researcher".into(),
                profile: None,
                editor: None,
            });
        }
        ProviderOutcome::Delegate(Delegate {
            agent: (self.case == "mixed").then(|| "researcher".into()),
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
        if matches!(self.case, "failure" | "reassign")
            && index == 0
            && request.input.objective.ends_with("wave 1")
        {
            self.ready.send(index).await.unwrap();
            let permit = self.gates[index].acquire().await.unwrap();
            permit.forget();
            return Err(ProviderError::InvalidOutcome);
        }
        if self.case == "waves" && index == 1 && step == 0 {
            self.check(
                std::fs::read(self.workspace.join("notes/0")).unwrap() == b"edited 0",
                "later wave captured before integration",
            );
            return Ok(tool_outcome(Self::read("notes/0"), None));
        }
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
            return Ok(tool_outcome(
                ToolCall::Write {
                    path: format!("notes/{index}"),
                    expected_digest: Some(crate::tools::types::hex_digest(
                        format!("original {index}").as_bytes(),
                    )),
                    content: format!("edited {index}"),
                },
                None,
            ));
        }
        self.check(step == write_step + 1, "unexpected child continuation");
        self.check(
            std::fs::read(self.workspace.join(format!("notes/{index}"))).unwrap()
                == format!("original {index}").as_bytes(),
            "private edit reached Workspace early",
        );
        self.ready.send(index).await.unwrap();
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
            return if wave == 1 {
                tool_outcome(Self::read("notes/0"), Some(Box::new(self.plan())))
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
            serde_json::from_str::<serde_json::Value>(&tools.catalog).unwrap()["pending_workers"]
                .as_array()
                .is_some_and(|p| !p.is_empty());
        let mut stale = self.stale.lock().unwrap();
        if pending && prior != 1 {
            *stale = observations.len() + 1;
            return tool_outcome(Self::read("notes/0"), None);
        }
        observations.drain(..(*stale).min(observations.len()));
        drop(stale);
        if self.case == "defer_finish" && prior == 1 {
            return finish_with("Too early");
        }
        if matches!(
            self.case,
            "leased_edit" | "background" | "command" | "budget_calls"
        ) && prior == 1
        {
            return tool_outcome(
                if self.case == "command" {
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
                },
                None,
            );
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
                return tool_outcome(Self::read("notes/0"), None);
            }
            return self.delegation(request, 2);
        }
        let count = if matches!(self.case, "cap" | "hard_cap") {
            8
        } else if self.case == "reassign" {
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
                return tool_outcome(Self::read(&path), None);
            }
        }
        if matches!(self.case, "cap" | "hard_cap") && prior == 9 {
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
    let commands = if case == "command" {
        let executable = std::fs::canonicalize("/usr/bin/true").unwrap();
        serde_json::json!([{"name":"verify", "executable":executable, "sha256":crate::tools::types::hex_digest(&std::fs::read(&executable).unwrap()),"interpreter":false,"inputs":[]}])
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
    if case == "background" {
        std::fs::write(state.join("agents/editor.md"), "---\nname: editor\ndescription: Edit in the background\ntools: Read, Write\npermissionMode: acceptEdits\nisolation: worktree\nbackground: true\n---\nKeep edits private.\n").unwrap();
    }
    std::fs::set_permissions(
        state.join("agents/editor.md"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    if case == "mixed" {
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
    input.policy = CollaborationPolicy::Team {
        max_active_children: 8,
    };
    input.goal = GoalRequest::new(budget, GoalPhase::Implementation);
    let (approvals, mut inbox) = ToolApprovals::new(ApprovalMode::Request);
    let cancellation = RunCancellation::new();
    let mut bindings = AdmittedProviders::single(provider.clone());
    let fallback = if case == "fallback" {
        let mut first = super::scenario::ScriptedProvider::new(Vec::new())
            .with_profile("scripted", "first-child");
        first.identity.limits.concurrency = 8;
        *first.responses.lock().unwrap() = std::collections::VecDeque::from([
            Err(ProviderError::HttpServerUnavailable),
            Err(ProviderError::HttpServerUnavailable),
        ]);
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
            Some(approval) = inbox.next() => { approval.decide(true); },
            Some(index) = arrivals.recv() => {
                ready.push(index);
                if case == "tokens" || case == "waves" || case == "reassign" { provider.gates[index].add_permits(1); }
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
                    else if matches!(case, "background" | "command" | "leased_edit" | "defer_finish" | "budget_calls") { if primary_seen { for gate in &provider.gates { gate.add_permits(1); } } }
                    else { provider.gates[1].add_permits(1); if case == "mixed" { provider.gates[7].add_permits(1); } }
                }
            },
            Some(event) = handle.events.next() => if let Tapped::Committed(envelope) = event {
                match &envelope.event {
                    Event::WorkerIntegrationRecorded { record, .. } => {
                        integrated.push(record.assignment_id);
                        if integrated.len() == 1 && matches!(case, "order" | "conflict" | "mixed" | "fallback") { provider.gates[0].add_permits(1); }
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
    }.unwrap_or_else(|e| panic!("{case}: team Run: {e:?}"));
    drop(handle);
    assert!(result.stopped.is_none());
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
        assert_eq!(first.requests.lock().unwrap().len(), 2);
        assert!(first.responses.lock().unwrap().is_empty());
    }
    let session = result.run.session_id;
    engine.close().await.unwrap();
    let (events, view) = replayed(&state, session).await;
    assert_action_bounds(&events, false, false);
    let run = &view.runs[0];
    let failed = matches!(
        case,
        "failure" | "c1_failure" | "cap" | "hard_cap" | "budget_calls"
    );
    assert_eq!(
        run.status,
        if case.starts_with("cancel_") {
            RunStatus::Cancelled
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
    if matches!(case, "cap" | "hard_cap") {
        assert_eq!(run.agents.len(), 9, "total attempts differ from active cap");
    }
    if matches!(case, "leased_edit" | "defer_finish") {
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
    if matches!(case, "order" | "conflict" | "mixed" | "fallback") {
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
