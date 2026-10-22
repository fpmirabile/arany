use crate::{AgentPhase, Answer, Event, ProviderOutcome, SessionView, StateRoot, Store};
use std::path::Path;

pub fn finish_with(text: &str) -> ProviderOutcome {
    ProviderOutcome::Answer(Answer { text: text.into() })
}

pub fn long_turn_provider() -> super::scenario::ScriptedProvider {
    super::scenario::ScriptedProvider::new(
        (0..64)
            .map(|_| {
                super::scenario::tool_outcome(crate::ToolCall::Read {
                    path: "notes/facts.txt".into(),
                    offset: None,
                    limit: None,
                })
            })
            .chain([finish_with("Long turn completed")])
            .collect(),
    )
    .with_profile("openai", "gpt-5.4")
    .with_known_input_tokens()
    .with_active_time_per_call(std::time::Duration::from_secs(29))
}

pub fn assert_long_turn(events: &[crate::EventEnvelope], view: &SessionView) {
    let goal = view.goals.last().expect("Goal");
    assert_eq!(goal.status, crate::GoalStatus::Completed);
    assert_eq!(goal.budget.max_provider_calls, None);
    assert_eq!(goal.budget.max_tokens, None);
    assert_eq!(goal.budget.max_active_ms, None);
    assert_eq!(goal.budget.max_runs, 16);
    assert_eq!(goal.provider_calls, 65);
    assert!(goal.active_ms >= 65 * 29_000);
    assert_eq!(goal.uncertain_active_ms, 0);
    assert!(!goal.token_usage_unknown);
    assert_eq!(goal.known_tokens, 65 * 8);
    assert_eq!(view.runs.last().expect("Run").tools.len(), 64);
    assert!(matches!(
        events.last().expect("receipt").event,
        Event::GoalFinished {
            exhausted: false,
            ..
        }
    ));
    assert_action_bounds(events, false, false);
}

pub fn facts_workspace(root: &Path, text: &str) -> std::path::PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("notes")).expect("workspace");
    std::fs::write(workspace.join("notes/facts.txt"), text).expect("facts file");
    workspace
}

pub async fn replayed(
    state_path: &Path,
    session_id: crate::SessionId,
) -> (Vec<crate::EventEnvelope>, SessionView) {
    let store = Store::open_read_only(StateRoot::open_existing(state_path).expect("state"))
        .expect("read-only Store");
    let events = store.load_session(session_id).await.expect("Events");
    let view = SessionView::replay(session_id, &events)
        .expect("strict replay")
        .expect("Session");
    store.close().await.expect("Store shutdown");
    (events, view)
}

