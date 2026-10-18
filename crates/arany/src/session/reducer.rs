use super::events::{
    AgentDisposition, AgentRole, AgentRunId, CompactionRecord, Event, EventEnvelope, ForkLineage,
    GoalBudget, GoalId, GoalPhase, GoalPlanContract, GoalReport, GoalStatus, GoalUsage,
    ProviderCallDisposition, ProviderCallRecord, ReplayError, RunConfig, RunDisposition, RunId,
    SessionDefaults, SessionId, WorkspaceIdentity,
};
use crate::provider::AgentPhase;

mod agent;
mod goal;
mod run;
mod session;
mod tool;

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
pub struct GoalView {
    pub id: GoalId,
    pub objective: String,
    pub workspace_identity: WorkspaceIdentity,
    pub budget: GoalBudget,
    pub resource_settings: crate::tools::ToolResourceSettings,
    pub phase: GoalPhase,
    pub autonomous: bool,
    pub status: GoalStatus,
    pub run_ids: Vec<RunId>,
    pub active_run: Option<RunId>,
    pub plan_contract: Option<GoalPlanContract>,
    pub(crate) success_state: crate::session::GoalSuccessState,
    pub success_criterion_satisfied: bool,
    pub awaiting_plan_contract: bool,
    pub active_ms: u64,
    pub uncertain_active_ms: u64,
    /// The started Run whose active time is not settled yet, with its active deadline.
    pub active_deadline: Option<(RunId, Option<u64>)>,
    /// Upper bound of the unsettled active time of the open Run or Goal compaction call.
    pub active_ceiling_ms: u64,
    pub provider_calls: u32,
    pub unobserved_provider_calls: Vec<AgentRunId>,
    /// Run whose Goal-context compaction call is reserved and not yet settled.
    pub compaction_call_reserved: Option<RunId>,
    /// Goal-context compaction calls whose outcome a crash left unknown.
    pub uncertain_compaction_calls: u32,
    pub known_tokens: u64,
    /// Information only: cached input tokens already included in `known_tokens`.
    pub known_cached_tokens: u64,
    pub token_usage_unknown: bool,
    pub report: Option<GoalReport>,
    pub started_sequence: u64,
    pub finished_sequence: Option<u64>,
    /// The exhausted Goal this one continues.
    pub continues: Option<GoalId>,
    /// Failed because the budget ran out (`GoalFinished.exhausted`).
    pub budget_closed: bool,
}

impl GoalView {
    pub fn effective_budget(&self) -> GoalBudget {
        self.budget.effective(self.plan_contract.as_ref())
    }

    pub(crate) fn admits_next_run(&self) -> bool {
        self.effective_budget().admits_next_run(GoalUsage {
            runs: self.run_ids.len(),
            provider_calls: self.provider_calls,
            active_ms: self.active_ms,
            known_tokens: self.known_tokens,
            token_usage_unknown: self.token_usage_unknown,
        })
    }

    pub(crate) fn budget_spent(&self) -> bool {
        self.effective_budget().spent(GoalUsage {
            runs: self.run_ids.len(),
            provider_calls: self.provider_calls,
            active_ms: self.active_ms,
            known_tokens: self.known_tokens,
            token_usage_unknown: self.token_usage_unknown,
        })
    }

    pub(crate) fn unsettled_active_charge_ms(&self) -> u64 {
        let ceiling = self
            .active_deadline
            .and_then(|(_, deadline)| deadline)
            .map_or(self.active_ceiling_ms, |deadline| {
                self.active_ceiling_ms.min(deadline)
            });
        self.effective_budget()
            .max_active_ms
            .map_or(ceiling, |limit| {
                ceiling.min(limit.saturating_sub(self.active_ms))
            })
    }

    pub(crate) fn resume_usage(&self, view: &SessionView) -> GoalUsage {
        let unaccepted_link = self
            .run_ids
            .last()
            .is_some_and(|run_id| !view.runs.iter().any(|run| run.id == *run_id));
        GoalUsage {
            runs: self.run_ids.len() - usize::from(unaccepted_link),
            provider_calls: self.provider_calls,
            active_ms: self
                .active_ms
                .saturating_add(self.unsettled_active_charge_ms()),
            known_tokens: self.known_tokens,
            token_usage_unknown: self.token_usage_unknown
                || !self.unobserved_provider_calls.is_empty()
                || self.compaction_call_reserved.is_some(),
        }
    }

    /// Whether explicit resume admits another Run; the Engine and `/goal` presentation both use this.
    pub fn admits_resume(&self, view: &SessionView) -> bool {
        self.effective_budget()
            .admits_next_run(self.resume_usage(view))
    }

