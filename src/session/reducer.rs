use super::events::{
    AgentDisposition, AgentRole, AgentRunId, CompactionRecord, Event, EventEnvelope, ForkLineage,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunConfig, RunDisposition, RunId,
    SessionDefaults, SessionId,
};
use crate::provider::AgentPhase;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunStatus {
    Pending,
    Active,
    Finished,
    Failed,
    Cancelled,
    Interrupted,
}

impl RunStatus {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Finished | Self::Failed | Self::Cancelled | Self::Interrupted
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStatus {
    Active,
    Finished,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentView {
    pub id: AgentRunId,
    pub role: AgentRole,
    pub ordinal: u8,
    pub objective: Option<String>,
    pub summary: Option<String>,
    pub result: Option<String>,
    pub provider_calls: Vec<ProviderCallRecord>,
    pub status: AgentStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunView {
    pub id: RunId,
    pub objective: String,
    pub images: Vec<crate::provider::ImageAttachment>,
    pub config: Option<RunConfig>,
    pub agents: Vec<AgentView>,
    pub assistant_message: Option<String>,
    pub status: RunStatus,
    pub accepted_sequence: u64,
    pub finished_sequence: Option<u64>,
    pub tools: Vec<ToolView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolView {
    pub intent: crate::tools::EffectIntent,
    pub observation: Option<crate::tools::ToolObservation>,
}

impl RunView {
    pub(crate) fn primary_tool_limit_reached(&self) -> bool {
        self.config
            .as_ref()
            .is_some_and(|config| config.tool_policy.is_some())
            && self
                .agents
                .first()
                .and_then(|agent| agent.provider_calls.last())
                .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
    }

    pub(crate) fn tool_context(&self) -> String {
        if self.tools.is_empty() {
            return String::new();
        }
        let records: Vec<_> = self.tools.iter().map(|tool| serde_json::json!({
            "intent":tool.intent,
            "disposition":tool.observation.as_ref().map(|observation| observation.disposition),
            "output":tool.observation.as_ref().map(|observation| &observation.output),
            "execution":if tool.observation.is_none() { "uncertain; never replay" } else { "observed; never replay" },
        })).collect();
        serde_json::json!({"run_status":format!("{:?}", self.status),"tool_records":records})
            .to_string()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionView {
    pub event_sequence: u64,
    pub record: CompactionRecord,
}

impl RunView {
    fn primary(&self) -> Option<&AgentView> {
        self.agents
            .first()
            .filter(|agent| agent.role == AgentRole::Primary)
    }

    fn interrupt(&mut self) {
        if !self.status.is_terminal() {
            self.status = RunStatus::Interrupted;
            for agent in &mut self.agents {
                if agent.status == AgentStatus::Active {
                    agent.status = AgentStatus::Interrupted;
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionView {
    pub id: SessionId,
    pub title: String,
    pub title_is_explicit: bool,
    pub inherited_title: Option<String>,
    pub workspace_identity: Option<(u64, u64)>,
    pub defaults: SessionDefaults,
    pub created_sequence: u64,
    pub last_sequence: u64,
    pub lineage: Option<ForkLineage>,
    pub runs: Vec<RunView>,
    pub compactions: Vec<CompactionView>,
}

impl SessionView {
    pub fn conversation_title(&self) -> String {
        if self.title_is_explicit {
            self.title.clone()
        } else {
            self.generated_title_through(self.last_sequence)
        }
    }

    pub(crate) fn generated_title_through(&self, sequence: u64) -> String {
        let direct = || {
            self.runs.iter().filter(|run| {
                run.accepted_sequence > self.created_sequence && run.accepted_sequence <= sequence
            })
        };
        direct()
            .filter(|run| {
                run.status == RunStatus::Finished
                    && run.finished_sequence.is_some_and(|end| end <= sequence)
            })
            .find_map(|run| run.primary().and_then(|agent| agent.summary.as_deref()))
            .map(super::title_preview)
            .or_else(|| {
                direct()
                    .next()
                    .map(|run| super::title_preview(&run.objective))
            })
            .or_else(|| self.inherited_title.clone())
            .unwrap_or_else(|| "Empty conversation".into())
    }

    pub fn replay(id: SessionId, events: &[EventEnvelope]) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, true)
    }

    pub(crate) fn replay_open(
        id: SessionId,
        events: &[EventEnvelope],
    ) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, false)
    }

    fn replay_prefix(
        id: SessionId,
        events: &[EventEnvelope],
        close_history: bool,
    ) -> Result<Option<Self>, ReplayError> {
        let mut view: Option<Self> = None;
        let mut prior = 0;
        for envelope in events {
            if view.is_none() {
                envelope.event.validate()?;
            }
            if envelope.session_id != id
                || envelope.sequence <= prior
                || envelope.event.scope() != (envelope.run_id, envelope.agent_run_id)
            {
                return Err(ReplayError::InvalidTransition);
            }
            prior = envelope.sequence;
            match &envelope.event {
                Event::SessionStarted {
                    title,
                    workspace_identity,
                } if view.is_none() => {
                    view = Some(Self {
                        id,
                        title: title.clone(),
                        title_is_explicit: !matches!(
                            title.as_str(),
                            "New Session" | "Forked Session"
                        ),
                        inherited_title: None,
                        workspace_identity: *workspace_identity,
                        defaults: SessionDefaults::default(),
                        created_sequence: prior,
                        last_sequence: prior,
                        lineage: None,
                        runs: Vec::new(),
                        compactions: Vec::new(),
                    });
                    continue;
                }
                Event::SessionForked {
                    title,
                    source_session_id,
                    source_run_id,
                    source_sequence,
                    prefix_digest,
                } if view.is_none() && *source_session_id != id => {
                    view = Some(Self {
                        id,
                        title: title.clone(),
                        title_is_explicit: !matches!(
                            title.as_str(),
                            "New Session" | "Forked Session"
                        ),
                        inherited_title: None,
                        workspace_identity: None,
                        defaults: SessionDefaults::default(),
                        created_sequence: prior,
                        last_sequence: prior,
                        lineage: Some(ForkLineage {
                            source_session_id: *source_session_id,
                            source_run_id: *source_run_id,
                            source_sequence: *source_sequence,
                            prefix_digest: *prefix_digest,
                        }),
                        runs: Vec::new(),
                        compactions: Vec::new(),
                    });
                    continue;
                }
                _ => {}
            }
            let current = view.as_mut().ok_or(ReplayError::InvalidTransition)?;
            current.apply_committed(envelope)?;
        }
        if close_history
            && let Some(current) = &mut view
            && let Some(last) = current.runs.last_mut()
        {
            last.interrupt();
        }
        super::compaction::validate_compactions(events)?;
        Ok(view)
    }

    pub(crate) fn apply_committed(&mut self, envelope: &EventEnvelope) -> Result<(), ReplayError> {
        envelope.event.validate()?;
        if envelope.session_id != self.id
            || envelope.sequence <= self.last_sequence
            || envelope.event.scope() != (envelope.run_id, envelope.agent_run_id)
        {
            return Err(ReplayError::InvalidTransition);
        }
        let prior = envelope.sequence;
        let current = self;
        if matches!(
            envelope.event,
            Event::SessionRenamed { .. } | Event::SessionDefaultChanged { .. }
        ) && let Some(previous) = current.runs.last_mut()
        {
            previous.interrupt();
        }
        match &envelope.event {
            Event::SessionStarted { .. } | Event::SessionForked { .. } => {
                return Err(ReplayError::InvalidTransition);
            }
            Event::SessionRenamed { title } => {
                current.title = title.clone();
                current.title_is_explicit = true;
            }
            Event::SessionDefaultChanged { defaults } => {
                current.defaults = defaults.clone();
            }
            Event::ContextCompacted { record } => {
                let boundary = if let Some(run) = current.runs.last() {
                    run.finished_sequence.map(|sequence| (run.id, sequence))
                } else {
                    current
                        .lineage
                        .map(|lineage| (lineage.source_run_id, current.created_sequence))
                };
                if current.compactions.len() >= super::MAX_COMPACTIONS
                    || boundary != Some((record.covered_run_id, record.covered_sequence))
                {
                    return Err(ReplayError::InvalidSnapshot);
                }
                current.compactions.push(CompactionView {
                    event_sequence: prior,
                    record: record.clone(),
                });
            }
            Event::MessageAccepted {
                run_id,
                text,
                images,
            } => {
                if current.runs.iter().any(|run| run.id == *run_id) {
                    return Err(ReplayError::InvalidTransition);
                }
                if let Some(previous) = current.runs.last_mut() {
                    previous.interrupt();
                }
                current.runs.push(RunView {
                    id: *run_id,
                    objective: text.clone(),
                    images: images.clone(),
                    config: None,
                    agents: Vec::new(),
                    assistant_message: None,
                    status: RunStatus::Pending,
                    accepted_sequence: prior,
                    finished_sequence: None,
                    tools: Vec::new(),
                });
            }
            Event::RunStarted { run_id, config } => {
                let workspace_identity = (config.workspace_device, config.workspace_inode);
                if current
                    .workspace_identity
                    .is_some_and(|identity| identity != workspace_identity)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                let run = current_run(current, *run_id)?;
                if run.status != RunStatus::Pending || run.config.is_some() {
                    return Err(ReplayError::InvalidTransition);
                }
                run.config = Some(config.clone());
                run.status = RunStatus::Active;
                current.workspace_identity = Some(workspace_identity);
            }
            Event::AgentSpawned {
                run_id,
                agent_run_id,
                role,
                ordinal,
                objective,
            } => {
                let run = active_run(current, *run_id)?;
                let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
                match role {
                    AgentRole::Primary if run.agents.is_empty() && *ordinal == 0 => {}
                    AgentRole::Child
                        if !run.agents.is_empty()
                            && *ordinal as usize == run.agents.len()
                            && *ordinal <= config.policy.max_children() => {}
                    _ => return Err(ReplayError::InvalidTransition),
                }
                if run.agents.iter().any(|agent| agent.id == *agent_run_id) {
                    return Err(ReplayError::InvalidTransition);
                }
                if *role == AgentRole::Child
                    && run.agents[0]
                        .provider_calls
                        .last()
                        .is_some_and(|call| call.disposition != ProviderCallDisposition::Delegated)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                run.agents.push(AgentView {
                    id: *agent_run_id,
                    role: *role,
                    ordinal: *ordinal,
                    objective: objective.clone(),
                    summary: None,
                    result: None,
                    provider_calls: Vec::new(),
                    status: AgentStatus::Active,
                });
            }
            Event::ProviderCallRecorded {
                run_id,
                agent_run_id,
                record,
            } => {
                let run = active_run(current, *run_id)?;
                let index = run
                    .agents
                    .iter()
                    .position(|agent| agent.id == *agent_run_id)
                    .ok_or(ReplayError::InvalidTransition)?;
                let agent = &run.agents[index];
                let tools_enabled = run
                    .config
                    .as_ref()
                    .is_some_and(|config| config.tool_policy.is_some());
                let tool_continuation = tools_enabled
                    && agent.role == AgentRole::Primary
                    && agent.provider_calls.last().is_some_and(|call| {
                        call.disposition == ProviderCallDisposition::ToolRequested
                            && call.phase == record.phase
                    })
                    && run.tools.last().is_some_and(|tool| {
                        tool.observation.as_ref().is_some_and(|observation| {
                            !matches!(
                                observation.disposition,
                                crate::tools::ToolDisposition::Uncertain
                                    | crate::tools::ToolDisposition::Cancelled
                            )
                        })
                    })
                    && run.tools.len()
                        == agent
                            .provider_calls
                            .iter()
                            .filter(|call| {
                                call.disposition == ProviderCallDisposition::ToolRequested
                            })
                            .count();
                let delegated = tools_enabled
                    && agent.role == AgentRole::Primary
                    && agent
                        .provider_calls
                        .last()
                        .is_some_and(|call| call.disposition == ProviderCallDisposition::Delegated)
                    && record.phase == AgentPhase::RootSynthesis
                    && run.agents.len() > 1
                    && run
                        .agents
                        .iter()
                        .skip(1)
                        .all(|child| child.status == AgentStatus::Finished);
                let valid = tool_continuation
                    || delegated
                    || match (agent.role, agent.provider_calls.as_slice(), record.phase) {
                        (AgentRole::Primary, [], AgentPhase::RootPlan) => true,
                        (AgentRole::Primary, [first], AgentPhase::RootSynthesis)
                            if first.disposition == ProviderCallDisposition::Delegated =>
                        {
                            run.agents.len() > 1
                                && run
                                    .agents
                                    .iter()
                                    .skip(1)
                                    .all(|child| child.status == AgentStatus::Finished)
                        }
                        (AgentRole::Child, [], AgentPhase::ChildWork) => true,
                        _ => false,
                    };
                if agent.status != AgentStatus::Active
                    || !valid
                    || record.disposition == ProviderCallDisposition::ToolRequested
                        && (!tools_enabled || agent.role != AgentRole::Primary)
                    || tools_enabled && agent.provider_calls.len() >= crate::tools::MAX_MODEL_STEPS
                    || (record.phase != AgentPhase::RootPlan
                        && record.disposition == ProviderCallDisposition::Delegated)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                run.agents[index].provider_calls.push(record.clone());
            }
            Event::ToolStarted {
                run_id,
                agent_run_id,
                intent,
            } => {
                let run = active_run(current, *run_id)?;
                let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
                let primary = run.primary().ok_or(ReplayError::InvalidTransition)?;
                if config
                    .tool_policy
                    .as_ref()
                    .is_none_or(|policy| policy.config_digest != intent.policy_digest)
                    || primary.id != *agent_run_id
                    || primary.status != AgentStatus::Active
                    || primary.provider_calls.last().is_none_or(|call| {
                        call.disposition != ProviderCallDisposition::ToolRequested
                    })
                    || run.tools.len() >= crate::tools::MAX_TOOL_CALLS
                    || primary
                        .provider_calls
                        .iter()
                        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
                        .count()
                        != run.tools.len() + 1
                    || run
                        .tools
                        .iter()
                        .any(|tool| tool.intent.id == intent.id || tool.observation.is_none())
                    || intent.workspace_device != config.workspace_device
                    || intent.workspace_inode != config.workspace_inode
                {
                    return Err(ReplayError::InvalidTransition);
                }
                run.tools.push(ToolView {
                    intent: intent.clone(),
                    observation: None,
                });
            }
            Event::ToolFinished {
                run_id,
                agent_run_id,
                observation,
            } => {
                let run = active_run(current, *run_id)?;
                if run.primary().is_none_or(|agent| {
                    agent.id != *agent_run_id || agent.status != AgentStatus::Active
                }) {
                    return Err(ReplayError::InvalidTransition);
                }
                let tool = run.tools.last_mut().ok_or(ReplayError::InvalidTransition)?;
                if tool.intent != observation.intent || tool.observation.is_some() {
                    return Err(ReplayError::InvalidTransition);
                }
                tool.observation = Some(observation.clone());
                let observations: Vec<_> = run
                    .tools
                    .iter()
                    .filter_map(|tool| tool.observation.as_ref())
                    .collect();
                if !serde_json::to_vec(&observations)
                    .is_ok_and(|bytes| bytes.len() <= crate::tools::MAX_TOOL_CONTEXT_BYTES)
                {
                    return Err(ReplayError::InvalidTransition);
                }
            }
            Event::AgentFinished {
                run_id,
                agent_run_id,
                disposition,
                summary,
                result,
            } => {
                let run = active_run(current, *run_id)?;
                let index = run
                    .agents
                    .iter()
                    .position(|agent| agent.id == *agent_run_id)
                    .ok_or(ReplayError::InvalidTransition)?;
                if run.agents[index].status != AgentStatus::Active {
                    return Err(ReplayError::InvalidTransition);
                }
                if run.agents[index].role == AgentRole::Primary
                    && *disposition == AgentDisposition::Finished
                    && run
                        .agents
                        .iter()
                        .skip(1)
                        .any(|agent| agent.status != AgentStatus::Finished)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if run.agents[index].role == AgentRole::Child
                    && result.as_ref().is_some_and(|text| text.len() > 16 * 1024)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if *disposition == AgentDisposition::Finished
                    && run.tools.iter().any(|tool| {
                        tool.observation.is_none()
                            || tool.observation.as_ref().is_some_and(|observation| {
                                matches!(
                                    observation.disposition,
                                    crate::tools::ToolDisposition::Uncertain
                                        | crate::tools::ToolDisposition::Cancelled
                                )
                            })
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
                if *disposition == AgentDisposition::Finished
                    && run.agents[index]
                        .provider_calls
                        .last()
                        .is_some_and(|call| call.disposition != ProviderCallDisposition::Finished)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                let agent = &mut run.agents[index];
                agent.status = match disposition {
                    AgentDisposition::Finished => AgentStatus::Finished,
                    AgentDisposition::Failed => AgentStatus::Failed,
                    AgentDisposition::Cancelled => AgentStatus::Cancelled,
                };
                agent.summary = summary.clone();
                agent.result = result.clone();
            }
            Event::MessageCommitted { run_id, text } => {
                let run = active_run(current, *run_id)?;
                if run.assistant_message.is_some()
                    || run.primary().is_none_or(|primary| {
                        primary.status != AgentStatus::Finished
                            || primary.result.as_deref() != Some(text)
                    })
                {
                    return Err(ReplayError::InvalidTransition);
                }
                run.assistant_message = Some(text.clone());
            }
            Event::RunFinished {
                run_id,
                disposition,
            } => {
                let run = active_run(current, *run_id)?;
                if run.agents.is_empty()
                    || run
                        .agents
                        .iter()
                        .any(|agent| agent.status == AgentStatus::Active)
                {
                    return Err(ReplayError::InvalidTransition);
                }
                run.status = match disposition {
                    RunDisposition::Finished
                        if run.assistant_message.is_some()
                            && run
                                .agents
                                .iter()
                                .all(|agent| agent.status == AgentStatus::Finished) =>
                    {
                        RunStatus::Finished
                    }
                    RunDisposition::Failed if run.assistant_message.is_none() => RunStatus::Failed,
                    RunDisposition::Cancelled if run.assistant_message.is_none() => {
                        RunStatus::Cancelled
                    }
                    _ => return Err(ReplayError::InvalidTransition),
                };
                run.finished_sequence = Some(prior);
            }
        }
        current.last_sequence = prior;
        Ok(())
    }
}

fn current_run(view: &mut SessionView, id: RunId) -> Result<&mut RunView, ReplayError> {
    view.runs
        .last_mut()
        .filter(|run| run.id == id)
        .ok_or(ReplayError::InvalidTransition)
}

fn active_run(view: &mut SessionView, id: RunId) -> Result<&mut RunView, ReplayError> {
    let run = current_run(view, id)?;
    if run.status != RunStatus::Active {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(run)
}