/// Splits a journal at each action's first admission and asserts that the action's bound covers the
/// Events up to the next admission, or up to the end of the journal (the terminal Events).
pub fn assert_action_bounds(events: &[crate::EventEnvelope], previews: bool, steering: bool) {
    use crate::Action;
    let primaries: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                role: crate::AgentRole::Primary,
                ..
            } => Some(*agent_run_id),
            _ => None,
        })
        .collect();
    let mut call_phases = vec![None; events.len()];
    let mut next_phase = std::collections::HashMap::new();
    for (index, envelope) in events.iter().enumerate().rev() {
        match &envelope.event {
            Event::ProviderCallRecorded {
                agent_run_id,
                record,
                ..
            } => {
                next_phase.insert(*agent_run_id, record.phase);
            }
            Event::GoalProviderCallReserved { agent_run_id, .. } => {
                call_phases[index] = next_phase.get(agent_run_id).copied();
            }
            _ => {}
        }
    }
    let in_flight = |at: usize| -> u32 {
        let mut open: std::collections::HashMap<crate::AgentRunId, i32> = Default::default();
        for (index, envelope) in events[..at].iter().enumerate() {
            match &envelope.event {
                Event::GoalProviderCallReserved { agent_run_id, .. }
                    if !primaries.contains(agent_run_id)
                        && call_phases[index] != Some(AgentPhase::ToolReview) =>
                {
                    *open.entry(*agent_run_id).or_default() += 1;
                }
                Event::ProviderCallRecorded {
                    agent_run_id,
                    record,
                    ..
                } if record.phase != AgentPhase::ToolReview => {
                    if let Some(count) = open.get_mut(agent_run_id) {
                        *count -= 1;
                    }
                }
                _ => {}
            }
        }
        open.values().filter(|count| **count > 0).count() as u32
    };
    let researchers: std::collections::HashSet<_> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                grant: Some(crate::ChildGrant::ReadOnly { .. }),
                ..
            } => Some(*agent_run_id),
            _ => None,
        })
        .collect();
    let workers: std::collections::HashMap<_, _> = events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            Event::AgentSpawned {
                agent_run_id,
                assignment: Some(pins),
                ..
            } => Some((*agent_run_id, pins.data_bytes as usize)),
            _ => None,
        })
        .collect();
    let primary_call = |index: usize| {
        matches!(
            &events[index].event,
            Event::GoalProviderCallReserved { agent_run_id, .. } if primaries.contains(agent_run_id)
        )
    };
    // (index, action, explicit end)
    let mut points: Vec<(usize, Action, Option<usize>)> = Vec::new();
    let mut markers: Vec<usize> = Vec::new();
    for (index, envelope) in events.iter().enumerate() {
        let previous = index.checked_sub(1).map(|previous| &events[previous].event);
        match &envelope.event {
            Event::GoalStarted { .. }
            | Event::GoalResumed { .. }
            | Event::GoalRunLinked { .. }
            | Event::SessionStarted { .. } => markers.push(index),
            Event::GoalProviderCallReserved { agent_run_id, .. } => {
                let action = if call_phases[index] == Some(AgentPhase::ToolReview) {
                    Action::Review
                } else if primaries.contains(agent_run_id) {
                    Action::ProviderCall { previews, steering }
                } else if workers.contains_key(agent_run_id) {
                    Action::Worker { chunks: 0 }
                } else {
                    Action::Child {
                        in_flight: in_flight(index),
                        research: researchers.contains(agent_run_id),
                    }
                };
                points.push((index, action, None));
            }
            Event::AgentSpawned {
                assignment: Some(_),
                ..
            } => points.push((index, Action::Worker { chunks: 0 }, None)),
            Event::AgentSpawned { grant: Some(_), .. }
                if !matches!(previous, Some(Event::AgentSpawned { .. })) =>
            {
                let spawned: Vec<_> = events[index..]
                    .iter()
                    .map_while(|later| match &later.event {
                        Event::AgentSpawned { grant, .. } => Some(*grant),
                        _ => None,
                    })
                    .collect();
                // The delegation covers its children through the synthesis call or the Run's end.
                let end = (index + 1..events.len())
                    .find(|later| {
                        primary_call(*later)
                            || matches!(
                                events[*later].event,
                                Event::GoalRunLinked { .. } | Event::GoalResumed { .. }
                            )
                    })
                    .unwrap_or(events.len());
                points.push((
                    index,
                    Action::Delegation {
                        children: spawned.len(),
                        research: spawned
                            .iter()
                            .any(|grant| matches!(grant, Some(crate::ChildGrant::ReadOnly { .. }))),
                    },
                    Some(end),
                ));
            }
            // A switch or retry re-admits its attempt's action headroom before it commits.
            Event::ProviderFallbackUsed {
                agent_run_id,
                fallback: crate::ProviderFallback { role, .. },
                ..
            }
            | Event::ProviderRetryScheduled {
                agent_run_id,
                retry: crate::ProviderRetry { role, .. },
                ..
            } => {
                let action = match role {
                    crate::ProviderRole::Primary => Action::ProviderCall { previews, steering },
                    crate::ProviderRole::Child => Action::Child {
                        in_flight: in_flight(index),
                        research: researchers.contains(agent_run_id),
                    },
                    crate::ProviderRole::Review => Action::Review,
                    crate::ProviderRole::Compaction => Action::Compaction,
                };
                points.push((index, action, None));
            }
            Event::GoalCompactionCallReserved { .. } => {
                let between_runs = events[index..]
                    .iter()
                    .find_map(|later| match &later.event {
                        Event::ContextCompacted { .. } => Some(true),
                        Event::GoalContextCompacted { .. } => Some(false),
                        _ => None,
                    })
                    .unwrap_or(false);
                let action = if between_runs {
                    Action::BoundaryCompaction { goal: true }
                } else {
                    Action::Compaction
                };
                points.push((index, action, None));
            }
            Event::ContextCompacted { .. }
                if !matches!(previous, Some(Event::GoalCompactionCallReserved { .. })) =>
            {
                points.push((index, Action::BoundaryCompaction { goal: false }, None));
            }
            Event::GoalPlanContractRecorded { .. } => points.push((index, Action::Tool, None)),
            Event::WorkerStopObserved { .. } => {
                points.push((index, Action::Worker { chunks: 0 }, None));
            }
            Event::WorkerVerificationRecorded { .. }
                if !matches!(previous, Some(Event::WorkerVerificationRecorded { .. })) =>
            {
                points.push((index, Action::Worker { chunks: 0 }, None));
            }
            Event::ToolStarted {
                agent_run_id,
                intent,
                ..
            } => {
                let action = if let crate::ToolCall::PrepareWorker { preparation } = &intent.call {
                    Action::Worker {
                        chunks: usize::from(preparation.max_chunks),
                    }
                } else if workers.contains_key(agent_run_id) {
                    Action::Worker {
                        chunks: if intent.call.mutates()
                            || matches!(intent.call, crate::ToolCall::Command { .. })
                        {
                            workers[agent_run_id].div_ceil(crate::worker::WORKER_CHUNK_BYTES)
                        } else {
                            0
                        },
                    }
                } else if matches!(
                    intent.call,
                    crate::ToolCall::ApplyChangeset {
                        source: crate::worker::ChangesetSource::WorkerHandover { .. }
                    }
                ) {
                    Action::Worker { chunks: 0 }
                } else if primaries.contains(agent_run_id) {
                    Action::Tool
                } else {
                    Action::Child {
                        in_flight: in_flight(index),
                        research: researchers.contains(agent_run_id),
                    }
                };
                points.push((index, action, None));
            }
            _ => {}
        }
    }
    assert!(!points.is_empty(), "the journal admits at least one action");
    let boundaries: Vec<usize> = points
        .iter()
        .map(|(index, ..)| *index)
        .chain(markers)
        .collect();
    for (index, action, end) in &points {
        let end = end.unwrap_or_else(|| {
            boundaries
                .iter()
                .copied()
                .filter(|boundary| boundary > index)
                .min()
                .unwrap_or(events.len())
        });
        // Admission reserves two Events per matched hook on top of the Tool's own.
        let hook_events = events[*index..end]
            .iter()
            .filter(|event| {
                matches!(
                    event.event,
                    Event::HookStarted { .. } | Event::HookFinished { .. }
                )
            })
            .count();
        let observed = end - index - hook_events;
        let config = events[..=*index]
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                Event::RunStarted { config, .. } => Some(config),
                _ => None,
            });
        let providers = config.map(|config| {
            let mut routes = config
                .providers
                .clone()
                .unwrap_or_else(|| crate::Providers::all(vec![config.identity().unwrap()]));
            if let Some(roster) = &config.roster
                && let Some(chain) = roster
                    .child_profiles
                    .iter()
                    .filter_map(|name| roster.chain(name))
                    .max_by_key(|chain| chain.len())
            {
                routes.child = chain.to_vec();
            }
            routes
        });
        let verdicts = config
            .filter(|config| config.supervisor.is_some())
            .map_or(0, |_| action.verdict_slots());
        let bound = verdicts
            + providers.map_or_else(
                || action.max_events(),
                |providers| action.max_events_for(&providers),
            );
        // A primary step also reserves the Events that draining each in-flight editor appends.
        let bound = bound
            + if matches!(action, Action::Tool | Action::ProviderCall { .. }) {
                let held = events[..*index]
                    .iter()
                    .map(|event| match event.event {
                        Event::AgentSpawned {
                            assignment: Some(_),
                            ..
                        } => 1_i64,
                        Event::WorkerLeaseReleased { .. } => -1,
                        _ => 0,
                    })
                    .sum::<i64>();
                // Each editor still holding its lease drains with four Events and one shadow verdict.
                usize::try_from(held).unwrap_or(0) * (4 + usize::from(verdicts > 0))
            } else {
                0
            };
        assert!(
            bound >= observed,
            "{action:?} at {index} bounds {bound} Events but appends {observed}",
        );
    }
    // A sealed-data allowance budgets its own chunks, so it is not headroom for later Events.
    let admissions: Vec<_> = super::admissions::recorded(events[0].session_id)
        .into_iter()
        .filter(|(_, _, sealed_data)| !sealed_data)
        .map(|(count, slots, _)| (count, slots))
        .collect();
    assert!(!admissions.is_empty(), "the Store recorded no admissions");
    let first = admissions.iter().map(|(count, _)| *count).min();
    for (position, envelope) in events.iter().enumerate() {
        let admitted_elsewhere = matches!(
            envelope.event,
            Event::GoalStarted { .. }
                | Event::GoalResumed { .. }
                | Event::GoalRunLinked { .. }
                | Event::SessionStarted { .. }
                | Event::MessageAccepted { .. }
                | Event::RunStarted { .. }
                | Event::AgentSpawned {
                    role: crate::AgentRole::Primary,
                    ..
                }
        );
        if admitted_elsewhere || first.is_none_or(|first| position < first) {
            continue;
        }
        assert!(
            admissions
                .iter()
                .any(|(count, slots)| *count <= position && position < count + slots),
            "{:?} at {position} is past every earlier Store admission",
            envelope.event.kind(),
        );
    }
}