    pub(crate) fn can_resume(&self, view: &SessionView) -> bool {
        if self.active_run.is_some() {
            return false;
        }
        let completed_run_ids = if self
            .run_ids
            .last()
            .is_some_and(|run_id| !view.runs.iter().any(|run| run.id == *run_id))
        {
            &self.run_ids[..self.run_ids.len().saturating_sub(1)]
        } else {
            &self.run_ids
        };
        completed_run_ids.iter().all(|run_id| {
            view.runs
                .iter()
                .find(|run| run.id == *run_id)
                .is_some_and(|run| {
                    run.tools
                        .iter()
                        .chain(run.agents.iter().flat_map(|agent| agent.tools.iter()))
                        .all(|tool| {
                            tool.observation
                                .as_ref()
                                .is_some_and(|observation| !observation.stops_run())
                        })
                })
        })
    }
}

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentView {
    pub definition: Option<String>,
    pub id: AgentRunId,
    pub role: AgentRole,
    pub ordinal: u8,
    pub objective: Option<String>,
    pub summary: Option<String>,
    pub result: Option<String>,
    pub provider_calls: Vec<ProviderCallRecord>,
    pub grant: Option<crate::session::ChildGrant>,
    /// Read-only research Tools of a child; the primary's Tools stay in `RunView::tools`.
    pub tools: Vec<ToolView>,
    pub status: AgentStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunView {
    pub id: RunId,
    pub objective: String,
    pub images: Vec<crate::provider::ImageAttachment>,
    pub config: Option<RunConfig>,
    pub provider_fallbacks: Vec<(AgentRunId, crate::provider::ProviderFallback)>,
    pub agents: Vec<AgentView>,
    pub assistant_message: Option<String>,
    pub status: RunStatus,
    pub accepted_sequence: u64,
    pub finished_sequence: Option<u64>,
    pub tools: Vec<ToolView>,
    pub tool_compaction_summary: Option<String>,
    pub tool_compaction_covered_count: u32,
    pub display: Vec<super::events::DisplayChunk>,
    pub steering_notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolView {
    pub intent: crate::tools::EffectIntent,
    pub observation: Option<crate::tools::ToolObservation>,
    pub review_call_index: Option<u32>,
    pub permission_verdict: Option<crate::tools::permissions::PermissionVerdict>,
    pub finished_sequence: Option<u64>,
}

impl ToolView {
    /// A host run that may have had effects: attested or uncertain, or approved without a recorded outcome.
    pub fn ran_unguarded(&self) -> bool {
        self.intent.call.is_unguarded()
            && match &self.observation {
                Some(observation) => {
                    matches!(
                        observation.attestation,
                        Some(crate::tools::Attestation::Unguarded(_))
                    ) || observation.disposition == crate::tools::ToolDisposition::Uncertain
                }
                None => self
                    .permission_verdict
                    .as_ref()
                    .is_some_and(|verdict| verdict.approved),
            }
    }
}

fn all_children_finished(run: &RunView) -> bool {
    run.agents.len() > 1
        && run
            .agents
            .iter()
            .skip(1)
            .all(|child| child.status == AgentStatus::Finished)
}

fn model_tool_count(run: &RunView) -> usize {
    run.tools
        .iter()
        .filter(|tool| {
            !is_integration_tool(tool)
                && !matches!(
                    tool.intent.call,
                    crate::tools::ToolCall::ParseAgentDefinition { .. }
                )
        })
        .count()
}

fn is_integration_tool(tool: &ToolView) -> bool {
    matches!(
        tool.intent.call,
        crate::tools::ToolCall::ApplyChangeset { .. }
    )
}

fn requested_tool_total(agent: &AgentView) -> usize {
    agent
        .provider_calls
        .iter()
        .filter(|call| call.disposition == ProviderCallDisposition::ToolRequested)
        .map(|call| usize::from(call.requested_tools))
        .sum()
}

fn pending_tool_requests(run: &RunView) -> Option<usize> {
    requested_tool_total(run.primary()?).checked_sub(model_tool_count(run))
}

fn observed_command_changeset(
    observation: &crate::tools::ToolObservation,
) -> Option<crate::tools::CommandChangeset> {
    if !matches!(
        observation.intent.call,
        crate::tools::ToolCall::Command { .. }
    ) || !observation.attested_for(&observation.intent)
        || !matches!(
            observation.disposition,
            crate::tools::ToolDisposition::Succeeded | crate::tools::ToolDisposition::Failed
        )
    {
        return None;
    }
    serde_json::from_str::<crate::tools::types::CommandResult>(&observation.output)
        .ok()
        .filter(|result| result.valid())
        .map(|result| result.changeset)
}

fn is_tool_continuation(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Primary
        && agent.provider_calls.last().is_some_and(|call| {
            call.disposition == ProviderCallDisposition::ToolRequested && call.phase == record.phase
                || call.phase == AgentPhase::ToolReview
                    && agent
                        .provider_calls
                        .iter()
                        .rev()
                        .find(|call| call.disposition == ProviderCallDisposition::ToolRequested)
                        .is_some_and(|call| call.phase == record.phase)
        })
        && run.tools.last().is_some_and(|tool| {
            tool.observation.as_ref().is_some_and(|observation| {
                !observation.stops_run()
                    && observed_command_changeset(observation)
                        .is_none_or(|changeset| changeset.files.is_empty())
            })
        })
        && requested_tool_total(agent).checked_sub(model_tool_count(run)) == Some(0)
}

fn is_delegated_synthesis(run: &RunView, agent: &AgentView, record: &ProviderCallRecord) -> bool {
    agent.role == AgentRole::Primary
        && agent
            .provider_calls
            .last()
            .is_some_and(|call| call.disposition == ProviderCallDisposition::Delegated)
        && record.phase == AgentPhase::RootSynthesis
        && all_children_finished(run)
}

impl RunView {
    pub fn unguarded_tools(&self) -> usize {
        self.tools
            .iter()
            .filter(|tool| tool.ran_unguarded())
            .count()
    }

    pub fn primary_tool_limit_reached(&self) -> bool {
        self.config
            .as_ref()
            .is_some_and(|config| config.tool_policy.is_some())
            && self
                .agents
                .first()
                .and_then(|agent| agent.provider_calls.last())
                .is_some_and(|call| call.disposition == ProviderCallDisposition::ToolRequested)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ForkInheritance {
    pub(crate) runs: Vec<RunView>,
    pub(crate) compactions: Vec<CompactionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompactionView {
    pub event_sequence: u64,
    pub record: CompactionRecord,
}

impl RunView {
    pub(crate) fn history_user_text(&self) -> String {
        let mut text = self.objective.clone();
        if !self.steering_notes.is_empty() {
            text.push_str("\n\nSteering notes the user sent during this Run:");
            for (index, note) in self.steering_notes.iter().enumerate() {
                use std::fmt::Write;
                write!(&mut text, "\n[{}] {note}", index + 1).expect("String formatting");
            }
        }
        text
    }

    pub(crate) fn primary(&self) -> Option<&AgentView> {
        self.agents
            .first()
            .filter(|agent| agent.role == AgentRole::Primary)
    }

    pub(crate) fn compaction_matches(&self, record: &CompactionRecord) -> bool {
        let Some(routes) = self
            .config
            .as_ref()
            .and_then(|config| config.providers.as_ref())
        else {
            return true;
        };
        let identity = self
            .provider_fallbacks
            .iter()
            .rev()
            .find(|(_, fallback)| fallback.role == crate::provider::ProviderRole::Compaction)
            .map(|(_, fallback)| &fallback.to)
            .unwrap_or(&routes.compaction[0]);
        record.output_tokens.is_none_or(|tokens| {
            tokens
                <= identity
                    .limits
                    .max_output_tokens
                    .min(super::MAX_COMPACTION_OUTPUT_TOKENS)
        }) && identity.profile == record.provider
            && identity.model == record.model
            && identity.chatgpt_provenance() == record.chatgpt_provenance.as_ref()
            && identity.limits.output_token_bound == record.output_token_bound
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
    pub workspace_identity: Option<WorkspaceIdentity>,
    pub defaults: SessionDefaults,
    pub created_sequence: u64,
    pub last_sequence: u64,
    pub lineage: Option<ForkLineage>,
    pub runs: Vec<RunView>,
    pub compactions: Vec<CompactionView>,
    pub goals: Vec<GoalView>,
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
        Self::replay_prefix(id, events, true, None)
    }

    pub(crate) fn replay_open(
        id: SessionId,
        events: &[EventEnvelope],
    ) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, false, None)
    }

    pub(crate) fn replay_forked(
        id: SessionId,
        events: &[EventEnvelope],
        close_history: bool,
        inherited: &ForkInheritance,
    ) -> Result<Option<Self>, ReplayError> {
        Self::replay_prefix(id, events, close_history, Some(inherited))
    }

    fn replay_prefix(
        id: SessionId,
        events: &[EventEnvelope],
        close_history: bool,
        inherited: Option<&ForkInheritance>,
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
                        title_is_explicit: !crate::session::placeholder_title(title),
                        inherited_title: None,
                        workspace_identity: Some(*workspace_identity),
                        defaults: SessionDefaults::default(),
                        created_sequence: prior,
                        last_sequence: prior,
                        lineage: None,
                        runs: Vec::new(),
                        compactions: Vec::new(),
                        goals: Vec::new(),
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
                        title_is_explicit: !crate::session::placeholder_title(title),
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
                        runs: inherited.map_or_else(Vec::new, |inherited| inherited.runs.clone()),
                        compactions: inherited
                            .map_or_else(Vec::new, |inherited| inherited.compactions.clone()),
                        goals: Vec::new(),
                    });
                    continue;
                }
                _ => {}
            }
            let current = view.as_mut().ok_or(ReplayError::InvalidTransition)?;
            current.apply_committed(envelope)?;
        }
        if close_history && let Some(current) = &mut view {
            if let Some(last) = current.runs.last_mut() {
                last.interrupt();
            }
            // A Goal whose Run ended without a terminal receipt is paused, wherever it sits in history.
            for goal in current
                .goals
                .iter_mut()
                .filter(|goal| goal.status == GoalStatus::Running)
            {
                goal.status = GoalStatus::Interrupted;
                goal.active_run = None;
            }
        }
        super::compaction::validate_compactions(events)?;
        super::compaction::validate_goal_context_compactions(events)?;
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
        // A Goal waiting for its plan contract admits only plan events, its explicit resume, or a new Goal that seals it.
        if let Some(awaiting) = current
            .goals
            .iter()
            .find(|goal| goal.awaiting_plan_contract)
            .map(|goal| goal.id)
            && !matches!(
                &envelope.event,
                Event::GoalPlanContractRecorded { goal_id, .. } if *goal_id == awaiting
            )
            && !matches!(
                &envelope.event,
                Event::ToolStarted { intent, .. }
                    if intent.call.is_goal_plan_prelude()
            )
            && !matches!(
                &envelope.event,
                Event::ToolFinished { observation, .. }
                    if observation.intent.call.is_goal_plan_prelude()
            )
            && !matches!(&envelope.event, Event::ToolPermissionDecided { .. })
            && !matches!(
                &envelope.event,
                Event::GoalResumed { goal_id } if *goal_id == awaiting
            )
            && !matches!(&envelope.event, Event::GoalStarted { .. })
            && !closes_run_without_plan(&envelope.event)
        {
            return Err(ReplayError::InvalidTransition);
        }
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
            Event::SessionRenamed { .. }
            | Event::SessionDefaultChanged { .. }
            | Event::ContextCompacted { .. } => session::apply(current, &envelope.event, prior)?,
            Event::GoalStarted { .. }
            | Event::GoalRunLinked { .. }
            | Event::GoalResumed { .. }
            | Event::GoalPlanContractRecorded { .. }
            | Event::GoalProviderCallReserved { .. }
            | Event::GoalCompactionCallReserved { .. }
            | Event::GoalContextCompacted { .. }
            | Event::GoalFinished { .. }
            | Event::GoalSteeringNoteRecorded { .. } => {
                goal::apply(current, &envelope.event, prior)?
            }
            Event::MessageAccepted { .. }
            | Event::RunStarted { .. }
            | Event::AssistantDisplayRecorded { .. }
            | Event::MessageCommitted { .. }
            | Event::RunFinished { .. } => run::apply(current, &envelope.event, prior)?,
            Event::AgentSpawned { .. }
            | Event::ProviderFallbackUsed { .. }
            | Event::ProviderCallRecorded { .. }
            | Event::AgentFinished { .. } => agent::apply(current, &envelope.event)?,
            Event::ToolStarted { .. }
            | Event::ToolFinished { .. }
            | Event::ToolPermissionDecided { .. } => tool::apply(current, &envelope.event, prior)?,
        }
        current.last_sequence = prior;
        Ok(())
    }

    /// The Goal whose Run is live. At most one Goal is Running.
    pub fn active_goal(&self) -> Option<&GoalView> {
        self.goals
            .iter()
            .find(|goal| goal.status == GoalStatus::Running)
    }

    pub(crate) fn resumable_goal_index(&self) -> Option<usize> {
        self.goals
            .iter()
            .rposition(|goal| matches!(goal.status, GoalStatus::Running | GoalStatus::Interrupted))
    }

    /// The paused Goal that explicit resume may continue.
    pub fn interrupted_goal_id(&self) -> Option<GoalId> {
        self.resumable_goal_index()
            .map(|index| &self.goals[index])
            .filter(|goal| goal.status == GoalStatus::Interrupted)
            .map(|goal| goal.id)
    }

    /// The most recent Goal closed by budget exhaustion that no later Goal continues yet,
    /// unless its last Run left a Tool without a known outcome.
    pub fn continuable_goal(&self) -> Option<&GoalView> {
        let latest = self.goals.iter().rev().find(|goal| {
            goal.budget_closed
                && !self
                    .goals
                    .iter()
                    .any(|other| other.continues == Some(goal.id))
        })?;
        latest
            .run_ids
            .iter()
            .rev()
            .find_map(|run_id| self.runs.iter().find(|run| run.id == *run_id))
            .is_some_and(|run| {
                run.tools
                    .iter()
                    .chain(run.agents.iter().flat_map(|agent| agent.tools.iter()))
                    .all(|tool| {
                        tool.observation.as_ref().is_some_and(|observation| {
                            observation.disposition != crate::tools::ToolDisposition::Uncertain
                                && observation.cache_update != crate::tools::CacheUpdate::Uncertain
                        })
                    })
            })
            .then_some(latest)
    }

    pub fn goal(&self, id: GoalId) -> Option<&GoalView> {
        self.goals.iter().find(|goal| goal.id == id)
    }
}

