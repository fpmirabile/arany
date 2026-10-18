use crate::{AgentPhase, Event, Finish, ProviderOutcome, SessionView, StateRoot, Store};
use std::path::Path;

pub fn finish_with(text: &str) -> ProviderOutcome {
    ProviderOutcome::Finish(Finish {
        summary: text.into(),
        result: text.into(),
    })
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
                grant: Some(crate::ChildGrant::ReadOnly),
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
                        research: spawned.contains(&Some(crate::ChildGrant::ReadOnly)),
                    },
                    Some(end),
                ));
            }
            Event::ProviderFallbackUsed {
                agent_run_id,
                fallback,
                ..
            } => {
                let action = match fallback.role {
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
                        chunks: if intent.call.mutates() {
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
        let providers = events[..=*index]
            .iter()
            .rev()
            .find_map(|event| match &event.event {
                Event::RunStarted { config, .. } => Some(config),
                _ => None,
            })
            .map(|config| {
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
        let bound = providers.map_or_else(
            || action.max_events(),
            |providers| action.max_events_for(&providers),
        );
        assert!(
            bound >= observed,
            "{action:?} at {index} bounds {bound} Events but appends {observed}",
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
    identity: crate::ProviderIdentity,
}

impl ForegroundWorkerProvider {
    pub fn new(workspace: &Path, case: &'static str, budget: crate::GoalBudget) -> Self {
        let mut identity =
            crate::ProviderIdentity::new("scripted", "test-model", crate::Provenance::native());
        if matches!(case, "headroom_background" | "headroom_race") {
            identity.limits.concurrency = 2;
        }
        Self {
            workspace: workspace.into(),
            case,
            budget,
            requests: Default::default(),
            violations: Default::default(),
            background_started: Default::default(),
            identity,
        }
    }

    fn check(&self, condition: bool, message: &'static str) {
        if !condition {
            self.violations.lock().unwrap().push(message);
        }
    }

    fn plan(&self) -> crate::GoalPlanContract {
        crate::GoalPlanContract {
            mission: "Edit two leased files and verify their publication".into(),
            paths: vec!["notes".into()],
            commands: Vec::new(),
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
            serde_json::from_str(&request.input.tools.as_ref().unwrap().catalog).unwrap();
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
                definition: "editor".into(),
                definition_digest: digest,
                mission: "Edit the leased notes".into(),
                direction: "Return one whole changeset".into(),
                lease: vec!["notes".into()],
                criterion: self.plan().success_criterion,
                mode: if matches!(self.case, "headroom_background" | "headroom_race") {
                    crate::worker::WorkerMode::Background
                } else {
                    crate::worker::WorkerMode::Foreground
                },
                max_turns: 17,
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
        &self.identity
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
        let outcome = match (request.phase, step) {
            (AgentPhase::RootPlan, 0) => super::scenario::tool_outcome(
                ToolCall::Read {
                    path: "notes/a".into(),
                    offset: 0,
                    limit: 128,
                },
                Some(Box::new(self.plan())),
            ),
            (AgentPhase::RootPlan, 1) => self.delegation(&request),
            (AgentPhase::ChildWork, 0) if self.case == "headroom_race" => {
                self.background_started.notify_one();
                super::scenario::tool_outcome(
                    ToolCall::Read {
                        path: "notes/a".into(),
                        offset: 0,
                        limit: 128,
                    },
                    None,
                )
            }
            (AgentPhase::ChildWork, _) if self.case == "headroom_background" => {
                self.background_started.notify_one();
                std::future::pending().await
            }
            (AgentPhase::ChildWork, _) if self.case == "headroom_child" => {
                super::scenario::tool_outcome(
                    ToolCall::Read {
                        path: "notes/a".into(),
                        offset: 0,
                        limit: 128,
                    },
                    None,
                )
            }
            (AgentPhase::RootSynthesis, _)
                if matches!(self.case, "headroom_background" | "headroom_race") =>
            {
                if step == 0 {
                    self.background_started.notified().await;
                }
                super::scenario::tool_outcome(
                    ToolCall::Read {
                        path: "notes/a".into(),
                        offset: 0,
                        limit: 128,
                    },
                    None,
                )
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
                super::scenario::tool_outcome(
                    ToolCall::Write {
                        path: path.into(),
                        expected_digest: Some(crate::tools::types::hex_digest(old)),
                        content: new.into(),
                    },
                    None,
                )
            }
            (AgentPhase::ChildWork, 2) => {
                self.check(
                    std::fs::read(self.workspace.join("notes/a")).unwrap() == b"original a",
                    "private edit leaked to Workspace",
                );
                super::scenario::tool_outcome(
                    ToolCall::Read {
                        path: "notes/a".into(),
                        offset: 0,
                        limit: 128,
                    },
                    None,
                )
            }
            (AgentPhase::ChildWork, 3) => {
                let tools = request.input.tools.as_ref().unwrap();
                self.check(
                    tools
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
            (AgentPhase::RootSynthesis, 0) if matches!(self.case, "success" | "unverified") => {
                self.check(
                    std::fs::read(self.workspace.join("notes/a")).unwrap() == b"edited a",
                    "integration was not visible to primary",
                );
                if self.case == "unverified" {
                    finish_with("Unverified finish")
                } else {
                    super::scenario::tool_outcome(
                        ToolCall::Read {
                            path: "notes/a".into(),
                            offset: 0,
                            limit: 128,
                        },
                        None,
                    )
                }
            }
            (AgentPhase::RootSynthesis, 0) if self.case == "calls" => {
                super::scenario::tool_outcome(
                    ToolCall::Read {
                        path: "notes/a".into(),
                        offset: 0,
                        limit: 128,
                    },
                    None,
                )
            }
            (AgentPhase::RootSynthesis, 0 | 1) => finish_with("Primary verified published notes"),
            _ => {
                self.check(false, "unexpected Provider call");
                ProviderOutcome::Finish(crate::Finish {
                    summary: "unexpected".into(),
                    result: "unexpected".into(),
                })
            }
        };
        Ok(crate::ProviderResponse {
            turn: crate::ProviderTurn::scripted(),
            outcome,
            response_id: None,
            usage: crate::Usage {
                input_tokens: Some(2),
                output_tokens: Some(2),
                ..Default::default()
            },
            wire_provenance: None,
        })
    }
}