pub struct ForegroundWorkerProvider {
    pub workspace: std::path::PathBuf,
    pub case: &'static str,
    pub budget: crate::GoalBudget,
    pub requests: std::sync::Mutex<Vec<crate::ProviderRequest>>,
    pub violations: std::sync::Mutex<Vec<&'static str>>,
    background_started: tokio::sync::Notify,
    clearing_finished: tokio::sync::Notify,
    identity: crate::ProviderIdentity,
    deadline_identity: crate::ProviderIdentity,
}

impl ForegroundWorkerProvider {
    pub fn new(workspace: &Path, case: &'static str, budget: crate::GoalBudget) -> Self {
        let mut identity =
            crate::ProviderIdentity::new("scripted", "test-model", crate::Provenance::native());
        if matches!(
            case,
            "headroom_background" | "headroom_race" | "clear_background"
        ) {
            identity.limits.concurrency = 2;
        }
        let mut deadline_identity = identity.clone();
        deadline_identity.limits.deadline = std::time::Duration::from_secs(1200);
        Self {
            workspace: workspace.into(),
            case,
            budget,
            requests: Default::default(),
            violations: Default::default(),
            background_started: Default::default(),
            clearing_finished: Default::default(),
            identity,
            deadline_identity,
        }
    }

    fn check(&self, condition: bool, message: &'static str) {
        if !condition {
            self.violations.lock().unwrap().push(message);
        }
    }