fn active_run_goal(view: &SessionView, run_id: RunId) -> Option<&GoalView> {
    view.goals
        .iter()
        .find(|goal| goal.active_run == Some(run_id))
}

fn current_run(view: &mut SessionView, id: RunId) -> Result<&mut RunView, ReplayError> {
    view.runs
        .last_mut()
        .filter(|run| run.id == id)
        .ok_or(ReplayError::InvalidTransition)
}

fn current_goal_mut(view: &mut SessionView, id: GoalId) -> Result<&mut GoalView, ReplayError> {
    view.goals
        .iter_mut()
        .find(|goal| goal.id == id)
        .ok_or(ReplayError::InvalidTransition)
}

// A refusal before ToolStarted leaves no pending plan, so failure and cancellation must still close the Run.
fn closes_run_without_plan(event: &Event) -> bool {
    matches!(
        event,
        Event::AgentFinished {
            disposition: AgentDisposition::Failed | AgentDisposition::Cancelled,
            ..
        } | Event::RunFinished {
            disposition: RunDisposition::Failed | RunDisposition::Cancelled,
            ..
        }
    )
}

fn active_run(view: &mut SessionView, id: RunId) -> Result<&mut RunView, ReplayError> {
    let run = current_run(view, id)?;
    if run.status != RunStatus::Active {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(run)
}

fn definition_registry(
    run: &RunView,
) -> Result<
    std::collections::BTreeMap<String, crate::tools::definitions::AgentDefinition>,
    ReplayError,
> {
    let Some(pin) = run
        .config
        .as_ref()
        .and_then(|config| config.agent_definitions.as_ref())
    else {
        return Ok(Default::default());
    };
    let outputs: Vec<_> = run
        .tools
        .iter()
        .filter(|tool| {
            matches!(
                tool.intent.call,
                crate::tools::ToolCall::ParseAgentDefinition { .. }
            )
        })
        .map(|tool| {
            tool.observation
                .as_ref()
                .filter(|observation| {
                    observation.disposition == crate::tools::ToolDisposition::Succeeded
                        && observation.attestation.is_some()
                })
                .map(|observation| observation.output.as_str())
                .ok_or(ReplayError::InvalidTransition)
        })
        .collect::<Result<_, _>>()?;
    if outputs.len() != pin.files.len() {
        return Err(ReplayError::InvalidTransition);
    }
    let registry = crate::tools::definitions::registry(pin, outputs.into_iter())
        .map_err(|_| ReplayError::InvalidTransition)?;
    let config = run.config.as_ref().ok_or(ReplayError::InvalidTransition)?;
    let child_model = config
        .providers
        .as_ref()
        .map_or(config.model.as_str(), |routes| {
            routes.child[0].model.as_str()
        });
    if registry.values().any(|definition| {
        definition
            .model
            .as_ref()
            .is_some_and(|model| model != child_model)
    }) {
        return Err(ReplayError::InvalidTransition);
    }
    Ok(registry)
}

fn definition_mode(
    run: &RunView,
    actor: AgentRunId,
) -> Result<Option<crate::tools::permissions::PermissionMode>, ReplayError> {
    let Some(name) = run
        .agents
        .iter()
        .find(|agent| agent.id == actor)
        .and_then(|agent| agent.definition.as_ref())
    else {
        return Ok(None);
    };
    let registry = definition_registry(run)?;
    Ok(registry
        .get(name)
        .ok_or(ReplayError::InvalidTransition)?
        .permission_mode)
}