    fn worker_command(&self) -> crate::ToolCall {
        crate::ToolCall::Command {
            program: "sh".into(),
            args: vec!["-c".into(), match self.case {
                "command_nonzero" | "command_unguarded" => "exit 1",
                "command_rejected" => "printf rejected > outside",
                _ => "test \"$(cat notes/a)\" = 'edited a' && test \"$(cat notes/build)\" = built",
            }.into()],
            cwd: ".".into(),
        }
    }

    fn command_outcome(
        &self,
        request: &crate::ProviderRequest,
        step: usize,
    ) -> crate::ProviderOutcome {
        use crate::ToolCall;
        if request.phase == AgentPhase::ChildWork {
            let catalog: serde_json::Value =
                serde_json::from_str(request.input.tools.as_ref().unwrap().catalog()).unwrap();
            self.check(
                catalog["commands"]
                    .as_array()
                    .is_some_and(|commands| commands.len() == 1 && commands[0]["name"] == "sh"),
                "private command missing from worker catalog",
            );
            self.check(
                catalog["operations"].as_array().is_some_and(|ops| {
                    ops.iter().any(|op| op == "command")
                        && ops.iter().all(|op| op != "command_unguarded")
                }),
                "worker command operations widened or missing",
            );
            self.check(
                std::fs::read(self.workspace.join("notes/a")).unwrap() == b"original a",
                "command published before handover",
            );
            if self.case == "command_cap"
                || step == 0
                    && !matches!(
                        self.case,
                        "command_denied"
                            | "command_review_denied"
                            | "command_hook_denied"
                            | "command_loss"
                            | "command_nonzero"
                            | "command_rejected"
                            | "command_unguarded"
                    )
            {
                return finish_with("Candidate private result");
            }
            if step > 0 && self.case == "command_denied" {
                let tools = request.input.tools.as_ref().unwrap();
                if tools.steps.last().and_then(|s| s.calls_remaining) == Some(0) {
                    return finish_with("Command was denied");
                }
            }
            let script = match step {
                1 if matches!(
                    self.case,
                    "command_success" | "command_stale" | "command_time"
                ) =>
                {
                    Some("printf 'edited a' > notes/a; printf 'edited b' > notes/b")
                }
                2 if matches!(
                    self.case,
                    "command_success" | "command_stale" | "command_time"
                ) =>
                {
                    Some("test -f notes/a; printf built > notes/build")
                }
                _ => None,
            };
            if let Some(script) = script {
                return super::scenario::tool_outcome(ToolCall::Command {
                    program: "sh".into(),
                    args: vec!["-c".into(), script.into()],
                    cwd: ".".into(),
                });
            }
            if self.case == "command_unguarded" && step == 1 {
                let ToolCall::Command { program, args, cwd } = self.worker_command() else {
                    unreachable!()
                };
                return super::scenario::tool_outcome(ToolCall::CommandUnguarded {
                    program,
                    args,
                    cwd,
                    justification: "sandbox failed".into(),
                });
            }
            if self.case == "command_stale" && step == 4 {
                return super::scenario::tool_outcome(ToolCall::Write {
                    path: "notes/b".into(),
                    expected_digest: Some(crate::tools::types::hex_digest(b"edited b")),
                    content: "late edit".into(),
                });
            }
            if self.case == "command_denied"
                || step == 0
                || step == 3
                    && matches!(
                        self.case,
                        "command_success" | "command_stale" | "command_time"
                    )
            {
                return super::scenario::tool_outcome(self.worker_command());
            }
            return finish_with("Private command work ready");
        }
        if self.case != "command_success" {
            return finish_with("Worker criterion remains unresolved");
        }
        match step {
            0 => super::scenario::tool_outcome(self.worker_command()),
            1 => super::scenario::tool_outcome(ToolCall::Read {
                path: "notes/a".into(),
                offset: None,
                limit: Some(128),
            }),
            _ => finish_with("Primary verified published notes"),
        }
    }

    fn plan(&self) -> crate::GoalPlanContract {
        crate::GoalPlanContract {
            mission: "Edit two leased files and verify their publication".into(),
            paths: if self.case.starts_with("clear_") {
                vec!["notes".into(), "reads".into()]
            } else {
                vec!["notes".into()]
            },
            commands: if self.case.starts_with("command_") {
                vec!["sh".into()]
            } else {
                Vec::new()
            },
            resource_class: crate::ToolResourceClass::Default,
            command_timeout_ms: None,
            skills: Vec::new(),
            mcp_tools: Vec::new(),
            budget: self.budget,
            success_criterion: crate::GoalSuccessCriterion::WorkspaceFileContent {
                path: "notes/a".into(),
                content: "edited a".into(),
            },
        }
    }

    fn delegation(&self, request: &crate::ProviderRequest) -> crate::ProviderOutcome {
        let catalog: serde_json::Value =
            serde_json::from_str(request.input.tools.as_ref().unwrap().catalog()).unwrap();
        let digest = catalog["agents"]
            .as_array()
            .and_then(|agents| agents.iter().find(|a| a["name"] == "editor"))
            .map(|a| a["digest"].clone())
            .and_then(|d| serde_json::from_value(d).ok())
            .unwrap_or([1; 32]);
        let children = vec![crate::ChildAssignment {
            objective: "Edit the leased notes".into(),
            profile: None,
            editor: Some(Box::new(crate::worker::EditorAssignment {
                commands: self.plan().commands,
                definition: "editor".into(),
                definition_digest: digest,
                mission: "Edit the leased notes".into(),
                direction: "Return one whole changeset".into(),
                lease: vec!["notes".into()],
                criterion: if self.case.starts_with("command_") {
                    crate::GoalSuccessCriterion::ToolCallSucceeded {
                        call: Box::new(self.worker_command()),
                    }
                } else if matches!(
                    self.case,
                    "hook_stop_last_turn"
                        | "hook_requested_stop_last_turn"
                        | "hook_stop_tokens"
                        | "hook_stop_deadline"
                        | "hook_stop_deadline_last_turn"
                ) {
                    crate::GoalSuccessCriterion::WorkspaceFileContent {
                        path: "notes/a".into(),
                        content: "original a".into(),
                    }
                } else {
                    self.plan().success_criterion
                },
                mode: if matches!(
                    self.case,
                    "headroom_background" | "headroom_race" | "clear_background"
                ) {
                    crate::worker::WorkerMode::Background
                } else {
                    crate::worker::WorkerMode::Foreground
                },
                max_turns: if matches!(
                    self.case,
                    "hook_stop_last_turn"
                        | "hook_requested_stop_last_turn"
                        | "hook_stop_deadline_last_turn"
                ) {
                    2
                } else {
                    17
                },
                wave: 1,
                resumes: None,
                supersedes: None,
            })),
        }];
        crate::ProviderOutcome::Delegate(crate::Delegate {
            agent: None,
            children,
        })
    }
}

#[async_trait::async_trait]
impl crate::Provider for ForegroundWorkerProvider {
    fn identity(&self) -> &crate::ProviderIdentity {
        if self.case.contains("deadline") {
            &self.deadline_identity
        } else {
            &self.identity
        }
    }

    async fn invoke(
        &self,
        request: crate::ProviderRequest,
    ) -> Result<crate::ProviderResponse, crate::ProviderError> {
        use crate::{ProviderOutcome, ToolCall};
        let step = self
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.phase == request.phase)
            .count();
        self.requests.lock().unwrap().push(request.clone());
        if self.case == "command_time" && request.phase == AgentPhase::ChildWork && step == 4 {
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(3600)).await;
            tokio::time::resume();
        }
        if self.case == "clear_background" && request.phase == AgentPhase::ChildWork && step == 0 {
            self.background_started.notify_one();
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.clearing_finished.notified(),
            )
            .await
            .expect("primary reaches the clearing boundary");
        }
        let mut release_child = None;
        let outcome = match (request.phase, step) {
            (AgentPhase::ToolReview, _) if self.case == "command_review_denied" => {
                ProviderOutcome::Review(crate::ReviewVerdict::Ask)
            }
            (AgentPhase::ChildWork | AgentPhase::RootSynthesis, _)
                if self.case.starts_with("command_") =>
            {
                self.command_outcome(&request, step)
            }
            (AgentPhase::RootPlan, 0) => super::scenario::plan_outcome(self.plan()),
            (AgentPhase::RootPlan, 1) => self.delegation(&request),
            (AgentPhase::RootSynthesis, _) if self.case.starts_with("clear_") => {
                if self.case == "clear_background" && step == 0 {
                    tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        self.background_started.notified(),
                    )
                    .await
                    .expect("background worker starts");
                }
                if step < 40 {
                    super::scenario::tool_outcome(ToolCall::Read {
                        path: "reads/facts.txt".into(),
                        offset: None,
                        limit: None,
                    })
                } else {
                    self.check(
                        request
                            .input
                            .tools
                            .as_ref()
                            .is_some_and(|tools| tools.steps.iter().any(|step| step.cleared)),
                        "old results were not cleared",
                    );
                    self.check(
                        request.input.tools.as_ref().is_some_and(|tools| {
                            tools.steps.first().is_some_and(|step| step.cleared)
                        }),
                        "oldest results were not cleared",
                    );
                    self.check(
                        request.input.tools.as_ref().is_some_and(|tools| {
                            tools.steps.len() >= 3
                                && tools.steps.iter().rev().take(3).all(|step| !step.cleared)
                        }),
                        "three most recent results were not retained",
                    );
                    self.clearing_finished.notify_one();
                    let verified = request.input.tools.as_ref().is_some_and(|tools| {
                        tools.observations().iter().any(|observation| {
                            matches!(&observation.intent.call,
                                ToolCall::Read { path, .. } if path == "notes/a"
                            )
                        })
                    });
                    if !request.input.child_results.is_empty() && !verified {
                        super::scenario::tool_outcome(ToolCall::Read {
                            path: "notes/a".into(),
                            offset: None,
                            limit: None,
                        })
                    } else {
                        finish_with("Primary verified published notes")
                    }
                }
            }
            (AgentPhase::ChildWork, 0) if self.case == "tool_deadline" => {
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(1192)).await;
                tokio::time::resume();
                super::scenario::tool_outcome(ToolCall::Write {
                    path: "notes/a".into(),
                    expected_digest: None,
                    content: "too late".into(),
                })
            }
            (AgentPhase::ChildWork, 0) if self.case == "headroom_race" => {
                self.background_started.notified().await;
                super::scenario::tool_outcome(ToolCall::Read {
                    path: "notes/a".into(),
                    offset: None,
                    limit: Some(128),
                })
            }
            (AgentPhase::ChildWork, 0 | 1)
                if matches!(
                    self.case,
                    "hook_stop_last_turn"
                        | "hook_requested_stop_last_turn"
                        | "hook_stop_tokens"
                        | "hook_stop_deadline"
                        | "hook_stop_deadline_last_turn"
                ) =>
            {
                if step == 0 {
                    super::scenario::tool_outcome(ToolCall::Read {
                        path: "notes/a".into(),
                        offset: None,
                        limit: Some(128),
                    })
                } else {
                    if matches!(
                        self.case,
                        "hook_stop_deadline" | "hook_stop_deadline_last_turn"
                    ) {
                        tokio::time::pause();
                        tokio::time::advance(std::time::Duration::from_secs(1192)).await;
                        tokio::time::resume();
                    }
                    finish_with("Candidate worker result")
                }
            }
            (AgentPhase::ChildWork, _) if self.case == "headroom_background" => {
                self.background_started.notify_one();
                std::future::pending().await
            }
            (AgentPhase::ChildWork, _) if self.case == "headroom_child" => {
                super::scenario::tool_outcome(ToolCall::Read {
                    path: "notes/a".into(),
                    offset: None,
                    limit: Some(128),
                })
            }
            (AgentPhase::RootSynthesis, _)
                if matches!(self.case, "headroom_background" | "headroom_race") =>
            {
                if self.case == "headroom_race" && step == 0 {
                    let (entered, release) = crate::FaultPlan::gate_goal_append_acknowledgment(
                        request.session_id,
                        |event| {
                            matches!(event, Event::ProviderCallRecorded { record, .. }
                            if record.phase == AgentPhase::ChildWork)
                        },
                    );
                    self.background_started.notify_one();
                    tokio::time::timeout(std::time::Duration::from_secs(30), entered)
                        .await
                        .expect("child response reaches the coordinator")
                        .expect("child response commits");
                    release_child = Some(release);
                } else if step == 0 {
                    self.background_started.notified().await;
                }
                super::scenario::tool_outcome(ToolCall::Read {
                    path: "notes/a".into(),
                    offset: None,
                    limit: Some(128),
                })
            }
            (AgentPhase::ChildWork, 0) if self.case == "calls" => {
                finish_with("No Tool allowance remains")
            }
            (AgentPhase::ChildWork, 0 | 1) => {
                self.check(
                    std::fs::read(self.workspace.join("notes/a")).unwrap() == b"original a",
                    "Workspace changed before integration",
                );
                self.check(
                    request.input.role == crate::PromptRole::Editor,
                    "editor prompt role",
                );
                let (path, old, new) = if step == 0 {
                    ("notes/a", b"original a".as_slice(), "edited a")
                } else {
                    ("notes/b", b"original b".as_slice(), "edited b")
                };
                super::scenario::tool_outcome(ToolCall::Write {
                    path: path.into(),
                    expected_digest: Some(crate::tools::types::hex_digest(old)),
                    content: new.into(),
                })
            }
            (AgentPhase::ChildWork, 2) => {
                self.check(
                    std::fs::read(self.workspace.join("notes/a")).unwrap() == b"original a",
                    "private edit leaked to Workspace",
                );
                super::scenario::tool_outcome(ToolCall::Read {
                    path: "notes/a".into(),
                    offset: None,
                    limit: Some(128),
                })
            }
            (AgentPhase::ChildWork, 3) => {
                let tools = request.input.tools.as_ref().unwrap();
                self.check(
                    self.case == "hook_write_update"
                        || tools
                            .observations()
                            .last()
                            .is_some_and(|o| o.output.contains("edited a")),
                    "Read did not see acknowledged private edit",
                );
                if self.case == "conflict" {
                    std::fs::write(self.workspace.join("notes/b"), "host b").unwrap();
                }
                if self.case == "time" {
                    tokio::time::pause();
                    tokio::time::advance(std::time::Duration::from_secs(3600)).await;
                }
                finish_with("Private changes ready")
            }
            (AgentPhase::ChildWork, 4..=11) if self.case == "hook_stop_block" => {
                self.check(
                    request.input.stop_hook_feedback.len() == step - 3,
                    "worker stop feedback was not retained",
                );
                finish_with("Private changes ready")
            }
            (AgentPhase::RootSynthesis, 0)
                if matches!(
                    self.case,
                    "success"
                        | "unverified"
                        | "hook_write"
                        | "hook_default_timeout"
                        | "hook_stop_block"
                        | "hook_handover_post"
                ) =>
            {
                self.check(
                    std::fs::read(self.workspace.join("notes/a")).unwrap() == b"edited a",
                    "integration was not visible to primary",
                );
                if self.case == "unverified" {
                    finish_with("Unverified finish")
                } else {
                    super::scenario::tool_outcome(ToolCall::Read {
                        path: "notes/a".into(),
                        offset: None,
                        limit: Some(128),
                    })
                }
            }
            (AgentPhase::RootSynthesis, 0) if self.case == "calls" => {
                super::scenario::tool_outcome(ToolCall::Read {
                    path: "notes/a".into(),
                    offset: None,
                    limit: Some(128),
                })
            }
            (AgentPhase::RootSynthesis, 0 | 1) => finish_with("Primary verified published notes"),
            _ => {
                self.check(false, "unexpected Provider call");
                ProviderOutcome::Answer(crate::Answer {
                    text: "unexpected".into(),
                })
            }
        };
        let response = crate::ProviderResponse {
            turn: crate::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: crate::Usage {
                input_tokens: Some(2),
                output_tokens: Some(2),
                ..Default::default()
            },
            wire_provenance: None,
        };
        if let Some(release) = release_child {
            // Complete this response in the same poll before the child continuation resumes.
            release
                .send(())
                .expect("release the committed child response");
        }
        Ok(response)
    }
}
